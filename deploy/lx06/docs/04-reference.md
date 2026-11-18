# 04 · 上游代码索引、关键原语、风险与回滚

> 实现时先在这里找"有没有现成的"，不要重复造。

---

## 1. 可复用上游（按文件）

### A 方案基线：`stevenjoezhang/xiaoai-agent`（Rust, LGPL-3.0）

| 文件 | 内容 | 怎么用 |
|---|---|---|
| `src/capture.rs` | 消费 `speech.rs` 交付的 16k mono PCM | 复用；它不是普通 Unix socket 客户端 |
| `src/speech.rs` | 标准 `speech.usock` 服务端、mipns 注册、回合控制与分段采集 | A 方案的协议核心，必须先在 LX06 真机验证 |
| `src/vad.rs` | 端点检测 | 复用；阈值按 LX06 实测调 |
| `src/asr.rs` | OpenAI 兼容 / Realtime transcription 两种 ASR | 复用；新增 provider 在此扩展 |
| `src/agent.rs` | LLM 调用 + 工具编排（Rig） | 复用；系统提示词改配置 |
| `src/mcp.rs` / `mcp_legacy_sse.rs` | MCP 客户端 | 需要 HA 控制时启用 |
| `src/music.rs` / `airplay.rs` | 音乐 / AirPlay | **本期禁用**（非目标） |
| `src/config.rs` | YAML 配置解析 | 复用；按本项目 schema 调整 |
| `src/main.rs` | 启动与信号 | 复用 |
| `xiaoai-agent/start-agent.sh` | 启停原生服务、PNS PCM 模式、socket bind-mount 保护、失败回滚与恢复 | **LX06 适配的主战场** |

### 设备控制原语：`coderzc/open-xiaoai`（Rust, MIT）

| 位置 | 命令 | 用途 |
|---|---|---|
| `client-rust/src/services/speaker.rs` | `ubus call pnshelper event_notify '{"src":1,"event":0}'` | 程序化唤醒 |
| 同上 | `{"src":3,"event":7}` / `{"src":3,"event":8}` | 开麦 / 关麦 |
| 同上 | `/usr/sbin/tts_play.sh '<文本>'` | 系统 TTS |
| 同上 | `ubus call mediaplayer player_play_url '{"url":"...","type":1}'` | 播放 URL |
| 同上 | `mphelper play` / `pause` / `mute_stat` | 播放控制 |
| 同上 | `/etc/init.d/mico_aivs_lab restart` | 掐断原生应答 |
| 同上 | `micocfg_model` / `micocfg_sn` | 型号 / SN |
| `client-rust/src/services/monitor/kws.rs` | `tail /tmp/open-xiaoai/kws.log` | 原生唤醒事件格式 `<ts>@<kw>` |
| `client-rust/src/services/monitor/instruction.rs` | `tail /tmp/mico_aivs_lab/instruction.log` | 原生 ASR/NLU 事件（`is_final`、`text`） |

### 音频前端与端侧 KWS 参考：`kslr/xiaoai-plus`（C++, MIT，已停更）与 `rolazd/xiaoai-doubao`（下游工程重构，2026-11）

> **2026-11-18 复核警示**：`rolazd/xiaoai-doubao` 在 OH2P 1.62.2 真机上已证实 `kslr/xiaoai-plus` 的 `aec_webrtc.cpp`（无硬件帧对齐延迟的 `set_stream_delay_ms(0)`）**不能支持可用回声消除**，生产默认切回半双工（`duplex_mode = half`）。其仓库新增的 `ordinary_barge_capture` + `shadow_webrtc.cpp`（`-2..90ms` 延迟搜索 + 128-tap NLMS）亦默认 `off`，且源码注释明确标注尚未通过真实房间与儿童声学验收。因此 Phase 3 **禁止假设"照抄 `aec_webrtc.cpp` 即可做全双工打断"**。

| 来源与文件 | 可参考什么 / 踩坑结论 |
|---|---|
| `xiaoai-doubao`: `toolchain/sherpa-kws-observability.patch` | 必须关闭 ONNX Runtime 线程自旋（`session.intra_op/inter_op.allow_spinning = "0"`），否则在 A113 上空闲 CPU 飙至约 270%；修复后单核均值约 80%（`kws_num_threads = 2`，单线程回放存在漏命中） |
| `xiaoai-doubao`: `src/audio/kws_diagnostics.cpp` | `kslr` 硬编码 `in[i] >> 8`（假定低 24 位）；`xiaoai-doubao` 默认改为 `s32_shift = 16` 并检查 `outside_signed_24` / `low_byte_zero`。LX06 若直采 ALSA，必须先实测裸 PCM 比特对齐 |
| `xiaoai-doubao`: `src/audio/kws_preprocess.cpp` | 10 ms 帧 RMS AGC（1.0–15.85x，约 0–24 dB）**仅用于 KWS 支路**，不污染主上行 ASR 音频 |
| `xiaoai-doubao`: `src/wakeup/{dual_wake_decider,kws_zipformer,local_listener}.cpp` + `exit_echo_guard.hpp` | 前缀与全词合并防抖（1100 ms / 1400 ms）、`CreateStream(lines)` 仅在 `kActive` 启用退出词、结合最近 TTS 文本 + 唤醒后 2500 ms 抑制自回声误退出 |
| `xiaoai-doubao`: `src/app/native_wake_monitor.cpp` + `src/native_speech/source.cpp` | 监听日志让位原生唤醒；源码另含不开 ALSA/KWS、直接消费 `/tmp/mico_aivs_lab/usock/speech.usock` 与 `pnshelper` 的 `XIAOAI_NATIVE_ONLY` 分支（但 `v0.1.0` 发布包仍走 `xiaoai_plus_speaker` 以保留"豆包豆包"唤醒词） |
| `xiaoai-plus`: `src/app/app.cpp::DownmixToMono` / `assets/keywords.txt` | 仅作 3 麦互相关参考；实机通道顺序与字节移位必须按 LX06 `asound.conf` 实测，词表生成仍可参考 `sherpa-onnx-cli text2token`（注：`xiaoai-doubao` 使用 `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20` `chunk-8-left-64.int8`） |

