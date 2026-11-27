use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::audio::FinishMode;
#[allow(unused_imports)]
pub use crate::audio::text_prep::{
    clean_tts_char, has_speakable_char, split_tts_sentences, StreamingSentenceSplitter,
};
use crate::config::{OpenAiTtsConfig, TtsConfig, TtsProvider};
use crate::device::Device;

/// 播放本地音频文件的默认命令。
///
/// 三条都是真机实测的结论（OH2P 1.62.2，见 `docs/device-scripts.md`）：
/// - `miplayer -f <本地文件>` 可用（日志 `event(Playing)` → `EndReached`）；
/// - `miplayer` **播完不会自己退出**，所以下次播放前必须停掉旧的本地 TTS `miplayer`；
/// - 后台起播（`&`），否则一条命令就吃掉整个超时预算。
///
/// 通过 `/proc/[0-9]*/cmdline` 精准过滤只杀非 `http(s)://` 的本地文件 `miplayer`，
/// 避免 `killall miplayer` 误杀暂停中的音乐流进程。
pub const DEFAULT_PLAYBACK_COMMAND: &str =
    "for p in /proc/[0-9]*; do [ -r \"$p/cmdline\" ] || continue; cmd=$(tr '\\0' ' ' <\"$p/cmdline\" 2>/dev/null); case \"$cmd\" in *miplayer*) case \"$cmd\" in *http://*|*https://*) ;; *) kill -9 \"${p#/proc/}\" 2>/dev/null || true ;; esac ;; esac; done; miplayer -f {path} >/tmp/xiaoai-tts-player.log 2>&1 &";

/// 外部播放器的日志。`DEFAULT_PLAYBACK_COMMAND` 每播一次用 `>` 截断重写，所以
/// 「里面出现 `EndReached`」就等于「这一句确实放完了」——比按时长估算可靠。
///
/// 两处字面必须一起改：只改模板不改这里，静音窗就永远只能等兜底上限（回答完
/// 十几秒不理人）；只改这里不改模板，`EndReached` 永远不出现，同样退化成上限。
/// `the_default_playback_command_still_writes_the_player_log` 钉住这条。
pub const PLAYER_LOG_PATH: &str = "/tmp/xiaoai-tts-player.log";

/// 轮询播放器日志的间隔（`sleep 0.2` 在 OH2P 的 busybox 上实测可用）。
const PLAYER_POLL: Duration = Duration::from_millis(200);

/// 静音窗的封顶：静音再久也不该超过这个数（没人想答完之后三分钟不理人）。
const MAX_QUIET_WINDOW: Duration = Duration::from_secs(180);

/// 拿不到音频时长（mp3 / 未知格式）时按这个估。
const FALLBACK_PLAY_DURATION: Duration = Duration::from_secs(30);

/// 播放会话令牌：**打断的判据不是「杀掉任务」，而是「令牌变了」**。
///
/// 当前实现里唤醒打断靠 `task.abort()`，但 `tts_play.sh` / `miplayer` 是**独立进程**，
/// abort 掉 future 不等于杀掉它们——所以「说了闭嘴还多播一两句」。
/// 令牌解决的是另一个方向：任何**新**会话（唤醒、`/stop`、下一轮）一开始就 `begin()`
/// 让旧令牌失效，正在进行的合成/播放每完成一步先查一次，失效就闭嘴。
///
/// 两条必须一起用：令牌负责「不要再开始新的播放」，`abort_current_output()`
/// 负责「把已经在响的那句掐掉」。只有令牌，当前这句还会播完；只有 kill，
/// 已排队但还没起播的句子仍会漏出来。
#[derive(Debug, Default)]
pub struct PlaybackGate {
    token: AtomicU64,
    quiet: Mutex<QuietWindow>,
    changed: Notify,
}

/// 采集静音窗：外部播放期间话筒仍在送 PCM，但这段音频里混着答案本身。
///
/// 判据是**时间**而不是「播放器进程还在不在」（`miplayer` 播完不退出），
/// 所以窗口有一个截止时间；`generation` 让「结束播放的信号」只对开窗的那次
/// 播放生效——否则第一次播放的收尾会把第二次播放的窗口提前关掉。
#[derive(Debug, Default)]
struct QuietWindow {
    /// `None` = 不静音。过了这个时刻自动解除，不需要任何人记得来关。
    until: Option<Instant>,
    /// 每次重新开窗 +1；收尾信号必须带上自己的 generation。
    generation: u64,
}

impl PlaybackGate {
    pub fn current(&self) -> u64 {
        self.token.load(Ordering::Relaxed)
    }

    /// 让所有在飞的播放会话失效，返回新令牌。
    ///
    /// 同时**清掉静音窗**：新会话意味着旧的那句声音已经作废（唤醒打断会
    /// `killall` 掉播放器），继续静音会让用户喊完唤醒词后说的话被丢掉。
    pub fn begin(&self) -> u64 {
        self.clear_quiet();
        let next = self.token.fetch_add(1, Ordering::Relaxed) + 1;
        self.changed.notify_waiters();
        next
    }

    /// 等待当前令牌失效（被唤醒/`/stop` 打断）。先注册通知再判状态，避免竞态丢失唤醒。
    pub async fn wait_until_inactive(&self, token: u64) {
        loop {
            let notified = self.changed.notified();
            if !self.is_active(token) {
                return;
            }
            notified.await;
        }
    }

    /// 抓到的令牌还是当前的吗。
    pub fn is_active(&self, token: u64) -> bool {
        self.token.load(Ordering::Relaxed) == token
    }

    /// 开一段静音窗，返回这一窗的 generation（收尾时凭它认领）。
    ///
    /// 截止时间可以给得很宽松（当作兜底上限）：真正的收尾靠播放器的结束信号
    /// 调 `settle_quiet_window` 收紧；信号没来就按 `window` 自己过期。
    pub fn mute_mic_for(&self, window: Duration) -> u64 {
        let mut quiet = self.quiet.lock().expect("playback gate mutex poisoned");
        quiet.generation += 1;
        quiet.until = Some(Instant::now() + window);
        quiet.generation
    }

    /// 播放确实结束了：把窗口收紧到尾巴。
    ///
    /// generation 对不上说明已经开了新的一窗（用户又问了下一句），直接忽略——
    /// 不认领的话会把新窗口提前关掉，回声又漏进来。
    pub fn settle_quiet_window(&self, generation: u64, tail: Duration) {
        let mut quiet = self.quiet.lock().expect("playback gate mutex poisoned");
        if quiet.generation != generation || quiet.until.is_none() {
            return;
        }
        quiet.until = Some(Instant::now() + tail);
    }

    /// 这一窗还是 `generation` 开的、并且还没结束吗。
    ///
    /// 「播放器进程没了」有两种相反的处置，判据只能是这一个：唤醒打断走 `begin()`，
    /// 那里已经把 generation 推进并把窗口清掉（麦克风是放开的，不能再动它）；
    /// 而 OOM、手动 `killall`、起音乐时 `play_url_command` 的 `killall` 都不碰窗口，
    /// 那时窗口仍是我们的，说明真的没在播了，该收到尾巴而不是静到兜底上限。
    pub fn owns(&self, generation: u64) -> bool {
        let quiet = self.quiet.lock().expect("playback gate mutex poisoned");
        quiet.generation == generation && quiet.until.is_some()
    }

    /// 现在能不能把音频喂给 VAD。
    pub fn mic_muted(&self) -> bool {
        self.mic_quiet_remaining().is_some()
    }

    /// 静音窗还剩多久；`None` = 不在窗口里。
    pub fn mic_quiet_remaining(&self) -> Option<Duration> {
        let quiet = self.quiet.lock().expect("playback gate mutex poisoned");
        let until = quiet.until?;
        let now = Instant::now();
        if until > now {
            Some(until - now)
        } else {
            None
        }
    }

    fn clear_quiet(&self) {
        let mut quiet = self.quiet.lock().expect("playback gate mutex poisoned");
        quiet.until = None;
        quiet.generation += 1;
    }
}

/// 从 WAV 字节里算播放时长，用来给静音窗一个兜底上限。
///
/// **不信头里写的长度**：实测后端会回流式写法（`data` 长度是 `0x7FFFFFFF`），
/// 拿它算时长会得到几小时的窗口，于是答完一整天都不理人。这里逐个 chunk 走，
/// `data` 长度取「声明值」与「文件里真实剩余字节」的较小者。
///
/// 非 wav（mp3 / opus / flac / 未知）返回 `None`——那些格式的时长要解码，不值得。
pub fn wav_play_duration(bytes: &[u8]) -> Option<Duration> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut byte_rate: Option<u32> = None;
    let mut data_len: Option<usize> = None;
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let declared = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        let available = bytes.len() - body;
        match id {
            b"fmt " => {
                // 采样率 4 字节 @4、字节率 4 字节 @8、块对齐 2 字节 @12。
                if declared < 16 || available < 16 {
                    return None;
                }
                let rate = u32::from_le_bytes(bytes[body + 8..body + 12].try_into().ok()?);
                if rate > 0 {
                    byte_rate = Some(rate);
                }
            }
            b"data" => {
                // 声明值是 0x7FFFFFFF / 0xFFFF_FFFF 时取真实剩余字节。
                data_len = Some(declared.min(available));
            }
            _ => {}
        }
        if data_len.is_some() && byte_rate.is_some() {
            break;
        }
        // chunk 按偶数字节对齐。
        let advance = declared + (declared & 1);
        if advance > available {
            break;
        }
        offset = body + advance;
    }
    let byte_rate = byte_rate.filter(|rate| *rate > 0)?;
    let data_len = data_len?;
    if data_len == 0 {
        return None;
    }
    Some(Duration::from_secs_f64(data_len as f64 / byte_rate as f64))
}

