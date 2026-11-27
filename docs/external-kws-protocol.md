# 通用外部 KWS 协议规范（UDP XPCM）

在 `runtime.mode: dual` 下，`xiaoai-agent` 自身完成 ALSA 采集、下采样、硬件回采检测与 2.56s 唤醒连说环形缓冲，并通过极简 **UDP XPCM 协议** 把待机音频流发送给任意外部 KWS 服务（NAS / Docker / 树莓派 / PC / 路由器）。

本协议无任何平台或推理引擎绑定：只要实现下面的报文收发规范，任何语言（Python / C / Rust / Go）均可作为 `xiaoai-agent` 的外部唤醒后端。

- **Python / Docker 通用参考实现**：[`deploy/kws-server/python/`](../deploy/kws-server/python/README.md)
- **红米 AX5 路由器参考实现**：[`deploy/kws-server/ax5/`](../deploy/kws-server/ax5/README.md)

---

## 1. 上行报文（音箱 `xiaoai-agent` → 外部 KWS 服务 `dual.kws.endpoint`）

每个 UDP 数据报固定包含 **8 字节小端序包头（`XPCM`）** 与可选载荷：

```text
 0   1   2   3   4   5   6   7   8 ...
+---+---+---+---+---+---+---+---+-------------------+
| 'X' 'P' 'C' 'M' |  seq  | flags |  PCM payload      |
+---+---+---+---+---+---+---+---+-------------------+
  ASCII magic      u16 LE  u16 LE  16kHz mono S16_LE
```

### `flags` 定义

| 常量 | 值（`u16 LE`） | 方向 / 说明 |
|---|---|---|
| `XPCM_FLAG_PCM` | `0x0001` | 携带 40 ms（640 samples = **1280 字节**）16 kHz 单声道 `S16_LE` PCM |
| `XPCM_FLAG_RESET` | `0x0002` | 进入主动录音/对话轮次，通知服务端重置当前 KWS 流状态（无载荷） |
| `XPCM_FLAG_PING` | `0x0003` | 待机心跳与 `xiaoai-agent doctor` 连通性探测包（无载荷） |
| `XPCM_FLAG_SYS_ACTIVE` | `0x0100` | 可与 `XPCM_FLAG_PCM` 按位或：硬件回采表明音箱正在外放系统语音（如原生小爱应答/提示音），服务端应抑制误触发退出词 |

> **兼容约定**：老旧简版实现可按 `(flags & 0x00FF)` 取基础类型，按 `(flags & 0x0100) != 0` 取 `SYS_ACTIVE`。

---

## 2. 下行报文（外部 KWS 服务 → 音箱发送端源地址 `src_addr`）

服务端通过 **标准 UDP 原路回包**（直接 `sendto(..., src_addr)`），**不要**写死固定目标端口；这使单服务端可同时服务多台音箱，并允许 `xiaoai-agent doctor` 从临时端口并发探测。

| 报文内容（UTF-8 行文本） | 触发时机 | 音箱端行为 |
|---|---|---|
| `XPONG\n` | 收到 `XPCM_FLAG_PING` 时立即回复 | 刷新心跳时间戳；`doctor` 据此计算 UDP RTT |
| `WAKE:<seq>:<keyword>\n` | 命中唤醒词时回复（`<seq>` 为递增整数） | 立即打断当前输出、回放 pre-wake 缓冲尝试 one-shot 连说并进入对话；若 `<keyword>` 为 `__CMD_EXIT__`，仅在对话/播报中打断并退出（待机忽略） |

> **Wi-Fi 可靠性建议**：命中唤醒词后，服务端可在 20 ms 后重发一次相同 `seq` 的 `WAKE:<seq>:<keyword>\n`；`xiaoai-agent` 内置基于 `seq` 的 5 秒幂等去重。
>
> **职责边界**：原生“小爱同学”唤醒打断与防双播放完全由音箱端 `dual.native_interlock`（`NativeWakeMonitor`，详见 [architecture.md](architecture.md)）负责，外部 KWS 服务端**不要**添加“小爱同学”作为热词。

---

## 3. 音箱端配置示例（`agent.yaml`）

```yaml
runtime:
  mode: dual

dual:
  kws:
    endpoint: "192.168.31.1:5005" # 指向任意实现了本协议的服务地址
    ping_interval_s: 30
  capture:
    # 默认即 LX06 1.94.13（arecord -D noop 48kHz 8ch S32_LE）
    # 通用 16kHz 单麦设备可改为 sample_rate: 16000, channels: 1, format: S16_LE, ref_channel: null
    device: "noop"
    sample_rate: 48000
    channels: 8
    format: S32_LE
    mic_channel: 0
    ref_channel: 6
    shift_bits: 9
```

配置完成后在音箱执行一键探测：

```sh
/data/open-xiaoai/xiaoai-agent doctor -c /data/open-xiaoai/agent.yaml
```
