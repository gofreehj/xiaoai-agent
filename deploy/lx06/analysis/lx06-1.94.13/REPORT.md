# LX06 1.94.13 官方 OTA 静态分析

## 结论

该文件是 LX06 1.94.13 的完整 Xiaomi `HDR1` OTA，且其 rootfs 中存在本项目 A 方案所需的 PNS、AIVS、TTS 和媒体控制组件。静态证据支持进入真机 T2A 受控接管探针；它不证明 socket 协议已经在 LX06 上互通。

## 原件身份与完整性

| 项目 | 值 |
|---|---|
| OTA | `mico_all_911151b95_1.94.13.bin` |
| 容器 | `HDR1` |
| 设备 | `LX06` |
| ROM | `1.94.13`，release |
| 构建时间 | 2025-09-17 11:43:57 +0800 |
| OTA SHA-256 | `a56a38619fd98e467adb8c998757f80109c4dac89db48d62dae985e3062476ed` |
| OTA MD5 | `b202de2bbe149ce884aa500911151b95` |
| rootfs | SquashFS 4.0，XZ，131072-byte block，37,500,699 bytes |

`mico_version`、每个 segment 的偏移、长度与散列见 [ota-manifest.json](ota-manifest.json)。原始 OTA 未被修改；分析副本位于 `extracted/` 与 `rootfs/`。

## 运行时证据

| 组件 | 官方路径 | 静态证据 | 含义 |
|---|---|---|---|
| PNS | `/usr/bin/mipns-xiaomi` | 包含 `/tmp/mico_aivs_lab/usock/speech.usock`、`RegisterRequestMessage`、`StreamTransmittingMessage` 和 PCM 符号 | 标准 speech usock 路径和注册/流协议存在 |
| PNS init | `/etc/init.d/pns` | Xiaomi 分支启动带 `-r opus32 -l` | 接管探针可验证去除 `-r opus32` 的 PCM 模式 |
| AIVS | `/usr/bin/mico_aivs_lab` | 包含 `pnshelper`、`event_notify`、`Dialog::notifyPnsOpenMic`、`/data/mipns/dialog_continuous` | AIVS 负责原生对话状态与请求下一轮采集 |
| PNS helper | `/usr/bin/pns_ubus_helper` | 暴露 `event_notify`，处理连续对话状态 | 可用于分段连续对话 |
| TTS | `/usr/sbin/tts_play.sh` | 仅接受第一个位置参数，调用 `mibrain text_to_speech` 和 `miplayer` | Agent 应以单个 argv 文本参数调用 |
| 媒体控制 | `/usr/bin/mphelper` | 被 `tts_play.sh` 调用 | 播报前后会暂停/恢复媒体 |

## 已确认的启动顺序

`pns_ubus_helper` 的 `START=54`，`pns` 的 `START=55`，`mico_aivs_lab` 的 `START=85`。接管顺序应先处理 PNS，再在 Agent 已绑定标准 `speech.usock` 后重启 AIVS，以恢复其余服务端资源；最终必须执行完整 stop 回滚。

## 对候选固件构建的限制

现有 `deploy/client-patch/src/squashfs.sh` 只允许 `OH2P`，且硬编码其 rootfs 上限。因此它不能用于构建 LX06 产物，直到基于真机分区表确认 LX06 的目标槽位和最大 rootfs 大小。

当前发布链固定使用 `deploy/lx06/patches/core/` 的 core-4 补丁集。它不修改 `mico_record`、`noop`、`libxaudio_engine.so` 或硬件采集路径，语音运行时经原生 `speech.usock` 接管。

## T2A 前仍需真机确认

1. `speech.usock` 与 `common.usock` 的实际 socket 类型、owner、inode 和 AIVS 重启行为。
2. Agent 与 mipns 的 server/client 角色、注册消息、PCM 参数及首次帧。
3. 不含 `-r opus32` 的 PNS 能否稳定启动。
4. `start-agent.sh stop` 能否解除 socket 保护、恢复原 init 脚本并让原生小爱回应。