/// 一次外部播放的静音计划。
///
/// 抽成纯函数：窗口时长、尾巴、以及「等播放器结束」的脚本都在这里算死，好给它
/// 写单测（真机上没法构造各种音频时长与播放器行为）。
#[derive(Debug, Clone, PartialEq)]
struct QuietPlan {
    /// 开窗时用的兜底上限（信号没来就按它自己过期）。
    window: Duration,
    /// 播放结束后还要静多久。
    tail: Duration,
    /// 设备上等 `EndReached` 的命令；`None` = 自定义播放命令，看不到播放器日志。
    wait_script: Option<String>,
    /// 等结束的命令自己的超时。
    wait_timeout: Duration,
}

fn quiet_plan(config: &TtsConfig, bytes: &[u8], default_player: bool) -> Option<QuietPlan> {
    if !config.suppress_capture_while_playing {
        return None;
    }
    let tail = Duration::from_millis(config.playback_tail_ms);
    let grace = Duration::from_millis(config.playback_startup_grace_ms);
    let expected = wav_play_duration(bytes).unwrap_or(FALLBACK_PLAY_DURATION);
    if !default_player {
        // 自定义播放命令：看不到播放器日志，只能按时长估。宽限 + 音频 + 尾巴，
        // 估错的方向是「多静一会儿」，不会漏回声（漏回声就是设备自问自答）。
        return Some(QuietPlan {
            window: (grace + expected + tail).min(MAX_QUIET_WINDOW),
            tail,
            wait_script: None,
            wait_timeout: Duration::ZERO,
        });
    }
    // 默认播放器：开窗给足（纯兜底），真正的收尾靠 `EndReached` 收紧。
    let window = (grace + expected * 2 + Duration::from_secs(10)).min(MAX_QUIET_WINDOW);
    let iterations = (window.as_millis() / PLAYER_POLL.as_millis()).max(1);
    Some(QuietPlan {
        window,
        tail,
        // 先 sleep 再查：播放器刚被后台起来，立刻探测可能还看不到它。
        // exit 2 = 播放器已经不在（唤醒打断会 killall），不必空转到上限。
        //
        // 用 `pidof` 而**不是** `pgrep`：设备是 busybox，没有 pgrep
        // （docs/flash-troubleshooting.md 第五节）。`pidof` 也不是每台固件都编了，
        // 所以外面再套一层 `command -v` 守卫——守卫掉了就退化成「等满上限 + 一条
        // warn」。绝不能让「探测手段不可用」等价于「播放器已经没了」：那会把每一次
        // 正常播放都判成被打断，于是窗口不收，麦克风静到兜底上限（几十秒）。
        wait_script: Some(format!(
            "i=0; while [ $i -lt {iterations} ]; do sleep 0.2; grep -q EndReached {PLAYER_LOG_PATH} 2>/dev/null && exit 0; if command -v pidof >/dev/null 2>&1; then pidof miplayer >/dev/null 2>&1 || exit 2; fi; i=$((i+1)); done; exit 1"
        )),
        wait_timeout: window + Duration::from_secs(5),
    })
}

/// 等结束脚本的三种结局。
///
/// 分开是因为处置不同：正常放完要收窗；**被唤醒打断**时播放器已被 `killall`，
/// 窗口早在 `begin()` 里清掉了，这里只该打 debug——打成 warn 会让用户每喊一声
/// 打断都在设备日志里留下一条看起来像故障的记录（真机实测就是这样，一次打断
/// 换一条 `没等到 EndReached` 的 WARN）。其余「播放器没了」的原因见
/// `watch_playback_end` 的 `Interrupted` 分支。
#[derive(Debug, PartialEq, Eq)]
enum PlaybackEnd {
    /// 看到 `EndReached`：这一句放完了。
    Finished,
    /// 播放器进程已经不在了：被打断（或自己退了）。
    Interrupted,
    /// 空转到上限仍没结果。
    Unclear(i32),
}

fn classify_playback_end(exit_code: i32) -> PlaybackEnd {
    match exit_code {
        0 => PlaybackEnd::Finished,
        2 => PlaybackEnd::Interrupted,
        other => PlaybackEnd::Unclear(other),
    }
}

/// 在设备上等播放器把这一句放完，然后让静音窗收紧到尾巴。
///
/// 失败（`EndReached` 没出现 / 命令跑不了）不关窗：窗口仍然按开窗时的上限自己
/// 过期。宁可多静一会儿也不要在还在出声的时候把麦克风放回来。
async fn watch_playback_end(
    device: Device,
    gate: Arc<PlaybackGate>,
    generation: u64,
    wait_script: String,
    wait_timeout: Duration,
    tail: Duration,
) {
    let exit_code = match device.run_raw(&wait_script, wait_timeout).await {
        Ok(result) => result.exit_code,
        Err(err) => {
            warn!(error = %err, "等播放器结束的命令失败：静音窗按兜底上限自己过期");
            return;
        }
    };
    match classify_playback_end(exit_code) {
        PlaybackEnd::Finished => {
            gate.settle_quiet_window(generation, tail);
            // 提到 info 而不是 debug：这一行是**唯一**能证明「窗口是被 EndReached
            // 收紧、而不是跑到兜底上限」的证据。每轮一条，成本可以忽略。
            info!(
                tail_ms = tail.as_millis() as u64,
                settled_by = "EndReached",
                "静音窗收紧到尾巴"
            );
        }
        PlaybackEnd::Interrupted => {
            // 播放器没了**不等于**被打断：OOM、手动 killall、起音乐时
            // `play_url_command` 里的 killall 都会走到这里，而那时窗口还在本次
            // generation 手里。那种情况是真的没在播了，收到尾巴就好；只有唤醒
            // 打断（`begin()` 已推进 generation 并清窗）才什么都不做。
            let owned = gate.owns(generation);
            if owned {
                gate.settle_quiet_window(generation, tail);
            }
            debug!(
                settled_to_tail = owned,
                "播放器进程已不在（打断或自行退出），不再等 EndReached"
            );
        }
        PlaybackEnd::Unclear(code) => warn!(
            exit = code,
            settled_by = "cap",
            "等播放器结束的脚本没等到 EndReached：静音窗按兜底上限自己过期"
        ),
    }
}

/// 连续多少次 TTS 失败后升级成 `error`。
///
/// 取 3：一次网络抖动不该刷红日志，但「每轮一条 warn」的长期故障必须能一眼看出来。
pub const TTS_FAILURES_BEFORE_ALARM: u32 = 3;

/// TTS 健康度：只做计数与日志分档，**不接让位机制**。
///
/// 让位是为了处理「能唤醒但答不上」的空转（大脑侧）；TTS 是输出侧，挂了不该把整个
/// agent 下线。但摆到「永远只 warn」那一端又会让运维分不出「刚抖了一下」和
/// 「已经坏了三天」——所以连续失败到阈值时升成 `error`。
#[derive(Debug, Default)]
pub struct TtsHealth {
    consecutive_failures: AtomicU32,
}

impl TtsHealth {
    pub fn on_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// 记一次失败并打日志，返回当前连续失败次数。
    pub fn on_failure(&self, err: &anyhow::Error) -> u32 {
        let streak = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= TTS_FAILURES_BEFORE_ALARM {
            error!(
                consecutive = streak,
                error = %err,
                "语音合成连续失败：检查 tts 后端/配置，或把 provider 改回 native（不影响对话，只是没声音）"
            );
        } else {
            warn!(
                consecutive = streak,
                error = %err,
                "回答的语音合成失败：不计入「连续失败让位」，本轮无声音但流程继续"
            );
        }
        streak
    }
}

/// 回答的音频出口。
///
/// `provider: native`（默认）与直调 `device.speak` 完全等价——同一个模板、同样的
/// 空文本短路、同样的非零退出码变 Err。外部 provider 走「拿音频字节 → 落
/// `/tmp` → 播放器命令」这条链路，任何一步失败都按 `fallback_to_native` 决定
/// 是回退系统 TTS 还是报错。
#[derive(Clone)]
pub struct TtsRouter {
    device: Device,
    gate: Arc<PlaybackGate>,
    client: reqwest::Client,
}

