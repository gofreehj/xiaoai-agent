# LX06 端侧本地助手：不经后端的实现方案

> 目标：刷机后让 LX06 **自己完成语音闭环**（唤醒 → 识别 → 大模型 → 说话），不依赖家里常开的电脑/NAS。
> 结论：不需要移植 xiaoai-plus，用 `coderzc/open-xiaoai` 客户端已有的原语就能拼出来。

---

## 1. 先厘清"本地"的三种含义

| 说法 | 含义 | 能否做到 |
|---|---|---|
| **无中转** | 不要局域网里那台常开的后端 | ✅ 本文方案 |
| **无云** | 语音识别/合成/模型都在设备上 | ❌ A113 跑不动 ASR+LLM |
| **无账号** | 不登录小米账号 | ✅ 刷机路线本来就不登录 |

下面所有设计都只承诺第一条。

---

## 2. 关键发现：设备上已经有一套"免费"的 ASR

`coderzc/open-xiaoai` 的客户端里，两个文件暴露了原生系统的能力：

**① `/tmp/open-xiaoai/kws.log`** —— 原生唤醒事件
```rust
// services/monitor/kws.rs
pub static KWS_FILE_PATH: &str = "/tmp/open-xiaoai/kws.log";
// 行格式： <timestamp>@<keyword>，keyword == "__STARTED__" 表示引擎就绪
```

**② `/tmp/mico_aivs_lab/instruction.log`** —— 原生 NLU/ASR 事件流
```rust
// services/monitor/instruction.rs
static INSTRUCTION_FILE_PATH: &str = "/tmp/mico_aivs_lab/instruction.log";
Payload::RecognizeResultPayload { is_final, is_vad_begin, results: [{ text, confidence, ... }] }
Payload::SpeakPayload { text }
Payload::PlayPayload { ... }
Payload::SetPropertyPayload { name, value }   // 音量等
```

也就是说：**只要原生小爱被唤醒，它的云端 ASR 就会把识别文本（含中间结果 is_final=false）写进这个日志。** 我们的程序 tail 这个文件，就能零成本拿到 ASR——不用在设备上跑 SenseVoice，也不用买云 ASR。

**③ 程序化唤醒**：`services/speaker.rs` 里已经验证过的 ubus 调用
```sh
ubus call pnshelper event_notify '{"src":1,"event":0}'    # 唤醒，等价于喊"小爱同学"
ubus call pnshelper event_notify '{"src":3,"event":7}'    # 开麦
ubus call pnshelper event_notify '{"src":3,"event":8}'    # 关麦
/etc/init.d/mico_aivs_lab restart                        # 掐掉原生回答（abort_xiaoai）
/usr/sbin/tts_play.sh '要说的文本'                        # 原生 TTS 直接播
ubus call mibrain ai_service '{"tts":1,"nlp":1,"nlp_text":"..."}'
```

**这就把"自定义唤醒词"和"复用原生 ASR"这两件看似矛盾的事解开了**：用自己的 KWS 监听 → 命中后调 `pnshelper` 把原生管线"骗"起来 → 白嫖它的云 ASR。

---

## 3. 三条可选架构

### 方案 A：寄生原生（改动最小，1–2 天）

```
原生 kws.log（"小爱同学"）
      │
      ├─ 读 instruction.log 的 is_final 文本
      ↓
  你的 LLM（HTTPS，OpenAI 兼容）
      ↓
  tts_play.sh（原生音色）或 云 TTS → aplay
```

- 不需要麦克风、不需要模型、不需要后端；纯 Rust + HTTP。
- 本质是"设备内版 MiGPT"：省掉云端轮询，延迟从 1–2s 降到几十毫秒。
- **缺点**：仍要喊"小爱同学"。

### 方案 B：自建唤醒词 + 原生 ASR（推荐，3–5 天）

```
自建 KWS（sherpa-onnx zipformer，~10MB）
      │ 命中
      ├─ pnshelper event_notify src:1 event:0   ← 程序化唤醒
      ├─ 原生云 ASR 出文本（instruction.log）
      ├─ abort_xiaoai()                         ← 掐掉原生回答
      ↓
  你的 LLM → TTS → 播放
```

- **自定义唤醒词**（"你好宗主""豆包豆包"随便设），**ASR 仍然是原生的**，不吃设备算力。
- 新增工作量：KWS 常驻 + 状态机 + 打断。
- 这是"无后端 + 自定义唤醒词 + 自定义模型"三者的唯一交点。

### 方案 C：xiaoai-plus 式端侧实时（1–2 周，不推荐）

```
4ch 采集 → WebRTC AEC/NS/AGC → KWS → 云实时语音 API（豆包/OpenAI Realtime）→ 播放
```