### 刷机与安全流程：`Liu-zhongxian/zongzhu-bot` + `coderzc/open-xiaoai`

- 官方 OTA 查询（`ota_query.py`，改 `MODEL` 即可）
- 只读备份 → 只写非活动槽 → 读回 cmp → 切 boot 的脚本编排
- `patches/core/` 的 4 个通用补丁；它们是标准 `speech.usock` 接管镜像的唯一批准补丁集

---

## 2. LX06 关键事实（访谈+代码推导，需实测确认）

| 事实 | 值 | 来源 |
|---|---|---|
| SoC | Amlogic A113 系，ARMv7 hard-float | xiaoai-plus 注释、open-xiaoai 编译目标 |
| 采集设备 | `hw:0,3`，**8 声道 S32_LE 48kHz** | LX06 `asound.conf` 补丁上下文 |
| 厂商 4 通道视图 | `pcm.mico_record`，`ttable` 取物理 4/2/0/6 → 4 路 | 同上 |
| 补丁后 | `noop` = `plug→Capture(dsnoop, ipc_perm 0666)`，8ch 直通 | 同上 |
| 固件版本线 | 1.94.x | 上游 release 标签 `LX06_1.94.13` |
| 本期批准补丁 | 通用 4（`patches/core/`） | 预期改动见 `scripts/expected-changes-lx06.txt`，不含任何 ALSA 补丁 |

> 待实测项：标准 socket 与 `common.usock` 是否存在、mipns 注册协议和 PCM 模式是否兼容、PNS 进程名，以及 `start`/`stop` 后原生服务是否可恢复。不要以 OH2P 的音频通道或 `/var/run` 路径推断 LX06。

---

## 3. 风险清单

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| LX06 无兼容的 `speech.usock` | 中 | 本期端侧不可行 | 停止端侧实施，转 bridge |
| socket 被原生服务重新绑定 | 中 | Agent 失去音频且原生服务异常 | 按主项目以 bind mount 保护，监控 inode 与挂载状态 |
| 恢复流程不完整 | 中 | 原生语音无法恢复 | 每次探针都执行并验证 `stop`；失败立即回滚 |
| 原生旁路不可逆 | 低 | 小爱永久失声 | `start-agent.sh stop` 必须验证 |
| 固件升级冲掉补丁 | 中 | 功能消失 | 关闭 OTA；记录版本 |
| 云端 ASR/LLM 涨价或断服 | 中 | 不可用 | provider 抽象，可换 |
| 设备算力不足导致卡顿 | 低 | 体验差 | 不在设备跑模型；监控 CPU |
| LGPL 合规 | 低 | 分发受限 | 修改后分发需开源 |

---

## 4. 回滚矩阵

| 场景 | 操作 | 预期 |
|---|---|---|
| 只想停 Agent | `start-agent.sh stop` | 原生小爱恢复 |
| 启动失败循环 | 删 `/data/init.sh` 里的启动行 | 设备回到原生 |
| 语音完全异常 | `touch /data/open-xiaoai/disabled` + 重启 | 不启动 Agent |
| 系统坏了 | `fw_env -s boot_part boot<另一边>` + 重启 | 回官方系统 |
| SSH 不通 | 刷机模式 `bulkcmd setenv boot_part ...` + `saveenv` | 同上 |

---

## 5. 背景：为什么不是别的方案（一句话版）

| 方案 | 否决理由 |
|---|---|
| xiaogpt / MiGPT | 必须"小爱同学+关键词"，延迟 2–5s，抢话无解 |
| open-xiaoai-bridge | 需要常开后端；与"无后端"冲突（保留为 C 方案兜底） |
| xiaoai-plus | 端到端豆包模型不可替换；已停更；下游实证其 WebRTC AEC 全双工在 OH2P 未跑通 |
| zongzhu-bot | 面向 OH2P，LX06 需改 5 处；单人新项目 |
| xiaoai-gpt | 只接 Codex、macOS 专用、非商业许可 |
| xiaoai-doubao | 仅 OH2P 1.62.2，锁豆包 O2；在 `xiaoai-plus` 上重构为半双工+KWS工程修复（修复 ONNX 自旋与移位对齐），另含未默认发布的 `speech.usock` 分支 |

完整调研见 `docs/reference/research-2026-11.md`。
