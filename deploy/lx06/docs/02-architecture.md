# 02 · 目标架构与接口规格

> 本文件定义"实现成什么样"。改动架构前先改 ADR。

---

## 1. 总体数据流（A 方案：有 `speech.usock`）

```
                 ┌──────────────── 设备（LX06, ARMv7） ────────────────┐
 麦克风 ──▶ 原生 VPM（AEC/NS/AGC）──▶ mipns（唤醒、回合控制）
                                                    │
                                                    │ 注册后发送 16k mono PCM
                                                    ▼
                                  [Agent speech.usock 服务端]
                                                    │
                                                    ▼
                                               [capture] 收 PCM
                 │                                  │
                 │                                  ▼
                 │                        [vad] 端点检测（本地）
                 │                                  │
                 │                                  ▼
                 │                        [asr] 外部服务（HTTP / Realtime WS）
                 │                                  │  文本
                 │                                  ▼
                 │                        [agent] LLM（OpenAI/Anthropic）+ 工具
                 │                                  │  回复文本
                 │                                  ▼
                 └──────────── [tts] tts_play.sh ◀── 文本
```

`speech.usock` 必须位于标准路径 `/tmp/mico_aivs_lab/usock/speech.usock`。Agent 在启动转换中绑定该路径；随后以 bind mount 固定该 socket，重启 `mico_aivs_lab` 以恢复其 `common.usock` 服务，并使用移除 `-r opus32` 的临时 PNS init 副本启动 mipns。任何一步失败必须撤销接管并恢复原生前端。

---

## 2. 进程与线程模型

单进程，tokio 多线程运行时：

| 任务 | 职责 | 阻塞点 |
|---|---|---|
| `main` | 解析配置、启动子任务、信号处理 | — |
| `capture` | 持续读 PCM，写入环形缓冲 | `read()` |
| `vad` | 端点检测，产出 `SpeechStart/SpeechEnd` | CPU 轻 |
| `wake` | 监听 PNS 事件 / KWS | 事件 |
| `session` | 状态机，串起 ASR→LLM→TTS | 网络 |
| `playback` | 播放与打断 | 系统 TTS / 媒体控制命令 |
| `watchdog` | 心跳、异常重启 | 定时 |

**约束**：任一子任务 panic 不得拖垮整进程；用 `JoinHandle` + 重启策略，最多连续重启 5 次后进入 fail-safe（只保留原生功能）。

---

## 3. 状态机

```
Idle ──wake──▶ Waking ──ack播放完──▶ Listening
                  │                     │
                  │                     ├─ VAD end ──▶ Thinking
                  │                     └─ 15s 超时 ──▶ Idle
                  │
Thinking ──LLM首token──▶ Speaking ──播放完──▶ DialogFinish ──pnshelper请求──▶ Listening
   │                        │
   └─ 30s 超时 ──▶ Idle      └─ 用户开口/唤醒 ──▶ BargeIn ──▶ Listening
```

| 状态 | 超时 | 行为 |
|---|---|---|
| Waking | 3s | 播放"在"，失败回 Idle |
| Listening | 15s 无语音 | 回 Idle |
| Thinking | 30s | 播报"网络有点问题"，回 Idle |
| Speaking | 按音频长度 + 5s | 可被 barge-in |
| 连续对话 | 3 轮无唤醒 | 必须重新唤醒 |

连续对话使用分段采集，不恢复原生 `dialog_continuous` 全双工标记。每段 TTS 结束后，等待 mipns 完成当前 DialogFinish 并回到 idle，再通过 `ubus call pnshelper event_notify '{"src":1,"event":0}'` 请求下一次采集。

---

## 4. 模块接口（Rust trait）

```rust
pub trait AudioSource: Send {
    /// 16k mono S16LE
    fn read(&mut self, buf: &mut [i16]) -> Result<usize>;
}

pub trait WakeSource: Send {
    fn wait(&mut self) -> Result<WakeEvent>;   // 阻塞式，带超时
}

pub trait Asr: Send + Sync {
    async fn transcribe(&self, pcm: &[i16], sample_rate: u32) -> Result<String>;
}

pub trait Llm: Send + Sync {
    async fn chat(&self, messages: &[Message], tools: &[Tool]) -> Result<Reply>;
}

pub trait Tts: Send + Sync {
    /// 文本转语音并播放；返回播放时长用于状态机
    async fn speak(&self, text: &str) -> Result<Duration>;
    fn stop(&self) -> Result<()>;
}
```