- 体验标注为“全双工”主要来自 `kslr/xiaoai-plus` 宣传，**模型被实时语音 API 锁死**，与"自定义大模型"直接冲突。
- 只有接受"就用豆包"时才值得。

> **2026-11-18 复核注记**：后续 `rolazd/xiaoai-doubao` 在 OH2P 真机实践表明，`kslr/xiaoai-plus` 的 `aec_webrtc.cpp` 并未跑通可用全双工（实际退回 `duplex_mode = half` 半双工）。若未来评估方案 B/C 自建端侧 KWS，不可假设直接复用 `aec_webrtc.cpp` 即可“播放中打断”；且接入 `sherpa-onnx` 必须解决 `allow_spinning=0`（防 ~270% CPU 空转）、`S32_LE` 位移实测对齐与 KWS 独立 AGC。

---

## 4. 复用清单（谁提供什么）

| 能力 | 来源 | 文件 | 说明 |
|---|---|---|---|
| 音频采集 | open-xiaoai client-rust | `services/audio/record.rs` | `arecord` 单声道 16k，需从"RPC 触发"改成"本地常开" |
| 音频播放 | 同上 | `services/audio/play.rs` | `aplay` 推 PCM |
| 原生事件 | 同上 | `monitor/{kws,instruction,playing}.rs` | 三个 log tail，直接可用 |
| 设备控制 | 同上 | `services/speaker.rs` | tts/音量/唤醒/掐断/播放控制，全部现成 |
| 唤醒词模型 | sherpa-onnx | zipformer KWS | 与 bridge 用的是同一套 |
| VAD / 断句 | open-xiaoai-bridge 的思路 | `vad.min_silence_duration` | 若用原生 ASR，VAD 由原生做，可省 |
| 对话/路由逻辑 | open-xiaoai-bridge | `before_wakeup` 状态机 | 翻译成 Rust 即可 |
| AEC/NS/AGC | xiaoai-plus / xiaoai-doubao | `src/audio/aec_webrtc.cpp` / `ordinary_barge_capture.cpp` | 仅历史参考；OH2P 实证 `aec_webrtc.cpp` 未跑通全双工，`ordinary_barge` 默认关闭且未过声学验收 |
| 4ch 拆分 | xiaoai-plus | `app.cpp::DownmixToMono` | LX06 需按 `ttable` 重排，见 §5 |

---

## 5. LX06 特有的两个坑

**① 4 声道语义不同。** xiaoai-plus 写死"ch0–2 是麦、ch3 是回采"，而 LX06 的 `/etc/asound.conf` 里 `mico_record` 用 `ttable` 从 8 路物理通道挑 4 路（4/2/0/6），**顺序和 OH2P 不同**。要做 AEC 就必须先 `arecord -D mico_record -c 4 -f S32_LE -r 48000` 录一段，逐通道看波形，确认哪一路是回采。

**② ALSA 共享。** 原生小爱一直在占用麦克风。`open-xiaoai` 的 LX06 补丁把 `noop` 改成 `plug→Capture(dsnoop, ipc_perm 0666)`，就是为了让第三方进程能**共享采集**。方案 B/C 依赖这一点，刷机时必须打上该补丁（`patches/LX06/01-asound.patch`）。

---

## 6. 方案 B 的实现骨架（Rust，伪代码）

```rust
// 1. 常开采集（自建 KWS 用）
AudioRecorder::start(|pcm| { kws.accept(pcm) });   // 16k mono

// 2. 唤醒
if kws.hit(keyword) {
    Speaker::wake_up(true).await;                  // pnshelper src:1 event:0
    state = Listening;
}

// 3. 读原生 ASR
InstructionMonitor::start(|ev| match ev {
    RecognizeResult { is_final: true, text } => {
        Speaker::abort_xiaoai().await;             // 掐掉原生回答
        let reply = llm.chat(&text).await;         // OpenAI 兼容
        Speaker::play_text(&reply).await;          // tts_play.sh
    }
    _ => {}
});

// 4. 超时/退出
if idle_for(30s) { Speaker::wake_up(false).await; state = Idle; }
```

**关键点**：`abort_xiaoai()` 必须在 `is_final` 到达后**立刻**调用，否则原生会先开口。这是整个方案最容易翻车的地方，建议加 150–300ms 的"抢跑窗口"实测。

---

## 7. 工作量与风险