struct AbortOnDrop<T>(Option<JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self(Some(handle))
    }

    fn detach(mut self) {
        self.0.take();
    }

    async fn wait_or_cancel(mut self, gate: &PlaybackGate, token: u64) -> bool {
        let Some(mut handle) = self.0.take() else {
            return gate.is_active(token);
        };
        tokio::select! {
            _ = &mut handle => gate.is_active(token),
            _ = gate.wait_until_inactive(token) => {
                handle.abort();
                false
            }
        }
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

enum PlaybackWait {
    Task(JoinHandle<()>),
    Duration(Duration),
    Skipped,
}

enum PreparedChunk {
    Audio { text: String, bytes: Vec<u8> },
    FallbackNative(String),
    Error(anyhow::Error),
}

impl TtsRouter {
    pub fn new(device: Device, gate: Arc<PlaybackGate>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .pool_idle_timeout(Duration::from_secs(10))
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            device,
            gate,
            client,
        }
    }

    /// 念一句话。
    ///
    /// 注意错误提示（`speak_service_error`）不走这里，是有意为之：那条路径要念的
    /// 是「服务出问题了」，此时外部 TTS 大概率同样不可用，只有设备自带的
    /// `tts_play.sh` 还靠得住。
    #[cfg(test)]
    pub async fn speak(&self, config: &TtsConfig, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        // 抓一次令牌：本轮从合成到播放用的都是它。中途有人喊唤醒（或 `/stop`）就会
        // begin() 一次，这里在真正出声前发现自己过期，于是闭嘴而不是补播一句。
        let token = self.gate.current();
        match config.provider {
            TtsProvider::Native => {
                // native 没有「几百毫秒合成」的窗口，但 `tts_play.sh` 起播本身也有延迟，
                // 且这里必须和外部路径共用同一道闸——结构上不能留例外。
                if !self.may_play(token) {
                    return Ok(());
                }
                self.device.speak(text).await
            }
            provider => self.speak_remote(config, provider, text, token).await,
        }
    }

    /// 流式分句播报流水线（默认 `FinishMode::DetachForQuietWindow` 保持既有静音窗行为）。
    #[allow(dead_code)] // 仅测试直接调用；运行时走 spawn_with_hooks / speak_stream_with_mode
    pub async fn speak_stream<F>(
        &self,
        config: &TtsConfig,
        chunks: mpsc::Receiver<String>,
        health: &TtsHealth,
        on_first_speak: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut() + Send,
    {
        self.speak_stream_with_mode(
            config,
            chunks,
            health,
            FinishMode::DetachForQuietWindow,
            on_first_speak,
        )
        .await
    }

    /// 流式分句播报流水线：
    /// - `Native`：边切句边同步顺序调用 `tts_play.sh`；
    /// - `OpenAi`：双缓冲预合成（当前句播放时后台并发抓取下一句音频），句间等待
    ///   `EndReached` 避免误掐前一句；末句按 `finish_mode` 决定是后台守望静音窗
    ///   （`DetachForQuietWindow`）还是等播完再返回（`WaitForEnd`）。
    /// - 任一阶段令牌失效（唤醒/`/stop` 打断）立即停播并安全退出。
    pub async fn speak_stream_with_mode<F>(
        &self,
        config: &TtsConfig,
        mut chunks: mpsc::Receiver<String>,
        health: &TtsHealth,
        finish_mode: FinishMode,
        mut on_first_speak: F,
    ) -> anyhow::Result<()>
    where
        F: FnMut() + Send,
    {
        let token = self.gate.current();
        let mut first_spoken = false;
        if config.provider == TtsProvider::Native {
            while let Some(chunk) = chunks.recv().await {
                if chunk.trim().is_empty() {
                    continue;
                }
                if !self.may_play(token) {
                    return Ok(());
                }
                if !first_spoken {
                    first_spoken = true;
                    on_first_speak();
                }
                match self.device.speak(&chunk).await {
                    Ok(()) => health.on_success(),
                    Err(err) => {
                        health.on_failure(&err);
                    }
                }
            }
            return Ok(());
        }

        let (prepared_tx, mut prepared_rx) = mpsc::channel::<PreparedChunk>(2);
        let router = self.clone();
        let cfg = config.clone();
        let prefetch_guard = AbortOnDrop::new(tokio::spawn(async move {
            while let Some(chunk) = chunks.recv().await {
                if chunk.trim().is_empty() {
                    continue;
                }
                if !router.gate.is_active(token) {
                    break;
                }
                let prepared = match cfg.provider {
                    TtsProvider::OpenAi => {
                        if cfg.openai.api_key.trim().is_empty() {
                            if cfg.fallback_to_native {
                                warn!("tts.openai.api_key 为空，回退系统 TTS");
                                PreparedChunk::FallbackNative(chunk)
                            } else {
                                PreparedChunk::Error(anyhow!("tts.openai.api_key 为空"))
                            }
                        } else {
                            match router.fetch_openai_speech(&cfg.openai, &chunk).await {
                                Ok(bytes) => PreparedChunk::Audio { text: chunk, bytes },
                                Err(err) if cfg.fallback_to_native => {
                                    warn!(error = %err, "外部 TTS 预合成失败，回退系统 TTS");
                                    PreparedChunk::FallbackNative(chunk)
                                }
                                Err(err) => PreparedChunk::Error(err),
                            }
                        }
                    }
                    other => {
                        let err = anyhow!("tts provider {other:?} 尚未接入");
                        if cfg.fallback_to_native {
                            warn!(?other, error = %err, "外部 TTS 不可用，回退系统 TTS");
                            PreparedChunk::FallbackNative(chunk)
                        } else {
                            PreparedChunk::Error(err)
                        }
                    }
                };
                if prepared_tx.send(prepared).await.is_err() {
                    break;
                }
            }
        }));

        let mut prev_wait: Option<AbortOnDrop<()>> = None;
        while let Some(item) = prepared_rx.recv().await {
            if let Some(wait) = prev_wait.take() {
                if !wait.wait_or_cancel(&self.gate, token).await {
                    return Ok(());
                }
            }
            if !self.may_play(token) {
                return Ok(());
            }
            match item {
                PreparedChunk::Audio { text, bytes } => {
                    if !first_spoken {
                        first_spoken = true;
                        on_first_speak();
                    }
                    match self.play_openai_bytes(config, &bytes, token).await {
                        Ok(PlaybackWait::Task(handle)) => {
                            health.on_success();
                            prev_wait = Some(AbortOnDrop::new(handle));
                        }
                        Ok(PlaybackWait::Duration(dur)) => {
                            health.on_success();
                            prev_wait = Some(AbortOnDrop::new(tokio::spawn(tokio::time::sleep(dur))));
                        }
                        Ok(PlaybackWait::Skipped) => return Ok(()),
                        Err(err) => {
                            if config.fallback_to_native {
                                warn!(error = %err, "外部 TTS 播放失败，回退系统 TTS");
                                if !self.may_play(token) {
                                    return Ok(());
                                }
                                match self.device.speak(&text).await {
                                    Ok(()) => health.on_success(),
                                    Err(fb_err) => {
                                        health.on_failure(&fb_err);
                                    }
                                }
                            } else {
                                health.on_failure(&err);
                            }
                        }
                    }
                }
                PreparedChunk::FallbackNative(text) => {
                    if !first_spoken {
                        first_spoken = true;
                        on_first_speak();
                    }
                    match self.device.speak(&text).await {
                        Ok(()) => health.on_success(),
                        Err(err) => {
                            health.on_failure(&err);
                        }
                    }
                }
                PreparedChunk::Error(err) => {
                    health.on_failure(&err);
                }
            }
        }

        if let Some(last_wait) = prev_wait.take() {
            match finish_mode {
                FinishMode::DetachForQuietWindow => last_wait.detach(),
                FinishMode::WaitForEnd => {
                    let _ = last_wait.wait_or_cancel(&self.gate, token).await;
                }
            }
        }
        drop(prefetch_guard);
        Ok(())
    }

    /// 起播前的最后一道闸：令牌过期就闭嘴。返回 `true` = 可以出声。
    ///
    /// 注意语义是「该不该播」，不是「有没有失败」——被打断不是失败，调用方返回
    /// `Ok(())`。**所有出声路径都必须过这里**（native、外部合成、失败回退、将来的
    /// `/play/*`），否则会出现「有的路径查、有的不查」的口子：复核指出的正是
    /// 「外部 TTS 超时 → 回退 native」这条最容易被用户打断的路。
    fn may_play(&self, token: u64) -> bool {
        if self.gate.is_active(token) {
            return true;
        }
        debug!("本轮回答在打断后已过期，跳过起播");
        false
    }

    #[cfg(test)]
    async fn speak_remote(
        &self,
        config: &TtsConfig,
        provider: TtsProvider,
        text: &str,
        token: u64,
    ) -> anyhow::Result<()> {
        let attempt = match provider {
            TtsProvider::OpenAi => self.speak_openai(config, text, token).await,
            TtsProvider::Doubao => Err(anyhow!("tts provider Doubao 尚未接入")),
            TtsProvider::Native => unreachable!("native 不会走到这里"),
        };
        match attempt {
            Ok(()) => Ok(()),
            Err(err) if config.fallback_to_native => {
                // 音色会变，但「能说话」比「音色统一」重要。
                warn!(?provider, error = %err, "外部 TTS 失败，回退系统 TTS");
                // 回退也是一次起播，同样要过闸：外部超时 + 用户已经不耐烦地喊了第二次，
                // 是最容易撞上的组合；不过闸就会在失败之后补一句迟到的话。
                if !self.may_play(token) {
                    return Ok(());
                }
                self.device.speak(text).await
            }
            Err(err) => Err(err.context(format!(
                "tts provider {provider:?} 失败，且 fallback_to_native=false"
            ))),
        }
    }

    #[cfg(test)]
    async fn speak_openai(&self, config: &TtsConfig, text: &str, token: u64) -> anyhow::Result<()> {
        let tts = &config.openai;
        if tts.api_key.trim().is_empty() {
            bail!("tts.openai.api_key 为空");
        }
        let bytes = self.fetch_openai_speech(tts, text).await?;
        let _ = self.play_openai_bytes(config, &bytes, token).await?;
        Ok(())
    }

    async fn play_openai_bytes(
        &self,
        config: &TtsConfig,
        bytes: &[u8],
        token: u64,
    ) -> anyhow::Result<PlaybackWait> {
        let tts = &config.openai;
        prune_old_speech_files(&tts.dir());
        let path = speech_output_path(&tts.dir(), &tts.response_format);

        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("写音频文件失败: {}", path.display()))?;
        debug!(bytes = bytes.len(), path = %path.display(), "TTS 音频已落盘");

        // 闸放在落盘**之后**：合成可能要几百毫秒到几秒，期间用户完全可能已经喊了
        // 唤醒词。放在 fetch 之后、write 之前也挡得住，但那样会白写一次几十到几百 KB；
        // 放在这里，检查到起播之间只剩同步的模板渲染（TOCTOU 从毫秒级压到微秒级）。
        if !self.may_play(token) {
            return Ok(PlaybackWait::Skipped);
        }

        let template = config
            .playback_command
            .as_deref()
            .unwrap_or(DEFAULT_PLAYBACK_COMMAND);
        let script = render_playback_command(template, &path)?;
        let plan = quiet_plan(config, bytes, config.playback_command.is_none());
        // 开窗必须排在起播**之前**：出声和开麦在两条独立的任务线上，先起播再
        // 静音的那条缝隙里，回声已经能进 VAD 了。
        let generation = plan
            .as_ref()
            .map(|plan| self.gate.mute_mic_for(plan.window));
        let result = match self.device.run_raw(&script, Duration::from_secs(10)).await {
            Ok(result) => result,
            // fork 失败 / 超时这类根本没拿到退出码的情况同样是「没播」：不把窗口
            // 收到尾巴的话，这里会以「兜底上限」静音十几秒，用户以为音箱坏了。
            Err(err) => {
                if let (Some(plan), Some(generation)) = (&plan, generation) {
                    self.gate.settle_quiet_window(generation, plan.tail);
                }
                return Err(err.context("播放命令执行失败"));
            }
        };
        if result.exit_code != 0 {
            // 播放失败时不能按上限继续静音（那是十几秒不理人）：收到尾巴就够，
            // 真有一截声音漏出来也有余量盖住。
            if let (Some(plan), Some(generation)) = (&plan, generation) {
                self.gate.settle_quiet_window(generation, plan.tail);
            }
            bail!(
                "播放命令失败 exit={} stderr={}",
                result.exit_code,
                result.stderr.trim()
            );
        }
        if let (Some(plan), Some(generation)) = (plan, generation) {
            if let Some(wait_script) = plan.wait_script {
                let handle = tokio::spawn(watch_playback_end(
                    self.device.clone(),
                    self.gate.clone(),
                    generation,
                    wait_script,
                    plan.wait_timeout,
                    plan.tail,
                ));
                return Ok(PlaybackWait::Task(handle));
            }
            return Ok(PlaybackWait::Duration(plan.window));
        }
        let fallback = wav_play_duration(bytes).unwrap_or(FALLBACK_PLAY_DURATION);
        Ok(PlaybackWait::Duration(fallback))
    }

    async fn fetch_openai_speech(
        &self,
        tts: &OpenAiTtsConfig,
        text: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let mut body = json!({
            "model": tts.model,
            "input": text,
            "voice": tts.voice,
            "response_format": tts.response_format,
        });
        if let Some(speed) = tts.speed {
            body["speed"] = json!(speed);
        }
        if let Some(sample_rate) = tts.sample_rate {
            body["sample_rate"] = json!(sample_rate);
        }
        if let Some(instructions) = tts.instructions.as_deref() {
            if !instructions.trim().is_empty() {
                body["instructions"] = json!(instructions);
            }
        }

        let request = self
            .client
            .post(tts.endpoint())
            .bearer_auth(&tts.api_key)
            .json(&body);
        // timeout_s 是「整体上限」，不是「等到响应头为止」：reqwest 的 send()
        // 在收到响应头就返回，body 是 bytes() 才读的。一个「200 之后几乎不吐
        // 数据」的后端会让这一轮 turn 永久卡住——LED 停在说话状态、空闲超时也
        // 要等这一轮结束才生效。所以两层都用同一个 limit 兜住。
        let limit = Duration::from_secs_f64(tts.timeout_s.max(1.0));
        let response = tokio::time::timeout(limit, request.send())
            .await
            .context("TTS 请求超时")??;

        let status = response.status();
        let bytes = tokio::time::timeout(limit, response.bytes())
            .await
            .context("TTS 响应体读取超时")?
            .context("读取 TTS 响应体失败")?;
        if !status.is_success() {
            // 错误体可能是 JSON、也可能是中转的 HTML 页，取前 200 字节止血。
            bail!(
                "TTS 接口返回 {status}: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
            );
        }
        if bytes.is_empty() {
            bail!("TTS 接口返回空音频");
        }
        Ok(bytes.to_vec())
    }
}

