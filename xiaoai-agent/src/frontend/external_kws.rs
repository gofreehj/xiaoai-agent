//! Built-in generic external KWS frontend (ALSA capture + UDP XPCM client).
//!
//! Replaces the external `lx06-kws-relay` C daemon and `/tmp/xiaoai-kws.sock`
//! framing layer by running ALSA `arecord` capture, downsampling/HPF, hardware
//! DAC loopback detection, 2.56s standby pre-wake ring buffering, and the
//! generic UDP XPCM protocol directly inside `xiaoai-agent`.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::net::{lookup_host, UdpSocket};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::capture::assemble_utterance;
use crate::config::{CaptureConfig, DualCaptureConfig, DualCaptureFormat, DualKwsConfig};
use crate::frontend::BargeInSignal;
use crate::vad::{SpeechCollector, SpeechEvent};

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
pub const STEP_SAMPLES: usize = 640; // 40ms @ 16kHz
pub const PCM_BYTES: usize = STEP_SAMPLES * 2;
pub const QUEUE_CAP: usize = 64;
pub const RING_CAP: usize = 64; // 2.56s @ 40ms
pub const ONESHOT_PREWAKE_CHUNKS: usize = 5; // 200ms pre-wake rewind
pub const ONESHOT_MAX_REPLAY_CHUNKS: usize = 38; // ~1.52s max oneshot catch-up

pub const XPCM_MAGIC: &[u8; 4] = b"XPCM";
pub const XPCM_HDR_LEN: usize = 8;
pub const XPCM_FLAG_PCM: u16 = 1;
pub const XPCM_FLAG_RESET: u16 = 2;
pub const XPCM_FLAG_PING: u16 = 3;
pub const XPCM_FLAG_SYS_ACTIVE: u16 = 0x0100;

const SYS_REF_MEAN_SQ_THR: f64 = 25.0 * 25.0;
const SYS_HANGOVER_CHUNKS: usize = 7; // ~280ms acoustic reverb tail

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub pcm: Bytes,
    pub sys_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KwsUdpReply {
    Pong,
    Wake { seq: u32, keyword: String },
}

/// Encode an 8-byte XPCM packet: `"XPCM"` + `seq (u16 LE)` + `flags (u16 LE)` + `pcm`.
pub fn encode_xpcm_packet(seq: u16, flags: u16, pcm: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(XPCM_HDR_LEN + pcm.len());
    pkt.extend_from_slice(XPCM_MAGIC);
    pkt.extend_from_slice(&seq.to_le_bytes());
    pkt.extend_from_slice(&flags.to_le_bytes());
    pkt.extend_from_slice(pcm);
    pkt
}

/// Parse a UDP reply packet from an external KWS server (`XPONG\n` or `WAKE:<seq>:<kw>\n`).
pub fn parse_udp_reply(raw: &[u8]) -> Option<KwsUdpReply> {
    let text = std::str::from_utf8(raw).ok()?.trim_end_matches(['\r', '\n']);
    if text.starts_with("XPONG") {
        return Some(KwsUdpReply::Pong);
    }
    if text == "WAKE" {
        return Some(KwsUdpReply::Wake {
            seq: 0,
            keyword: "unknown".to_string(),
        });
    }
    if let Some(rest) = text.strip_prefix("WAKE:") {
        if let Some((seq_str, kw)) = rest.split_once(':') {
            let seq = seq_str.trim().parse::<u32>().unwrap_or(0);
            let kw = kw.trim();
            return Some(KwsUdpReply::Wake {
                seq,
                keyword: if kw.is_empty() {
                    "unknown".to_string()
                } else {
                    kw.to_string()
                },
            });
        }
        return Some(KwsUdpReply::Wake {
            seq: 0,
            keyword: rest.trim().to_string(),
        });
    }
    None
}

/// Classify a wake keyword payload into either a wake trigger or an exit command.
pub fn parse_wake_signal(keyword: &str) -> BargeInSignal {
    if keyword.trim() == "__CMD_EXIT__" {
        BargeInSignal::ExitCmd
    } else {
        BargeInSignal::Wake
    }
}

/// Probe an external UDP XPCM KWS endpoint once with `XPCM_FLAG_PING` and return RTT.
pub async fn probe_external_kws(endpoint: &str, timeout_dur: Duration) -> anyhow::Result<Duration> {
    let target = resolve_endpoint(endpoint).await?;
    let udp = bind_matching_udp(target).await?;
    let ping = encode_xpcm_packet(0, XPCM_FLAG_PING, &[]);
    let started = Instant::now();
    udp.send_to(&ping, target).await?;

    let mut buf = [0u8; 512];
    let recv_fut = async {
        loop {
            let (n, _) = udp.recv_from(&mut buf).await?;
            if matches!(parse_udp_reply(&buf[..n]), Some(KwsUdpReply::Pong)) {
                return Ok::<Duration, anyhow::Error>(started.elapsed());
            }
        }
    };
    timeout(timeout_dur, recv_fut)
        .await
        .map_err(|_| anyhow::anyhow!("UDP PING 超时（未收到 XPONG）"))?
}