实现要求：
- `Asr` / `Llm` / `Tts` 必须可替换（配置驱动）。
- 所有网络调用带超时（连接 5s / 总 30s）与重试（1 次，指数退避）。
- `Tts::speak` 必须处理长文本切分（见 §6）。

---

## 5. 配置 schema（`/data/open-xiaoai/agent.yaml`）

运行时包提供 `templates/agent.yaml.lx06.example` 的副本作为唯一 LX06 配置模板。其字段与当前 Agent 的 `AppConfig` 一致：`runtime.speech_socket`、`runtime.session_idle_timeout_s`、`device.tts_command`、`capture`、`asr`、`llm`、`agent`、`mcp`、`music` 和 `airplay`。配置缺失的字段会采用 Agent 默认值。

```yaml
runtime:
  speech_socket: /tmp/mico_aivs_lab/usock/speech.usock
  session_idle_timeout_s: 20

device:
  # LX06 脚本无 shebang，只读取第一个位置参数。
  tts_command: "sh /usr/sbin/tts_play.sh {text}"

asr:
  provider: open_ai
  open_ai:
    base_url: https://api.openai.com/v1
    api_key: EMPTY
    model: whisper-1
    language: zh

llm:
  protocol: open_ai_chat
  base_url: https://api.openai.com/v1
  api_key: EMPTY
  model: gpt-5.4-mini

agent:
  timezone: Asia/Shanghai
```

`api_key` 是字面值，当前 Agent 不展开 `${ENV}`。复制模板后只在设备上填入密钥并设 `agent.yaml` 权限为 `600`，不得提交该文件。

---

## 6. TTS 文本处理

1. 去除 Markdown 标记（`**`、`#`、`-`、代码块）。
2. 按 `max_chars` 在标点处切分（句号/问号/感叹号/分号），逐段调用。
3. 单段仍超长 → 按逗号切；再超 → 硬切。
4. 全部失败 → 播放"回答太长了"。

在 LX06 1.94.13 中，`tts_play.sh` 无 shebang，只接收第一个位置参数；因此模板必须采用 `sh /usr/sbin/tts_play.sh {text}`，不能追加 `--notify-pns`。它经 `ubus call mibrain text_to_speech` 生成音频，再用 `miplayer` 播放，并由 `mphelper` 暂停/恢复当前媒体。当前 Agent 通过 `/bin/sh -c` 执行模板，T2A 前必须用含引号和换行的文本在真机验证模板替换不会改变命令边界。

---

## 7. 启动与自启

`/data/init.sh`（唯一入口）：

```sh
#!/bin/sh
[ -f /data/open-xiaoai/disabled ] && exit 0
export XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log
exec /data/open-xiaoai/start-agent.sh start
```

`start-agent.sh` 负责：
1. 停止 PNS 和 `mico_aivs_lab`，并清理上次未完成的接管；
2. 在 `/tmp` 制作 PNS init 的临时副本，移除 `-r opus32` 后 bind mount 覆盖原脚本；
3. 由 Agent 绑定标准 `speech.usock`，对该 socket 做自 bind mount 保护；
4. 重启 `mico_aivs_lab`，确认 `common.usock` 正常创建而受保护的 `speech.usock` inode 未变；
5. 启动 PNS 并确认使用 PCM；失败即执行完整回滚；
6. `stop`：停止 PNS、Agent 与 mico 服务，解除挂载、恢复原文件和原生服务。

**验收**：`start` 后 5s 内可唤醒，socket 保护挂载与 inode 均正常；`stop` 后原生小爱恢复应答。不能只凭 Agent 进程仍在运行判定接管成功。

---

## 8. 日志与可观测

- 结构化单行：`ts level mod event k=v ...`
- 必记事件：`wake`、`asr.done`（含耗时）、`llm.first_token`、`tts.start`、`tts.done`、`barge_in`、`error`
- 日志轮转：单文件 2MB，保留 3 份
- `xiaoai-agent status` 输出：状态机当前态、最近一次各阶段耗时、上游连通性

---

## 9. 资源预算（A113）

| 项 | 预算 |
|---|---|
| 常驻内存 | < 60 MB |
| 空闲 CPU | < 5% |
| 对话时峰值 CPU | < 40% |
| 端到端延迟 | 唤醒→首字 < 2.5s（不含 LLM 首 token） |

超预算要给出实测数据，不能估算。