pub fn strip_punctuation_and_space(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '。' | '，' | '！' | '？' | '；' | '：' | '、' | '.' | ',' | '!' | '?' | ';' | ':' | '…'
                )
        })
        .collect()
}

pub fn is_noise_or_filler(text: &str) -> bool {
    let cleaned = strip_punctuation_and_space(text);
    if cleaned.is_empty() {
        return true;
    }
    let count = cleaned.chars().count();
    if count == 1 {
        let ch = cleaned.chars().next().unwrap_or_default();
        return matches!(
            ch,
            '嗯' | '啊' | '哦' | '呃' | '唔' | '哎' | '哼' | '哈' | '喂' | '呐' | '呀' | '呵'
        );
    }
    matches!(
        cleaned.as_str(),
        "嗯嗯" | "啊啊" | "哦哦" | "呃呃" | "哈哈" | "哈哈哈" | "呵呵" | "哎呀" | "那个" | "就是"
    )
}

/// 识别用户是否只说了唤醒词或单纯确认在不在（没有附带实际问题）。
pub fn is_wake_word_only(text: &str) -> bool {
    let cleaned = strip_punctuation_and_space(text);
    matches!(
        cleaned.as_str(),
        "小爱同学" | "小爱" | "豆包豆包" | "豆包" | "在吗" | "你在吗" | "小爱在吗" | "豆包在吗"
    )
}

/// 剥离一句话开头的唤醒词前缀（支持「小爱同学 / 小爱 / 豆包豆包 / 豆包」连说）。
pub fn strip_wake_prefix(text: &str) -> String {
    let mut s = text.trim();
    loop {
        let trimmed = s.trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '。' | '，' | '！' | '？' | '；' | '：' | '、' | '.' | ',' | '!' | '?' | ';' | ':' | '…'
                )
        });
        if let Some(rest) = trimmed.strip_prefix("小爱同学") {
            s = rest;
        } else if let Some(rest) = trimmed.strip_prefix("豆包豆包") {
            s = rest;
        } else if let Some(rest) = trimmed.strip_prefix("小爱") {
            s = rest;
        } else if let Some(rest) = trimmed.strip_prefix("豆包") {
            s = rest;
        } else {
            return trimmed.to_string();
        }
    }
}

/// 用户主动退出会话的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAction {
    /// 「闭嘴 / 算了 / 不用了 / 退下」：用户希望音箱立刻安静，不再啰嗦播报告别语。
    Silent,
    /// 「再见 / 拜拜 / 退出对话」：礼貌告别后退出会话。
    Farewell,
}