async fn resolve_endpoint(endpoint: &str) -> anyhow::Result<SocketAddr> {
    let trimmed = endpoint.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "dual.kws.endpoint 不能为空（例如 192.168.31.1:5005）"
    );
    lookup_host(trimmed)
        .await
        .map_err(|e| anyhow::anyhow!("解析外部 KWS 地址 {trimmed} 失败: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("外部 KWS 地址 {trimmed} 未解析出任何 IP"))
}

async fn bind_matching_udp(target: SocketAddr) -> anyhow::Result<UdpSocket> {
    let bind_addr = match target {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    Ok(UdpSocket::bind(bind_addr).await?)
}

/// Multi-channel ALSA raw PCM -> 16 kHz mono S16_LE downsampler + 1st-order HPF
/// + soft-knee limiter + optional hardware DAC loopback activity detector.
#[derive(Debug, Clone)]
pub struct PcmDownsampler {
    channels: usize,
    mic_channel: usize,
    ref_channel: Option<usize>,
    format: DualCaptureFormat,
    decim: usize,
    shift_bits: u8,
    prev_x: f32,
    prev_y: f32,
    ref_prev_x: f32,
    ref_prev_y: f32,
    sys_hangover: usize,
}

impl PcmDownsampler {
    pub fn new(cfg: &DualCaptureConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(cfg.channels >= 1, "dual.capture.channels 必须 >= 1");
        anyhow::ensure!(
            cfg.mic_channel < cfg.channels,
            "dual.capture.mic_channel ({}) 必须 < channels ({})",
            cfg.mic_channel,
            cfg.channels
        );
        if let Some(ref_ch) = cfg.ref_channel {
            anyhow::ensure!(
                ref_ch < cfg.channels,
                "dual.capture.ref_channel ({ref_ch}) 必须 < channels ({})",
                cfg.channels
            );
        }
        anyhow::ensure!(
            cfg.sample_rate >= TARGET_SAMPLE_RATE && cfg.sample_rate % TARGET_SAMPLE_RATE == 0,
            "dual.capture.sample_rate ({}) 必须为 {} 的正整数倍",
            cfg.sample_rate,
            TARGET_SAMPLE_RATE
        );
        Ok(Self {
            channels: cfg.channels as usize,
            mic_channel: cfg.mic_channel as usize,
            ref_channel: cfg.ref_channel.map(|c| c as usize),
            format: cfg.format,
            decim: (cfg.sample_rate / TARGET_SAMPLE_RATE) as usize,
            shift_bits: cfg.shift_bits,
            prev_x: 0.0,
            prev_y: 0.0,
            ref_prev_x: 0.0,
            ref_prev_y: 0.0,
            sys_hangover: 0,
        })
    }

    pub fn raw_bytes_per_chunk(&self) -> usize {
        STEP_SAMPLES * self.decim * self.channels * self.bytes_per_sample()
    }

    fn bytes_per_sample(&self) -> usize {
        match self.format {
            DualCaptureFormat::S32Le => 4,
            DualCaptureFormat::S16Le => 2,
        }
    }

    #[inline]
    fn read_sample(&self, raw: &[u8], offset: usize) -> i32 {
        match self.format {
            DualCaptureFormat::S32Le => i32::from_le_bytes([
                raw[offset],
                raw[offset + 1],
                raw[offset + 2],
                raw[offset + 3],
            ]),
            DualCaptureFormat::S16Le => i16::from_le_bytes([raw[offset], raw[offset + 1]]) as i32,
        }
    }

    /// Downsample one `raw_bytes_per_chunk()` buffer into a 40ms 16 kHz mono [`AudioChunk`].
    pub fn process_raw(&mut self, raw: &[u8]) -> AudioChunk {
        debug_assert_eq!(raw.len(), self.raw_bytes_per_chunk());
        let bps = self.bytes_per_sample();
        let frame_bytes = self.channels * bps;
        let step_stride = self.decim * frame_bytes;
        let decim_i64 = self.decim as i64;
        let mut out = Vec::with_capacity(PCM_BYTES);
        let mut ref_sq_sum = 0.0f64;

        for i in 0..STEP_SAMPLES {
            let base = i * step_stride;
            let mut mic_sum = 0i64;
            let mut ref_sum = 0i64;

            for k in 0..self.decim {
                let frame_off = base + k * frame_bytes;
                let mic_off = frame_off + self.mic_channel * bps;
                mic_sum += self.read_sample(raw, mic_off) as i64;

                if let Some(ref_ch) = self.ref_channel {
                    let ref_off = frame_off + ref_ch * bps;
                    ref_sum += self.read_sample(raw, ref_off) as i64;
                }
            }

            if self.ref_channel.is_some() {
                let ref_avg = match self.format {
                    DualCaptureFormat::S32Le => ((ref_sum / decim_i64) >> self.shift_bits) as f32,
                    DualCaptureFormat::S16Le => (ref_sum / decim_i64) as f32,
                };
                let ref_hp = ref_avg - self.ref_prev_x + 0.985 * self.ref_prev_y;
                self.ref_prev_x = ref_avg;
                self.ref_prev_y = ref_hp;
                ref_sq_sum += (ref_hp as f64) * (ref_hp as f64);
            }

            let avg = match self.format {
                DualCaptureFormat::S32Le => ((mic_sum / decim_i64) >> self.shift_bits) as f32,
                DualCaptureFormat::S16Le => (mic_sum / decim_i64) as f32,
            };
            let mut hp = avg - self.prev_x + 0.985 * self.prev_y;
            self.prev_x = avg;
            self.prev_y = hp;

            if matches!(self.format, DualCaptureFormat::S32Le) {
                if hp > 24_576.0 {
                    hp = 24_576.0 + (hp - 24_576.0) / (1.0 + (hp - 24_576.0) / 8191.0);
                } else if hp < -24_576.0 {
                    hp = -24_576.0 + (hp + 24_576.0) / (1.0 - (hp + 24_576.0) / 8191.0);
                }
            }

            let rounded = if hp >= 0.0 { hp + 0.5 } else { hp - 0.5 } as i32;
            let sample = rounded.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            out.extend_from_slice(&sample.to_le_bytes());
        }

        let sys_active = if self.ref_channel.is_some() {
            if (ref_sq_sum / STEP_SAMPLES as f64) >= SYS_REF_MEAN_SQ_THR {
                self.sys_hangover = SYS_HANGOVER_CHUNKS;
            } else if self.sys_hangover > 0 {
                self.sys_hangover -= 1;
            }
            self.sys_hangover > 0
        } else {
            false
        };

        AudioChunk {
            pcm: Bytes::from(out),
            sys_active,
        }
    }
}

fn spawn_alsa_capture_worker(
    cfg: DualCaptureConfig,
    audio_tx: mpsc::Sender<AudioChunk>,
) -> anyhow::Result<JoinHandle<()>> {
    let mut downsampler = PcmDownsampler::new(&cfg)?;
    let raw_len = downsampler.raw_bytes_per_chunk();
    let format_str = match cfg.format {
        DualCaptureFormat::S32Le => "S32_LE",
        DualCaptureFormat::S16Le => "S16_LE",
    };
    let rate_str = cfg.sample_rate.to_string();
    let ch_str = cfg.channels.to_string();

    Ok(tokio::spawn(async move {
        let mut raw = vec![0u8; raw_len];
        loop {
            info!(
                device = %cfg.device,
                sample_rate = cfg.sample_rate,
                channels = cfg.channels,
                format = format_str,
                "启动 ALSA 音频采集子进程 (arecord)"
            );

            let mut cmd = Command::new("arecord");
            cmd.args([
                "-D",
                &cfg.device,
                "-r",
                &rate_str,
                "-f",
                format_str,
                "-c",
                &ch_str,
                "-t",
                "raw",
                "-q",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(err) => {
                    warn!(error = %err, device = %cfg.device, "启动 arecord 失败，500ms 后重试");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };

            let Some(mut stdout) = child.stdout.take() else {
                let _ = child.kill().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            };

            while stdout.read_exact(&mut raw).await.is_ok() {
                let chunk = downsampler.process_raw(&raw);
                match audio_tx.try_send(chunk) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Drop newest chunk if engine is backlogged so arecord never stalls.
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        return;
                    }
                }
            }

            warn!("arecord 音频流中断，500ms 后自动拉起");
            let _ = child.kill().await;
            let _ = child.wait().await;
            if audio_tx.is_closed() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }))
}

enum EngineCommand {
    StartStream {
        oneshot: bool,
        reply: oneshot::Sender<mpsc::Receiver<AudioChunk>>,
    },
    StopStream {
        reply: oneshot::Sender<()>,
    },
}

/// Built-in dual-mode external KWS engine + handle owned by the session daemon.
pub struct ExternalKwsEngine;

impl ExternalKwsEngine {
    /// Start ALSA capture + UDP XPCM engine using `cfg`.
    pub async fn spawn(cfg: &DualKwsConfig) -> anyhow::Result<ExternalKwsHandle> {
        let (audio_tx, audio_rx) = mpsc::channel(QUEUE_CAP);
        // Validate endpoint + capture settings before spawning child processes.
        let _ = resolve_endpoint(&cfg.kws.endpoint).await?;
        let capture_task = spawn_alsa_capture_worker(cfg.capture.clone(), audio_tx)?;
        let ping_interval = Duration::from_secs(cfg.kws.ping_interval_s.max(5));
        match Self::spawn_with_audio_rx(&cfg.kws.endpoint, ping_interval, audio_rx).await {
            Ok(mut handle) => {
                handle.capture_task = Some(capture_task);
                Ok(handle)
            }
            Err(err) => {
                capture_task.abort();
                Err(err)
            }
        }
    }

    /// Start UDP XPCM engine driven by an arbitrary [`AudioChunk`] receiver (used by tests).
    pub async fn spawn_with_audio_rx(
        endpoint: &str,
        ping_interval: Duration,
        mut audio_rx: mpsc::Receiver<AudioChunk>,
    ) -> anyhow::Result<ExternalKwsHandle> {
        let target_addr = resolve_endpoint(endpoint).await?;
        let udp = bind_matching_udp(target_addr).await?;
        let local_port = udp.local_addr().map(|a| a.port()).unwrap_or(0);
        info!(
            endpoint = %target_addr,
            local_udp_port = local_port,
            "外部 KWS UDP XPCM 客户端已启动"
        );

        let initial_ping = encode_xpcm_packet(0, XPCM_FLAG_PING, &[]);
        let _ = udp.send_to(&initial_ping, target_addr).await;

        let (wake_tx, wake_rx) = mpsc::channel(16);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);

        let engine_task = tokio::spawn(async move {
            let mut ring: VecDeque<AudioChunk> = VecDeque::with_capacity(RING_CAP);
            let mut ring_total: u64 = 0;
            let mut wake_replay_idx: Option<u64> = None;
            let mut stream_tx: Option<mpsc::Sender<AudioChunk>> = None;
            let mut seq: u16 = 0;
            let mut last_wake_seq: u32 = 0;
            let mut last_wake_at: Option<Instant> = None;
            let mut last_rx_at = Instant::now();
            let mut last_timeout_warn: Option<Instant> = None;
            let mut udp_buf = [0u8; 1024];

            let mut ping_ticker = interval(ping_interval);
            ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ping_ticker.tick().await;

            loop {
                tokio::select! {
                    maybe_chunk = audio_rx.recv() => {
                        let Some(chunk) = maybe_chunk else {
                            break;
                        };
                        if let Some(tx) = stream_tx.as_ref() {
                            match tx.try_send(chunk) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    stream_tx = None;
                                }
                            }
                        } else {
                            if ring.len() == RING_CAP {
                                ring.pop_front();
                            }
                            ring.push_back(chunk.clone());
                            ring_total += 1;
                            seq = seq.wrapping_add(1);
                            let flags = XPCM_FLAG_PCM
                                | if chunk.sys_active { XPCM_FLAG_SYS_ACTIVE } else { 0 };
                            let pkt = encode_xpcm_packet(seq, flags, &chunk.pcm);
                            let _ = udp.send_to(&pkt, target_addr).await;
                        }
                    }
                    recv_res = udp.recv_from(&mut udp_buf) => {
                        match recv_res {
                            Ok((n, _)) => {
                                if let Some(reply) = parse_udp_reply(&udp_buf[..n]) {
                                    last_rx_at = Instant::now();
                                    last_timeout_warn = None;
                                    match reply {
                                        KwsUdpReply::Pong => {
                                            debug!(endpoint = %target_addr, "收到外部 KWS XPONG 心跳响应");
                                        }
                                        KwsUdpReply::Wake { seq: wake_seq, keyword } => {
                                            let now = Instant::now();
                                            let is_dup = if wake_seq > 0 {
                                                wake_seq == last_wake_seq
                                                    && last_wake_at.is_some_and(|t| {
                                                        now.duration_since(t) < Duration::from_secs(5)
                                                    })
                                            } else {
                                                last_wake_at.is_some_and(|t| {
                                                    now.duration_since(t) < Duration::from_millis(1200)
                                                })
                                            };
                                            if is_dup {
                                                continue;
                                            }
                                            last_wake_seq = wake_seq;
                                            last_wake_at = Some(now);

                                            let signal = parse_wake_signal(&keyword);
                                            info!(
                                                seq = wake_seq,
                                                keyword = %keyword,
                                                ?signal,
                                                "收到外部 KWS 唤醒通知"
                                            );
                                            if signal == BargeInSignal::Wake {
                                                wake_replay_idx = Some(
                                                    ring_total.saturating_sub(ONESHOT_PREWAKE_CHUNKS as u64),
                                                );
                                            } else {
                                                wake_replay_idx = None;
                                            }
                                            let _ = wake_tx.try_send(signal);
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, "外部 KWS UDP 接收出错");
                            }
                        }
                    }
                    maybe_cmd = cmd_rx.recv() => {
                        let Some(cmd) = maybe_cmd else {
                            break;
                        };
                        match cmd {
                            EngineCommand::StartStream { oneshot, reply } => {
                                let (tx, rx) = mpsc::channel(QUEUE_CAP);
                                seq = seq.wrapping_add(1);
                                let reset_pkt = encode_xpcm_packet(seq, XPCM_FLAG_RESET, &[]);
                                let _ = udp.send_to(&reset_pkt, target_addr).await;

                                if oneshot {
                                    if let Some(replay_from) = wake_replay_idx.take() {
                                        if ring_total > replay_from {
                                            let avail = (ring_total - replay_from) as usize;
                                            let replay_count = avail
                                                .min(ONESHOT_MAX_REPLAY_CHUNKS)
                                                .min(ring.len());
                                            let skip = ring.len().saturating_sub(replay_count);
                                            for chunk in ring.iter().skip(skip) {
                                                let _ = tx.try_send(chunk.clone());
                                            }
                                        }
                                    }
                                } else {
                                    wake_replay_idx = None;
                                }
                                stream_tx = Some(tx);
                                let _ = reply.send(rx);
                            }
                            EngineCommand::StopStream { reply } => {
                                stream_tx = None;
                                let _ = reply.send(());
                            }
                        }
                    }
                    _ = ping_ticker.tick() => {
                        if stream_tx.is_none() {
                            seq = seq.wrapping_add(1);
                            let ping_pkt = encode_xpcm_packet(seq, XPCM_FLAG_PING, &[]);
                            let _ = udp.send_to(&ping_pkt, target_addr).await;
                            let silent_for = last_rx_at.elapsed();
                            if silent_for > Duration::from_secs(60)
                                && last_timeout_warn.is_none_or(|t| t.elapsed() >= ping_interval)
                            {
                                warn!(
                                    endpoint = %target_addr,
                                    silent_s = silent_for.as_secs(),
                                    "超过 60s 未收到外部 KWS XPONG/WAKE 响应"
                                );
                                last_timeout_warn = Some(Instant::now());
                            }
                        }
                    }
                }
            }
        });

        Ok(ExternalKwsHandle {
            wake_rx,
            cmd_tx,
            active_rx: None,
            capture_task: None,
            engine_task: Some(engine_task),
        })
    }
}

/// Handle used by the dual-mode session loop to wait for wakes and pull PCM streams.
pub struct ExternalKwsHandle {
    wake_rx: mpsc::Receiver<BargeInSignal>,
    cmd_tx: mpsc::Sender<EngineCommand>,
    active_rx: Option<mpsc::Receiver<AudioChunk>>,
    capture_task: Option<JoinHandle<()>>,
    engine_task: Option<JoinHandle<()>>,
}

impl ExternalKwsHandle {
    pub async fn next_wake(&mut self) -> anyhow::Result<BargeInSignal> {
        self.wake_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("外部 KWS 引擎通道已关闭"))
    }

    pub fn drain_pending_wakes(&mut self) {
        while self.wake_rx.try_recv().is_ok() {}
    }

    pub async fn start_stream(&mut self) -> anyhow::Result<()> {
        self.start_stream_mode(false).await
    }

    pub async fn start_oneshot_stream(&mut self) -> anyhow::Result<()> {
        self.start_stream_mode(true).await
    }

    async fn start_stream_mode(&mut self, oneshot: bool) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(EngineCommand::StartStream {
                oneshot,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("外部 KWS 引擎已停止"))?;
        let rx = reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("外部 KWS 启动音频流失败"))?;
        self.active_rx = Some(rx);
        Ok(())
    }

    pub async fn stop_stream(&mut self) {
        self.active_rx = None;
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(EngineCommand::StopStream { reply: reply_tx })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    pub async fn capture_utterance(
        &mut self,
        capture_config: &CaptureConfig,
        timeout_duration: Duration,
        oneshot_probe: Option<Duration>,
    ) -> anyhow::Result<Option<Bytes>> {
        let Some(ref mut rx) = self.active_rx else {
            anyhow::bail!("尚未开启双模式音频流");
        };
        capture_dual_utterance(rx, capture_config, timeout_duration, oneshot_probe).await
    }

    pub async fn wait_for_barge_in(&mut self) -> anyhow::Result<BargeInSignal> {
        self.next_wake().await
    }
}

impl Drop for ExternalKwsHandle {
    fn drop(&mut self) {
        if let Some(task) = self.capture_task.take() {
            task.abort();
        }
        if let Some(task) = self.engine_task.take() {
            task.abort();
        }
    }
}

/// Tracks monotonic dual-KWS session/turn identity and freshness windows.
#[derive(Debug, Clone)]
pub struct SessionTracker {
    pub session_id: u64,
    pub turn_seq: u64,
    pub wake_at: Instant,
    pub last_updated_at: Instant,
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTracker {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            turn_seq: 0,
            wake_at: now,
            last_updated_at: now,
        }
    }

    pub fn begin_turn(&mut self) -> u64 {
        self.turn_seq += 1;
        self.last_updated_at = Instant::now();
        self.turn_seq
    }

    pub fn touch(&mut self) {
        self.last_updated_at = Instant::now();
    }

    pub fn reset_wake_window(&mut self) {
        let now = Instant::now();
        self.wake_at = now;
        self.last_updated_at = now;
    }

    pub fn is_fresh_for_enter_prompt(&self, max_age: Duration) -> bool {
        self.wake_at.elapsed() <= max_age
    }

    #[cfg(test)]
    pub fn is_turn_fresh(
        &self,
        expected_session: u64,
        expected_turn: u64,
        max_age: Duration,
    ) -> bool {
        self.session_id == expected_session
            && self.turn_seq == expected_turn
            && self.last_updated_at.elapsed() <= max_age
    }
}