| 阶段 | 内容 | 时间 | 风险 |
|---|---|---|---|
| P0 | 刷机 + SSH + 确认三个 log 文件在本机存在且格式一致 | 半天 | LX06 固件版本差异（1.94.x vs OH2P 1.62.x） |
| P1 | 方案 A 跑通（读 ASR → LLM → tts_play） | 1–2 天 | `tts_play.sh` 有长度/字符限制 |
| P2 | 方案 B：接自建 KWS + 程序化唤醒 | 3–5 天 | KWS 误唤醒；唤醒与原生抢麦 |
| P3 | 打断 / AEC / 4ch 回采 | 1–2 周 | LX06 通道语义未验证，最大不确定性 |
| P4 | 开机自启 / 看门狗 / 断电恢复 | 1–2 天 | init 脚本与原生服务的启动顺序 |

**不建议做的事**：在设备上跑本地 ASR 或本地 LLM。A113 的算力和内存都不够，会把延迟拖到不可用。

---

## 8. 验收标准

1. 断电重启后，不依赖任何电脑，喊唤醒词→回答→播放，全链路自通。
2. `ps` 里只有你自己的进程，没有连 4399 的连接。
3. 断网时：原生小爱仍可用；你的助手给出"连不上"的语音提示而不是卡死。
4. 连续 3 轮对话不需重复唤醒；说"退出"能回到原生。
5. 原生小爱放歌时，你的助手不误触发（KWS 阈值 + 播放态判断）。

---

## 8.5 更优起点：stevenjoezhang/xiaoai-agent（2026-11 补充）

调研后发现的同目标项目，**比本方案第 6 节的骨架更完整**，建议优先评估移植它，而不是自研：

| | xiaoai-agent | 本文方案 B |
|---|---|---|
| 音频来源 | 原生 `speech.usock`（16k 单声道 PCM，VPM 已做 AEC/NS/AGC） | `arecord` + 自己处理 |
| 唤醒 | 原生 `mipns-*` 语音前端 | 自建 sherpa KWS |
| ASR | 外部服务（OpenAI 兼容 HTTP / Realtime transcription） | 原生 `instruction.log` |
| LLM | `open_ai_chat` / `anthropic`（Rig 框架） | 自己写 |
| 工具 | MCP（含 HA-MCP）、天气、Navidrome、AirPlay | 无 |
| 后端 | **无** | **无** |
| 兼容性 | 仅 OH2P 1.62.2 | 未验证 |
| 许可 | LGPL-3.0 | — |

它的关键设计（也是它敢说"不抢麦、不抢答、不触发小米云端控制"的原因）：

1. 用原生 PNS 的唤醒事件，不用自己跑 KWS；
2. 把原生麦克风**静音**，Agent 独占真实麦克风音频（所以不会和原生抢）；
3. 走 `speech.usock` 拿已经过 VPM 处理的 PCM，回声消除/降噪由固件负责；
4. 回复用系统 TTS 命令朗读。

**代价**：原生小爱变成"聋子"——语音控制家电失效，只能靠 HA-MCP 补回来。这与 `open-xiaoai-bridge` 的"双助手共存"是互斥的产品选择。

> **补充判断（2026-11）**：一旦选择"完全接管"，**自定义唤醒词就不再是刚需**。
> 自定义唤醒词原本要解决的两个问题——抢答、打断——在接管模式下已经由架构消除：原生对话链路被旁路，麦克风归 Agent，打断由自己的 VAD/ASR 处理。
> 此时继续用"小爱同学"当唤醒词反而更优：远场唤醒是小米调过多轮的，比自塞一个 10MB zipformer 稳。
> 仍值得自定义的只剩两种情况：家里有多台小爱（喊一句会串台）、或想要人格化命名。
>
> **真正的代价不是唤醒词，而是原生技能**：开灯、放歌、闹钟、米家场景全部失效，要用 MCP 工具重做一遍（HA-MCP 能覆盖大部分，音乐换成 Navidrome，闹钟要自己写）。

**移植到 LX06 的关键假设**（作者本人也这么写）：「只要目标固件提供兼容的 `mipns`/`speech.usock` 音频接口，同一套 Rust 程序就有机会直接复用，设备差异主要由启动脚本吸收。」

所以 P0 验证只需要回答两个问题：

```sh
ps | grep -i mipns                 # 有没有原生语音前端进程
ls -l /var/run/*.usock /tmp/*.usock 2>/dev/null   # 有没有 speech.usock
```

两个都有 → 改 `start-agent.sh` 即可；只有一个或都没有 → 回到自研路线。

---

## 9. 一句话总结

> xiaoai-plus 的价值不在于"它的代码"，而在于它证明了**端侧可以自己跑完整闭环**。
> 而在 LX06 上，你不需要它的 ASR/AEC —— 原生的 ASR 已经躺在 `instruction.log` 里，`pnshelper` 还能被你程序化唤醒。**把这两件事接起来，就是一个不经过后端的本地助手。**