/// 识别用户退出对话指令（仅精确匹配完整退出意图，绝不子串误伤「把客厅灯关闭 / 取消明天日程」）。
pub fn classify_exit_command(text: &str, custom_keywords: &[String]) -> Option<ExitAction> {
    let cleaned = strip_punctuation_and_space(text);
    let core = cleaned
        .trim_end_matches(|c| matches!(c, '吧' | '了' | '啊' | '呀' | '哦' | '呢'))
        .trim();
    if core.is_empty() {
        return None;
    }
    if matches!(
        core,
        "闭嘴" | "别说" | "安静" | "不用" | "算" | "没事" | "退下" | "别讲"
    ) || matches!(
        cleaned.as_str(),
        "算了" | "不用了" | "没事了" | "别说了" | "别讲了"
    ) {
        return Some(ExitAction::Silent);
    }
    if matches!(
        core,
        "再见" | "拜拜" | "退出" | "退出对话" | "结束对话" | "先这样" | "休息"
    ) {
        return Some(ExitAction::Farewell);
    }
    if custom_keywords.iter().any(|kw| {
        let kw_clean = strip_punctuation_and_space(kw);
        !kw_clean.is_empty() && (cleaned == kw_clean || core == kw_clean)
    }) {
        return Some(ExitAction::Farewell);
    }
    None
}

/// 音乐播放期间的本地快捷语音控制动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicVoiceControl {
    Stop,
    Pause,
    Resume,
    Next,
    Previous,
}

/// 识别音乐快捷控制指令（仅在 `music.has_current()` 为真时由上层拦截生效）。
pub fn classify_music_voice_control(text: &str) -> Option<MusicVoiceControl> {
    let cleaned = strip_punctuation_and_space(text);
    let core = cleaned
        .trim_end_matches(|c| matches!(c, '吧' | '了' | '啊' | '呀' | '哦' | '呢'))
        .trim();
    if core.is_empty() {
        return None;
    }
    if matches!(
        core,
        "停止"
            | "停"
            | "停下"
            | "别放"
            | "别唱"
            | "不要放"
            | "不听"
            | "停止播放"
            | "停止音乐"
            | "停止播放音乐"
            | "停止放歌"
            | "关掉音乐"
            | "关闭音乐"
            | "退出音乐"
    ) {
        return Some(MusicVoiceControl::Stop);
    }
    if (core.contains("停止")
        || core.contains("停下")
        || core.contains("关闭")
        || core.contains("关掉")
        || core.contains("别放"))
        && (core.contains("音乐") || core.contains("播放") || core.contains("歌"))
    {
        return Some(MusicVoiceControl::Stop);
    }
    if matches!(core, "暂停" | "暂停播放" | "暂停音乐" | "先暂停") {
        return Some(MusicVoiceControl::Pause);
    }
    if matches!(
        core,
        "继续" | "继续播放" | "继续放" | "继续放歌" | "恢复播放" | "接着放"
    ) {
        return Some(MusicVoiceControl::Resume);
    }
    if matches!(core, "下一首" | "下一曲" | "切歌" | "换一首" | "换首歌") {
        return Some(MusicVoiceControl::Next);
    }
    if matches!(core, "上一首" | "上一曲") {
        return Some(MusicVoiceControl::Previous);
    }
    None
}

/// 把 `{path}` 渲染成被 shell 引号化的路径。
///
/// 模板里没有 `{path}` 直接报错：写成 `miplayer -f`（忘了占位符）会变成
/// 「播放器空转、什么都没响」，比启动时炸掉难查得多。
pub fn render_playback_command(template: &str, path: &Path) -> anyhow::Result<String> {
    if !template.contains("{path}") {
        bail!("playback_command 缺少 {{path}} 占位符: {template}");
    }
    let displayed = path.to_string_lossy();
    let quoted = shell_words::quote(&displayed);
    Ok(template.replace("{path}", quoted.as_ref()))
}

fn extension_for(response_format: &str) -> &str {
    match response_format {
        "wav" => "wav",
        "mp3" => "mp3",
        "opus" => "opus",
        "aac" => "aac",
        "flac" => "flac",
        "pcm" => "pcm",
        // 未知格式也让它落盘，别在这里拦——真正的判据是播放器能不能放。
        // 结局是「播放失败 → 回退系统 TTS」，不是静默成功。
        _ => "bin",
    }
}

fn speech_output_path(dir: &Path, response_format: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "xiaoai-tts-{}-{}.{}",
        std::process::id(),
        stamp,
        extension_for(response_format)
    ))
}