/// Capture a single utterance from the dual-KWS chunk stream using [`SpeechCollector`].
pub async fn capture_dual_utterance(
    rx: &mut mpsc::Receiver<AudioChunk>,
    capture_config: &CaptureConfig,
    timeout_duration: Duration,
    oneshot_probe: Option<Duration>,
) -> anyhow::Result<Option<Bytes>> {
    let mut collector = SpeechCollector::new(capture_config);
    let started_at = Instant::now();
    let deadline = started_at + timeout_duration;
    let probe_deadline = oneshot_probe.map(|d| started_at + d);
    let utterance_cap = Duration::from_secs_f64(capture_config.max_utterance_s.max(5.0) + 3.0);
    let mut speech_prefix: Option<Bytes> = None;
    let mut utterance_deadline: Option<Instant> = None;

    loop {
        let now = Instant::now();
        if let Some(ud) = utterance_deadline {
            if now >= ud {
                warn!("单句语音采集达到安全保护上限，结束本轮等待");
                return Ok(None);
            }
        } else {
            if now >= deadline {
                return Ok(None);
            }
            if let Some(pd) = probe_deadline {
                if now >= pd {
                    return Ok(None);
                }
            }
        }

        let wait = if let Some(ud) = utterance_deadline {
            ud.saturating_duration_since(now)
                .min(Duration::from_secs(3))
                .max(Duration::from_millis(1))
        } else {
            let mut w = deadline
                .saturating_duration_since(now)
                .min(Duration::from_secs(3))
                .max(Duration::from_millis(1));
            if let Some(pd) = probe_deadline {
                w = w.min(pd.saturating_duration_since(now).max(Duration::from_millis(1)));
            }
            w
        };

        let chunk = match timeout(wait, rx.recv()).await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                if speech_prefix.is_some() {
                    if let Some(tail) = collector.flush() {
                        let full_pcm = assemble_utterance(speech_prefix.take(), tail);
                        return Ok(Some(full_pcm));
                    }
                }
                anyhow::bail!("外部 KWS 音频流已关闭");
            }
            Err(_) => {
                let now_after = Instant::now();
                if let Some(ud) = utterance_deadline {
                    if now_after >= ud {
                        return Ok(None);
                    }
                } else {
                    if let Some(pd) = probe_deadline {
                        if now_after >= pd {
                            return Ok(None);
                        }
                    }
                    if now_after >= deadline {
                        return Ok(None);
                    }
                }
                anyhow::bail!("等待外部 KWS 音频流超时 (>3s 无数据)");
            }
        };

        if chunk.pcm.is_empty() {
            continue;
        }
        if chunk.sys_active && speech_prefix.is_none() {
            continue;
        }

        for event in collector.push(&chunk.pcm) {
            match event {
                SpeechEvent::SpeechStart(prefix) => {
                    info!(prefix_bytes = prefix.len(), "VAD 检测到用户开始说话...");
                    speech_prefix = Some(prefix);
                    utterance_deadline = Some(Instant::now() + utterance_cap);
                }
                SpeechEvent::SpeechRejected => {
                    speech_prefix = None;
                    utterance_deadline = None;
                }
                SpeechEvent::Utterance(tail) => {
                    let full_pcm = assemble_utterance(speech_prefix.take(), tail);
                    info!(len = full_pcm.len(), "VAD 检测到用户说话结束，切片完成！");
                    return Ok(Some(full_pcm));
                }
                SpeechEvent::SpeechChunk(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::sleep;

    use super::*;

    fn make_pcm_bytes(sample: i16, samples: usize) -> Bytes {
        let mut out = Vec::with_capacity(samples * 2);
        for _ in 0..samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        Bytes::from(out)
    }

    fn make_chunk(sample: i16, sys_active: bool) -> AudioChunk {
        AudioChunk {
            pcm: make_pcm_bytes(sample, STEP_SAMPLES),
            sys_active,
        }
    }

    #[test]
    fn xpcm_packet_and_reply_roundtrip() {
        let pkt = encode_xpcm_packet(42, XPCM_FLAG_PCM | XPCM_FLAG_SYS_ACTIVE, b"\x01\x02");
        assert_eq!(&pkt[..4], b"XPCM");
        assert_eq!(u16::from_le_bytes([pkt[4], pkt[5]]), 42);
        assert_eq!(
            u16::from_le_bytes([pkt[6], pkt[7]]),
            XPCM_FLAG_PCM | XPCM_FLAG_SYS_ACTIVE
        );
        assert_eq!(&pkt[8..], b"\x01\x02");

        assert_eq!(parse_udp_reply(b"XPONG\n"), Some(KwsUdpReply::Pong));
        assert_eq!(
            parse_udp_reply("WAKE:7:豆包豆包\r\n".as_bytes()),
            Some(KwsUdpReply::Wake {
                seq: 7,
                keyword: "豆包豆包".to_string()
            })
        );
        assert_eq!(parse_wake_signal("豆包豆包"), BargeInSignal::Wake);
        assert_eq!(parse_wake_signal("__CMD_EXIT__"), BargeInSignal::ExitCmd);
    }

    #[test]
    fn pcm_downsampler_handles_lx06_s32_8ch_and_sys_active() {
        let cfg = DualCaptureConfig::default();
        let mut ds = PcmDownsampler::new(&cfg).unwrap();
        let raw_len = ds.raw_bytes_per_chunk();
        assert_eq!(raw_len, STEP_SAMPLES * 3 * 8 * 4);

        // Inject a loud signal on mic ch0 and system loopback on ch6.
        let mut raw = vec![0u8; raw_len];
        for frame_idx in 0..(STEP_SAMPLES * 3) {
            let mic_val: i32 = if frame_idx % 2 == 0 {
                4000 << 9
            } else {
                -4000 << 9
            };
            let ref_val: i32 = if frame_idx % 2 == 0 { 800 << 9 } else { -800 << 9 };
            let off = frame_idx * 8 * 4;
            raw[off..off + 4].copy_from_slice(&mic_val.to_le_bytes());
            raw[off + 6 * 4..off + 7 * 4].copy_from_slice(&ref_val.to_le_bytes());
        }

        let chunk = ds.process_raw(&raw);
        assert_eq!(chunk.pcm.len(), PCM_BYTES);
        assert!(chunk.sys_active, "ch6 DAC loopback should flag sys_active");

        // Silence keeps hangover active for (SYS_HANGOVER_CHUNKS - 1) chunks, clearing on the next.
        let silent = vec![0u8; raw_len];
        for _ in 0..(SYS_HANGOVER_CHUNKS - 1) {
            assert!(ds.process_raw(&silent).sys_active);
        }
        assert!(!ds.process_raw(&silent).sys_active);
    }

    #[test]
    fn pcm_downsampler_handles_generic_16khz_mono_s16_without_ref() {
        let cfg = DualCaptureConfig {
            device: "default".into(),
            sample_rate: 16_000,
            channels: 1,
            format: DualCaptureFormat::S16Le,
            mic_channel: 0,
            ref_channel: None,
            shift_bits: 0,
        };
        let mut ds = PcmDownsampler::new(&cfg).unwrap();
        assert_eq!(ds.raw_bytes_per_chunk(), PCM_BYTES);

        let raw = make_pcm_bytes(1000, STEP_SAMPLES);
        let chunk = ds.process_raw(&raw);
        assert_eq!(chunk.pcm.len(), PCM_BYTES);
        assert!(!chunk.sys_active);
    }

    #[tokio::test]
    async fn probe_external_kws_succeeds_against_loopback_udp_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();

        let srv_task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            let (n, peer) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..4], b"XPCM");
            assert_eq!(u16::from_le_bytes([buf[6], buf[7]]), XPCM_FLAG_PING);
            assert_eq!(n, XPCM_HDR_LEN);
            server.send_to(b"XPONG\n", peer).await.unwrap();
        });

        let rtt = probe_external_kws(&addr.to_string(), Duration::from_secs(2))
            .await
            .expect("probe should receive XPONG");
        assert!(rtt < Duration::from_secs(1));
        srv_task.await.unwrap();
    }

    #[tokio::test]
    async fn external_kws_engine_end_to_end_udp_wake_and_oneshot_replay() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();

        let (audio_tx, audio_rx) = mpsc::channel(16);
        let mut handle =
            ExternalKwsEngine::spawn_with_audio_rx(&addr.to_string(), Duration::from_secs(30), audio_rx)
                .await
                .unwrap();

        let srv_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut peer_addr = None;
            let mut saw_pcm = false;
            let mut saw_reset = false;
            for _ in 0..20 {
                let Ok(Ok((n, peer))) = timeout(Duration::from_secs(2), server.recv_from(&mut buf)).await
                else {
                    break;
                };
                peer_addr = Some(peer);
                if n >= XPCM_HDR_LEN && &buf[..4] == b"XPCM" {
                    let flags = u16::from_le_bytes([buf[6], buf[7]]);
                    if flags == XPCM_FLAG_PING {
                        let _ = server.send_to(b"XPONG\n", peer).await;
                    } else if (flags & 0x00FF) == XPCM_FLAG_PCM && !saw_pcm {
                        saw_pcm = true;
                        // Emit duplicate WAKE packets; engine must deduplicate seq=1.
                        let _ = server.send_to("WAKE:1:豆包豆包\n".as_bytes(), peer).await;
                        let _ = server.send_to("WAKE:1:豆包豆包\n".as_bytes(), peer).await;
                    } else if flags == XPCM_FLAG_RESET {
                        saw_reset = true;
                        break;
                    }
                }
            }
            assert!(peer_addr.is_some());
            assert!(saw_pcm, "server should receive XPCM PCM packets");
            assert!(saw_reset, "server should receive XPCM_FLAG_RESET on start_stream");
        });

        for _ in 0..8 {
            audio_tx.send(make_chunk(300, false)).await.unwrap();
        }
        assert_eq!(handle.next_wake().await.unwrap(), BargeInSignal::Wake);
        // Verify duplicate seq=1 was suppressed.
        assert!(
            timeout(Duration::from_millis(50), handle.next_wake())
                .await
                .is_err(),
            "duplicate WAKE seq=1 should be deduplicated"
        );

        handle.start_oneshot_stream().await.unwrap();
        // Send live frame after starting stream.
        audio_tx.send(make_chunk(999, false)).await.unwrap();

        let mut rx = handle.active_rx.take().unwrap();
        let mut count = 0;
        let mut saw_999 = false;
        while let Ok(Some(c)) = timeout(Duration::from_millis(100), rx.recv()).await {
            count += 1;
            if i16::from_le_bytes([c.pcm[0], c.pcm[1]]) == 999 {
                saw_999 = true;
                break;
            }
        }
        assert!(count >= 2, "oneshot should replay pre-wake ring chunks + live frame");
        assert!(saw_999);
        srv_task.await.unwrap();
    }

    #[tokio::test]
    async fn capture_dual_utterance_preserves_speech_start_prefix() {
        let (tx, mut rx) = mpsc::channel(32);
        let cfg = CaptureConfig {
            sample_rate: 16_000,
            block_ms: 40,
            pre_roll_ms: 120,
            silence_ms: 120,
            min_speech_ms: 80,
            threshold: 0.1,
            cooldown_s: 0.0,
            ..CaptureConfig::default()
        };

        for _ in 0..2 {
            tx.send(make_chunk(100, false)).await.unwrap();
        }
        for _ in 0..4 {
            tx.send(make_chunk(12_000, false)).await.unwrap();
        }
        for _ in 0..4 {
            tx.send(make_chunk(0, false)).await.unwrap();
        }

        let captured = capture_dual_utterance(&mut rx, &cfg, Duration::from_secs(2), None)
            .await
            .unwrap()
            .expect("should capture utterance");
        assert!(captured.len() >= 9 * PCM_BYTES);
        let has_loud_head = captured
            .chunks_exact(PCM_BYTES)
            .take(4)
            .any(|blk| i16::from_le_bytes([blk[0], blk[1]]) == 12_000);
        assert!(has_loud_head, "SpeechStart trigger block was clipped!");
    }

    #[tokio::test]
    async fn oneshot_probe_returns_none_quickly_on_silence() {
        let (tx, mut rx) = mpsc::channel(16);
        let cfg = CaptureConfig {
            sample_rate: 16_000,
            block_ms: 40,
            threshold: 0.1,
            ..CaptureConfig::default()
        };
        tokio::spawn(async move {
            for _ in 0..10 {
                let _ = tx.send(make_chunk(50, false)).await;
                sleep(Duration::from_millis(20)).await;
            }
        });

        let start = Instant::now();
        let res = capture_dual_utterance(
            &mut rx,
            &cfg,
            Duration::from_secs(5),
            Some(Duration::from_millis(120)),
        )
        .await
        .unwrap();
        assert!(res.is_none());
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn session_tracker_advances_monotonically_and_checks_freshness() {
        let mut t1 = SessionTracker::new();
        let t2 = SessionTracker::new();
        assert!(t2.session_id > t1.session_id);

        let turn1 = t1.begin_turn();
        assert!(t1.is_turn_fresh(t1.session_id, turn1, Duration::from_secs(5)));
        assert!(!t1.is_turn_fresh(t2.session_id, turn1, Duration::from_secs(5)));

        sleep(Duration::from_millis(25)).await;
        assert!(!t1.is_fresh_for_enter_prompt(Duration::from_millis(10)));
        t1.reset_wake_window();
        assert!(t1.is_fresh_for_enter_prompt(Duration::from_secs(1)));
    }
}