/// 清掉过旧（> 600s）或超过数量上限（最多保留 5 个最新）的落盘音频。
///
/// `/tmp` 是 tmpfs（不写 NAND），但它是内存：LX06 内存有限，只按时间清理在高频对话时
/// 仍可能堆积大量音频，因此同时施加时间与文件数双上限。只删自己写的那种文件名，不碰别人的东西。
fn prune_old_speech_files(dir: &Path) {
    const MAX_AGE: Duration = Duration::from_secs(600);
    const MAX_FILES: usize = 5;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kept: Vec<(SystemTime, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !file_name.starts_with("xiaoai-tts-") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified.elapsed().map(|age| age > MAX_AGE).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        } else {
            kept.push((modified, file_name, entry.path()));
        }
    }
    if kept.len() > MAX_FILES {
        kept.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        for (_, _, path) in kept.into_iter().skip(MAX_FILES) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::DeviceConfig;
    use crate::shell::RecordingShell;

    fn router(shell: Arc<RecordingShell>) -> TtsRouter {
        TtsRouter::new(
            Device::with_runner(DeviceConfig::default(), shell),
            Arc::new(PlaybackGate::default()),
        )
    }

    /// CRLF 用 13/10 拼：源码里不出现转义序列，避免被工具链改写（踩过一次）。
    fn crlf() -> String {
        [13_u8, 10_u8].iter().map(|byte| *byte as char).collect()
    }

    /// 带独立令牌的 router：测试要自己控制「什么时候被打断」。
    fn router_with_gate(shell: Arc<RecordingShell>, gate: Arc<PlaybackGate>) -> TtsRouter {
        TtsRouter::new(Device::with_runner(DeviceConfig::default(), shell), gate)
    }

    /// 一个必定连接被拒的 OpenAI 兼容端点：不碰外网、失败得又快又确定。
    fn dead_endpoint() -> OpenAiTtsConfig {
        OpenAiTtsConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "sk-test".to_string(),
            timeout_s: 3.0,
            output_dir: Some(std::env::temp_dir()),
            ..OpenAiTtsConfig::default()
        }
    }

    /// 默认配置（native）必须走原来的 `tts_play.sh`，一条命令、不多不少。
    #[tokio::test]
    async fn native_provider_still_uses_the_device_tts_command() {
        let shell = Arc::new(RecordingShell::always_ok());
        router(shell.clone())
            .speak(&TtsConfig::default(), "你好")
            .await
            .unwrap();
        assert_eq!(shell.command_count(), 1);
        assert!(
            shell
                .last_script()
                .unwrap()
                .starts_with("/usr/sbin/tts_play.sh"),
            "{:?}",
            shell.last_script()
        );
    }

    /// 尚未接入的 provider → 回退系统 TTS，且**确实念了**（不是静默跳过）。
    #[tokio::test]
    async fn unimplemented_provider_falls_back_to_native() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::Doubao,
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "回退测试")
            .await
            .unwrap();
        assert_eq!(
            shell.command_count(),
            1,
            "回退时也要真的念出来，否则表现为「答了但没声音」"
        );
        assert!(shell.last_script().unwrap().contains("回退测试"));
    }

    #[tokio::test]
    async fn unimplemented_provider_without_fallback_is_an_error() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::Doubao,
            fallback_to_native: false,
            ..TtsConfig::default()
        };
        let err = router(shell.clone())
            .speak(&config, "不该被念出来")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback_to_native=false"), "{err}");
        assert_eq!(shell.command_count(), 0, "报错时不该动设备");
    }

    /// 没配 key 就没必要发请求：直接回退。这类配置错误重试多少次都没用。
    #[tokio::test]
    async fn openai_without_api_key_falls_back_without_a_request() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "没配 key")
            .await
            .unwrap();
        assert_eq!(shell.count_matching("/usr/sbin/tts_play.sh"), 1);
        assert_eq!(shell.count_matching("miplayer"), 0, "不该尝试播放");
    }

    /// 后端不可达 → 回退系统 TTS（这是「音箱必须能出声」的底线）。
    #[tokio::test]
    async fn unreachable_tts_backend_falls_back_to_native() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: dead_endpoint(),
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "后端挂了")
            .await
            .unwrap();
        assert_eq!(shell.count_matching("/usr/sbin/tts_play.sh"), 1);
    }

    /// 关掉回退 = 明确要求「宁可不说话」，必须报错且不动设备。
    #[tokio::test]
    async fn unreachable_tts_backend_without_fallback_is_an_error() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            fallback_to_native: false,
            openai: dead_endpoint(),
            ..TtsConfig::default()
        };
        let err = router(shell.clone())
            .speak(&config, "不该被念出来")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback_to_native=false"), "{err}");
        assert_eq!(shell.command_count(), 0);
    }

    /// 令牌语义：`begin()` 之后旧令牌立刻失效，新令牌有效。
    #[test]
    fn beginning_a_session_invalidates_the_old_token() {
        let gate = PlaybackGate::default();
        let first = gate.current();
        assert!(gate.is_active(first));

        let second = gate.begin();
        assert!(!gate.is_active(first), "begin() 之后旧令牌必须失效");
        assert!(gate.is_active(second));
        assert_eq!(gate.begin(), second + 1, "令牌必须单调递增");
    }

    /// 静音窗必须自己过期：没有「谁记得来关」这一步，否则一次丢掉的结束信号
    /// 会让音箱答完之后永远不理人。
    #[tokio::test]
    async fn the_quiet_window_expires_on_its_own() {
        let gate = PlaybackGate::default();
        assert!(!gate.mic_muted(), "默认不静音");

        gate.mute_mic_for(Duration::from_millis(30));
        assert!(gate.mic_muted(), "开窗后必须静音");

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!gate.mic_muted(), "窗口到期必须自动解除");
        assert_eq!(gate.mic_quiet_remaining(), None);
    }

    /// 播放器的结束信号负责把「兜底上限」收紧成尾巴：没有它，一个 2 秒的回答
    /// 会按上限静音十几秒，用户接着说下一句就被丢掉。
    #[tokio::test]
    async fn settling_the_quiet_window_shortens_it_to_the_tail() {
        let gate = PlaybackGate::default();
        let generation = gate.mute_mic_for(Duration::from_secs(30));
        assert!(
            gate.mic_quiet_remaining().unwrap() > Duration::from_secs(29),
            "开窗时是兜底上限"
        );

        gate.settle_quiet_window(generation, Duration::from_millis(200));
        let remaining = gate.mic_quiet_remaining().expect("尾巴还在窗口里");
        assert!(
            remaining <= Duration::from_millis(200),
            "结束信号必须把窗口收紧到尾巴，实际 {remaining:?}"
        );
    }

    /// 上一句播放的结束信号会迟到（它自己还在等设备上的 shell 返回）。若它被
    /// 新窗口认领，第二次播放刚开窗就被关掉，回声又漏进来。
    #[tokio::test]
    async fn a_late_signal_from_the_previous_playback_cannot_close_the_new_window() {
        let gate = PlaybackGate::default();
        let first = gate.mute_mic_for(Duration::from_secs(30));
        let _second = gate.mute_mic_for(Duration::from_secs(30));

        gate.settle_quiet_window(first, Duration::ZERO);
        assert!(
            gate.mic_quiet_remaining().unwrap() > Duration::from_secs(25),
            "旧 generation 的收尾必须被忽略"
        );
    }

    /// 唤醒打断会 `killall` 播放器：那句声音已经没了，窗口必须跟着撤掉，
    /// 否则用户喊完唤醒词说的第一句话就被当成回声丢掉。
    #[test]
    fn beginning_a_session_clears_the_quiet_window() {
        let gate = PlaybackGate::default();
        gate.mute_mic_for(Duration::from_secs(30));
        assert!(gate.mic_muted());

        gate.begin();
        assert!(!gate.mic_muted(), "新会话不能再静音");
    }

    /// 「这一窗还是我们的吗」——`Interrupted` 分支靠它决定收不收窗。
    ///
    /// 唤醒打断（`begin()`）之后不该再动窗口：麦克风那时已经放开，把它收到尾巴
    /// 反而会让用户喊完唤醒词的第一句话被吃掉。而窗口还在手上时（OOM / 手动
    /// killall / 起音乐时的 killall）必须收，否则会静到兜底上限。
    #[test]
    fn owns_follows_the_generation() {
        let gate = PlaybackGate::default();
        let generation = gate.mute_mic_for(Duration::from_secs(30));
        assert!(gate.owns(generation), "刚开的窗应当属于自己");

        gate.begin();
        assert!(!gate.owns(generation), "新会话之后旧 generation 不再是我们的");

        let next = gate.mute_mic_for(Duration::from_secs(30));
        assert!(gate.owns(next), "新窗属于新 generation");
        assert!(!gate.owns(generation), "旧 generation 永远不再属于我们");
    }

    /// 16k 单声道 16bit 的 canonical 头；`data_len_field` 故意可传流式哨兵值。
    fn wav_header(byte_rate: u32, data_len_field: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // 文件总长：解析器不该信它
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&16000u32.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len_field.to_le_bytes());
        out
    }

    #[test]
    fn wav_duration_uses_the_data_chunk() {
        let mut wav = wav_header(32_000, 3_200); // 32000 B/s，0.1 秒
        wav.extend(std::iter::repeat_n(0_u8, 3_200));
        let duration = wav_play_duration(&wav).expect("标准 wav 应能算出时长");
        assert!(
            (duration.as_secs_f64() - 0.1).abs() < 0.002,
            "应为 0.1 秒，实际 {duration:?}"
        );
    }

    /// 实测后端回过流式头（`data` 长度 0x7FFFFFFF）。信它就得到一个几小时的窗口。
    #[test]
    fn streaming_data_length_falls_back_to_the_real_byte_count() {
        let mut wav = wav_header(32_000, 0x7FFF_FFFF);
        wav.extend(std::iter::repeat_n(0_u8, 3_200));
        let duration = wav_play_duration(&wav).expect("流式头也要算得出");
        assert!(
            (duration.as_secs_f64() - 0.1).abs() < 0.002,
            "应按真实剩余字节算成 0.1 秒，实际 {duration:?}"
        );
    }

    #[test]
    fn non_wav_or_empty_data_has_no_duration() {
        assert!(wav_play_duration(b"ID3\x03\x00\x00\x00\x00\x00\x00").is_none());
        assert!(wav_play_duration(&wav_header(32_000, 0)).is_none());
        let mut truncated = wav_header(32_000, 3_200);
        truncated.truncate(20); // 头都没走完
        assert!(wav_play_duration(&truncated).is_none());
    }

    /// 默认播放模板与静音窗依赖的日志路径必须指向同一个文件：改一个忘另一个，
    /// 窗口就只能等兜底上限（回答完十几秒不理人），且没有任何报错。
    #[test]
    fn the_default_playback_command_still_writes_the_player_log() {
        assert!(
            DEFAULT_PLAYBACK_COMMAND.contains(PLAYER_LOG_PATH),
            "默认播放模板不再写 {PLAYER_LOG_PATH}：{DEFAULT_PLAYBACK_COMMAND}"
        );
    }

    /// 默认播放器：开窗给足兜底上限，收尾靠日志里的 `EndReached`。
    #[test]
    fn quiet_plan_waits_for_the_player_log_by_default() {
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200)); // 0.1 秒
        let plan = quiet_plan(&TtsConfig::default(), &wav, true).expect("默认必须开窗");

        // 3s 宽限 + 2×0.1s + 10s 兜底
        assert!(
            (plan.window.as_secs_f64() - 13.2).abs() < 0.05,
            "开窗应为 13.2s，实际 {:?}",
            plan.window
        );
        assert_eq!(plan.tail, Duration::from_millis(500));
        let script = plan.wait_script.expect("默认播放器要看日志");
        assert!(script.contains("EndReached"), "{script}");
        assert!(script.contains(PLAYER_LOG_PATH), "{script}");
        assert!(
            script.contains("sleep 0.2"),
            "OH2P 的 busybox 支持小数 sleep（真机验过）：{script}"
        );
    }

    /// 自定义播放命令看不到播放器日志：退回按时长估，估错的方向是「多静一会儿」。
    #[test]
    fn quiet_plan_for_a_custom_command_uses_the_duration_estimate() {
        let mut config = TtsConfig::default();
        config.playback_command = Some("aplay {path}".to_string());
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200));

        let plan = quiet_plan(&config, &wav, false).expect("自定义命令也要开窗");
        assert!(plan.wait_script.is_none(), "没有日志可等");
        // 3s 宽限 + 0.1s + 0.5s 尾巴
        assert!(
            (plan.window.as_secs_f64() - 3.6).abs() < 0.05,
            "应为 3.6s，实际 {:?}",
            plan.window
        );
    }

    /// 播放器被打断（唤醒会 `killall miplayer`）时日志里不该出现「没等到 EndReached」
    /// 这种像故障的 WARN：真机实测一次打断换一条，排查时全是噪音。
    #[test]
    fn an_interrupted_playback_is_not_reported_as_a_failure() {
        assert_eq!(classify_playback_end(0), PlaybackEnd::Finished);
        assert_eq!(classify_playback_end(2), PlaybackEnd::Interrupted);
        assert_eq!(classify_playback_end(1), PlaybackEnd::Unclear(1));
    }

    /// 「播放器进程没了就退出」这条必须在脚本里：只等 `EndReached` 的话，一次
    /// 唤醒打断会让这个任务空转到兜底上限，然后留下来一条 WARN。
    ///
    /// 用 `pidof` 不用 `pgrep`：设备上没有 `pgrep`（busybox），用了会让每一次正常
    /// 播放都在第一轮被判成「被打断」。`command -v` 守卫则是反过来的一层：连
    /// `pidof` 都没有的固件要退化成「等满上限」，不能被当成「播放器没了」。
    #[test]
    fn the_wait_script_gives_up_when_the_player_is_gone() {
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200));
        let plan = quiet_plan(&TtsConfig::default(), &wav, true).unwrap();
        let script = plan.wait_script.expect("默认播放器");
        assert!(script.contains("pidof miplayer"), "{script}");
        assert!(script.contains("exit 2"), "{script}");
        assert!(
            script.contains("command -v pidof"),
            "探测命令本身要对「设备上有没有它」设防：{script}"
        );
        assert!(
            !script.contains("pgrep"),
            "设备是 busybox，没有 pgrep：{script}"
        );
    }

    /// 关掉开关时一条窗口都不开：这是「宁可自问自答也要留免唤醒插话」的出口。
    #[test]
    fn disabling_the_guard_opens_no_window() {
        let mut config = TtsConfig::default();
        config.suppress_capture_while_playing = false;
        assert!(quiet_plan(&config, b"RIFF....WAVE", true).is_none());
    }

    /// native 走 pns，AEC 认得自己那段声音：**不许**顺手把麦克风也静音掉。
    #[tokio::test]
    async fn native_playback_does_not_mute_the_mic() {
        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        router_with_gate(shell, gate.clone())
            .speak(&TtsConfig::default(), "你好")
            .await
            .unwrap();
        assert!(!gate.mic_muted(), "native 路径不该开静音窗");
    }

    /// 外部播放：起播前开窗、播放器报 `EndReached` 后收紧到尾巴。
    /// 只开窗不收尾的话，一个 2 秒的回答会静音十几秒，用户接着说下一句就被丢掉。
    #[tokio::test]
    async fn external_playback_mutes_the_mic_and_the_end_signal_settles_it() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200)); // 0.1 秒
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                let nl = crlf();
                let head = format!(
                    "HTTP/1.1 200 OK{nl}Content-Type: audio/wav{nl}Content-Length: {}{nl}{nl}",
                    wav.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&wav).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        router_with_gate(shell.clone(), gate.clone())
            .speak(&config, "你好")
            .await
            .expect("起播应当成功");
        assert!(gate.mic_muted(), "外部播放必须开静音窗");

        // 收尾信号在一个独立任务里跑（它要在设备上轮询日志），这里让出执行权。
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            shell.count_matching("EndReached") >= 1,
            "必须真的去等播放器结束，而不是只等兜底上限"
        );
        let remaining = gate.mic_quiet_remaining().expect("尾巴还在窗口里");
        assert!(
            remaining <= Duration::from_millis(600),
            "结束信号没把窗口收紧，实际还剩 {remaining:?}"
        );
    }

    /// 合成期间被打断（用户喊了唤醒词）→ **不播**，但也不算失败。
    ///
    /// 时序用 oneshot 握手而不是 sleep：假后端收到请求后卡住，测试确认「请求已经发出」
    /// 才去打断，然后放行响应。这样「打断一定发生在合成完成之前」是构造出来的，
    /// 不依赖 300ms/100ms 这种相对时间窗（CI 上一次链接抖动就能让它偶发翻车）。
    #[tokio::test]
    async fn a_reply_interrupted_during_synthesis_is_not_played() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = request_tx.send(()); // 「请求已到，合成中」
                let _ = release_rx.await; // 等测试先打断
                let body = b"RIFFfake-wav";
                let nl = crlf();
                let head = format!("HTTP/1.1 200 OK{nl}Content-Length: {}{nl}{nl}", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let router = router_with_gate(shell.clone(), gate.clone());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let speaking =
            tokio::spawn(async move { router.speak(&config, "这句不该被念出来").await });
        request_rx.await.expect("假后端应当先收到请求");
        gate.begin(); // 合成中，用户喊了唤醒词
        release_tx.send(()).expect("放行响应");
        speaking
            .await
            .expect("speak 任务不该 panic")
            .expect("被打断不是失败");

        assert_eq!(
            shell.count_matching("miplayer"),
            0,
            "合成期间被打断时不能再去起播放器"
        );
    }

    /// 复核 P2 的现场：外部 TTS 失败/超时 + 用户已经喊了第二次 → **回退也不许出声**。
    ///
    /// 这条比 openai 那条更容易被真实用户撞上：外部超时往往要等好几秒，用户早就
    /// 不耐烦地又喊了一声。回退是 catch 里新起的动作，很容易被漏掉不查令牌。
    #[tokio::test]
    async fn a_fallback_after_an_interrupt_stays_silent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = request_tx.send(());
                let _ = release_rx.await;
                // 500 → 触发 fallback_to_native
                let nl = crlf();
                let head =
                    format!("HTTP/1.1 500 Internal Server Error{nl}Content-Length: 0{nl}{nl}");
                let _ = socket.write_all(head.as_bytes()).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let router = router_with_gate(shell.clone(), gate.clone());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            fallback_to_native: true,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let speaking = tokio::spawn(async move { router.speak(&config, "不许补播这一句").await });
        request_rx.await.expect("假后端应当先收到请求");
        gate.begin(); // 用户喊了第二次
        release_tx.send(()).expect("放行 500 响应");
        speaking
            .await
            .expect("speak 不该 panic")
            .expect("被打断不是失败");

        assert_eq!(
            shell.count_matching("/usr/sbin/tts_play.sh"),
            0,
            "被打断之后，失败回退也不能出声（否则就是「迟到的一句」）"
        );
    }

    /// 失败计数：到阈值前只 warn，连续三次后升 error；成功一次就清零。
    #[test]
    fn tts_health_escalates_then_resets() {
        let health = TtsHealth::default();
        let err = anyhow!("fake backend down");

        assert_eq!(health.on_failure(&err), 1);
        assert_eq!(health.on_failure(&err), 2);
        assert_eq!(
            health.on_failure(&err),
            TTS_FAILURES_BEFORE_ALARM,
            "第三次应当达到升级阈值"
        );

        health.on_success();
        assert_eq!(
            health.on_failure(&err),
            1,
            "成功一次必须清零，否则「坏过三天」永远都是同一个计数"
        );
    }

    /// `timeout_s` 必须覆盖**响应体读取**，不只是等到响应头。
    ///
    /// 假后端：回 `200` + 一个巨大的 `Content-Length`，然后一个字节都不写。
    /// 如果 body 不受 timeout 约束，这个测试会挂到测试超时；受约束时应当在
    /// `timeout_s` 附近返回 Err 并回退系统 TTS。
    #[tokio::test]
    async fn tts_body_read_is_bounded_by_the_timeout() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                // 用 13/10 拼 CRLF：源码里不出现转义序列，避免被各种工具链改写。
                let nl: String = [13_u8, 10_u8].iter().map(|byte| *byte as char).collect();
                let head = format!("HTTP/1.1 200 OK{nl}Content-Length: 999999{nl}{nl}");
                let _ = socket.write_all(head.as_bytes()).await;
                // 头发完就装死，body 永不到齐。
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 1.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let started = std::time::Instant::now();
        router(shell.clone())
            .speak(&config, "超时测试")
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "body 读取必须受 timeout_s 约束，实际等了 {:?}",
            started.elapsed()
        );
        assert_eq!(
            shell.count_matching("/usr/sbin/tts_play.sh"),
            1,
            "超时后必须回退系统 TTS，而不是这一轮直接没声音"
        );
    }

    /// 播放命令必须带 `{path}`，且路径要被引号化（带空格的文件名不会裂开）。
    #[test]
    fn playback_command_quotes_the_path_and_requires_the_placeholder() {
        let rendered =
            render_playback_command(DEFAULT_PLAYBACK_COMMAND, Path::new("/tmp/a b.wav")).unwrap();
        assert!(
            rendered.contains("miplayer -f '/tmp/a b.wav'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("/proc/[0-9]*") && rendered.contains("*http://*|*https://*"),
            "必须精准过滤本地 TTS miplayer 并保留音乐流: {rendered}"
        );
        assert!(
            !rendered.contains("killall miplayer"),
            "不能再使用 blanket killall miplayer: {rendered}"
        );

        let err = render_playback_command("miplayer -f", Path::new("/tmp/x.wav"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("{path}"), "{err}");
    }

    /// 落盘文件名要带格式后缀：播放器多数按后缀选解复用器。
    #[test]
    fn output_path_follows_the_response_format() {
        let dir = std::env::temp_dir();
        for (format, ext) in [("wav", "wav"), ("mp3", "mp3"), ("pcm", "pcm")] {
            let path = speech_output_path(&dir, format);
            assert_eq!(path.extension().unwrap(), ext);
            assert!(path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("xiaoai-tts-"));
        }
    }

    #[tokio::test]
    async fn speak_stream_native_plays_chunks_sequentially_and_stops_on_interrupt() {
        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let router = router_with_gate(shell.clone(), gate.clone());
        let health = TtsHealth::default();

        let (tx, rx) = mpsc::channel(4);
        tx.send("第一句。".to_string()).await.unwrap();
        tx.send("第二句。".to_string()).await.unwrap();
        drop(tx);

        let mut first_count = 0usize;
        router
            .speak_stream(&TtsConfig::default(), rx, &health, || {
                first_count += 1;
            })
            .await
            .unwrap();
        assert_eq!(shell.command_count(), 2);
        assert_eq!(first_count, 1);

        // 令牌失效后不再起播后续片段
        let (tx2, _rx2) = mpsc::channel::<String>(4);
        tx2.send("不该播".to_string()).await.unwrap();
        drop(tx2);
        gate.begin();
        // 注意：speak_stream 每次进入取当前令牌；在接收途中被 begin() 才会中断
        let (tx3, rx3) = mpsc::channel(4);
        let gate_clone = gate.clone();
        let router_clone = router.clone();
        let handle = tokio::spawn(async move {
            router_clone
                .speak_stream(&TtsConfig::default(), rx3, &TtsHealth::default(), || {})
                .await
        });
        tokio::task::yield_now().await;
        gate_clone.begin();
        let _ = tx3.send("被打断的句子".to_string()).await;
        drop(tx3);
        handle.await.unwrap().unwrap();
        assert_eq!(shell.command_count(), 2, "被打断后不能再起播");
    }

    #[tokio::test]
    async fn speak_stream_openai_prefetches_and_waits_between_chunks() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let req_count = requests.clone();
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200));

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                req_count.fetch_add(1, Ordering::Relaxed);
                let wav = wav.clone();
                tokio::spawn(async move {
                    let mut buf = [0_u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let nl = crlf();
                    let head = format!(
                        "HTTP/1.1 200 OK{nl}Content-Type: audio/wav{nl}Content-Length: {}{nl}{nl}",
                        wav.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&wav).await;
                });
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let router = router_with_gate(shell.clone(), gate);
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let (tx, rx) = mpsc::channel(4);
        tx.send("第一段。".to_string()).await.unwrap();
        tx.send("第二段。".to_string()).await.unwrap();
        drop(tx);

        router
            .speak_stream(&config, rx, &TtsHealth::default(), || {})
            .await
            .unwrap();

        assert_eq!(requests.load(Ordering::Relaxed), 2);
        assert_eq!(shell.count_matching("miplayer -f"), 2);
        assert!(
            shell.count_matching("EndReached") >= 1,
            "多段播报之间必须等上一句结束，否则第二句 killall 会把第一句掐掉"
        );
    }

    #[test]
    fn strip_wake_prefix_and_wake_word_only_support_xiaoai_and_doubao() {
        assert!(is_wake_word_only("小爱同学。"));
        assert!(is_wake_word_only("小爱！"));
        assert!(is_wake_word_only("豆包豆包"));
        assert!(is_wake_word_only("你在吗？"));
        assert!(!is_wake_word_only("小爱同学，今天天气怎么样"));

        assert_eq!(
            strip_wake_prefix("小爱同学，今天天气怎么样？"),
            "今天天气怎么样？"
        );
        assert_eq!(strip_wake_prefix("小爱 几点了"), "几点了");
        assert_eq!(strip_wake_prefix("豆包豆包，讲个笑话"), "讲个笑话");
        assert_eq!(strip_wake_prefix("小爱同学。"), "");
    }

    #[test]
    fn classify_exit_command_distinguishes_silent_and_farewell_without_false_positives() {
        assert_eq!(
            classify_exit_command("闭嘴！", &[]),
            Some(ExitAction::Silent)
        );
        assert_eq!(
            classify_exit_command("算了吧。", &[]),
            Some(ExitAction::Silent)
        );
        assert_eq!(
            classify_exit_command("不用了", &[]),
            Some(ExitAction::Silent)
        );
        assert_eq!(
            classify_exit_command("退下吧", &[]),
            Some(ExitAction::Silent)
        );
        assert_eq!(
            classify_exit_command("再见。", &[]),
            Some(ExitAction::Farewell)
        );
        assert_eq!(
            classify_exit_command("结束对话", &[]),
            Some(ExitAction::Farewell)
        );
        assert_eq!(classify_exit_command("把客厅灯关闭", &[]), None);
        assert_eq!(classify_exit_command("取消明天的闹钟", &[]), None);
        assert_eq!(classify_exit_command("停止播放音乐", &[]), None);
    }

    #[test]
    fn classify_music_voice_control_covers_stop_pause_resume_next_previous() {
        assert_eq!(
            classify_music_voice_control("停止播放音乐。"),
            Some(MusicVoiceControl::Stop)
        );
        assert_eq!(
            classify_music_voice_control("别唱了"),
            Some(MusicVoiceControl::Stop)
        );
        assert_eq!(
            classify_music_voice_control("暂停播放"),
            Some(MusicVoiceControl::Pause)
        );
        assert_eq!(
            classify_music_voice_control("继续播放吧"),
            Some(MusicVoiceControl::Resume)
        );
        assert_eq!(
            classify_music_voice_control("下一首"),
            Some(MusicVoiceControl::Next)
        );
        assert_eq!(
            classify_music_voice_control("上一首"),
            Some(MusicVoiceControl::Previous)
        );
        assert_eq!(classify_music_voice_control("播放周杰伦的晴天"), None);
    }

    #[tokio::test]
    async fn speak_stream_wait_for_end_blocks_until_final_end_reached() {
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use crate::shell::{CommandResult, ShellFuture, ShellRunner};

        struct BlockingEndShell {
            wait_started: Arc<Notify>,
            release_wait: Arc<Notify>,
            wait_finished: Arc<AtomicBool>,
        }

        impl ShellRunner for BlockingEndShell {
            fn run<'a>(&'a self, script: &'a str, _limit: Duration) -> ShellFuture<'a> {
                Box::pin(async move {
                    if script.contains("EndReached") {
                        self.wait_started.notify_one();
                        self.release_wait.notified().await;
                        self.wait_finished.store(true, Ordering::SeqCst);
                    }
                    Ok(CommandResult {
                        exit_code: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                    })
                })
            }

            fn run_detached<'a>(&'a self, script: &'a str, limit: Duration) -> ShellFuture<'a> {
                self.run(script, limit)
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut wav = wav_header(32_000, 3_200);
        wav.extend(std::iter::repeat_n(0_u8, 3_200));

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                let nl = crlf();
                let head = format!(
                    "HTTP/1.1 200 OK{nl}Content-Type: audio/wav{nl}Content-Length: {}{nl}{nl}",
                    wav.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&wav).await;
            }
        });

        let wait_started = Arc::new(Notify::new());
        let release_wait = Arc::new(Notify::new());
        let wait_finished = Arc::new(AtomicBool::new(false));
        let shell = Arc::new(BlockingEndShell {
            wait_started: wait_started.clone(),
            release_wait: release_wait.clone(),
            wait_finished: wait_finished.clone(),
        });
        let gate = Arc::new(PlaybackGate::default());
        let router = TtsRouter::new(Device::with_runner(DeviceConfig::default(), shell), gate);
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let (tx, rx) = mpsc::channel(2);
        tx.send("最后一句告别。".to_string()).await.unwrap();
        drop(tx);

        let task = tokio::spawn(async move {
            router
                .speak_stream_with_mode(
                    &config,
                    rx,
                    &TtsHealth::default(),
                    FinishMode::WaitForEnd,
                    || {},
                )
                .await
        });

        // Wait until the EndReached watcher has started, then verify speak_stream_with_mode
        // has NOT returned prematurely.
        wait_started.notified().await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !task.is_finished(),
            "FinishMode::WaitForEnd 必须等末句 EndReached 完成才返回"
        );
        assert!(!wait_finished.load(Ordering::SeqCst));

        release_wait.notify_one();
        task.await.unwrap().unwrap();
        assert!(
            wait_finished.load(Ordering::SeqCst),
            "返回时末句 EndReached 必须已完成"
        );
    }

    #[test]
    fn prune_old_speech_files_caps_file_count_to_five_newest() {
        let unique = format!(
            "xiaoai-prune-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).unwrap();

        for i in 1..=8 {
            let p = dir.join(format!("xiaoai-tts-000{i}.wav"));
            std::fs::write(&p, b"audio").unwrap();
        }
        let unrelated = dir.join("other-file.txt");
        std::fs::write(&unrelated, b"keep").unwrap();

        prune_old_speech_files(&dir);

        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("xiaoai-tts-"))
            .collect();
        remaining.sort();

        assert_eq!(
            remaining,
            vec![
                "xiaoai-tts-0004.wav",
                "xiaoai-tts-0005.wav",
                "xiaoai-tts-0006.wav",
                "xiaoai-tts-0007.wav",
                "xiaoai-tts-0008.wav",
            ],
            "必须最多保留 5 个最新的 xiaoai-tts-* 文件"
        );
        assert!(unrelated.exists(), "不该误删非 xiaoai-tts-* 文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
