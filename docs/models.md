# 机型支持矩阵与适配前提

本仓库的 `xiaoai-agent` 在 `exclusive` 模式下接管小米音箱固件里**原生语音前端**（`mipns-*`）送出的音频，在 `dual` 模式下通过 `arecord` + 轻量中继与原生小爱共存。不同机型的硬件与固件脚本存在差异，换机型前请先按本文核对。

图例：✅ 本项目实测可用 ｜ ⚠️ 有线索但本项目未验证 ｜ ❓ 完全未验证

| 机型 | 固件 | 状态 | 支持模式 | 说明 |
|---|---|---|---|---|
| **LX06**（小爱音箱 Pro） | `1.94.13` | ✅ | `exclusive` / `dual` | 完整真机验证。`tts_play.sh` 无 shebang 且不接受 `--notify-pns`，需用 `sh /usr/sbin/tts_play.sh {text}`；`dual` 模式单二进制内置采集与 [UDP XPCM 客户端](external-kws-protocol.md)，可配合任意外部 KWS 服务运行。详见 [`deploy/lx06/README.md`](../deploy/lx06/README.md) 与 [`deploy/kws-server/`](../deploy/kws-server/python/README.md) |
| **OH2P**（小米智能音箱 Pro） | `1.62.2` | ✅ | `exclusive` | 完整真机验证。支持 `tts_play.sh --notify-pns`，外部 TTS 播放启动宽限实测 ~3.2s。详见 [`deploy/OH2P_1.62.2_BUILD_NOTES.md`](../deploy/OH2P_1.62.2_BUILD_NOTES.md) |
| L06A | — | ⚠️ | — | 社区反馈：`tts_play.sh` **不认** `--notify-pns`，需改成 `/usr/sbin/tts_play.sh {text}`（见 [`device-scripts.md`](device-scripts.md)）。未在本项目验证 |
| LX01 / LX04 / LX05 / LX07 / X08A 等 | — | ❓ | — | 无任何验证，也没有社区数据 |

> ⚠️ **不要把「能刷 open-xiaoai 官方客户端」当成「能用本 agent」**：官方客户端自己抓麦克风、自己播 PCM，不依赖 `speech.usock`；本 agent 的 `exclusive` 模式依赖固件原生语音前端。

---

## 已知机型硬件与固件差异

| 差异点 | LX06（1.94.13） | OH2P（1.62.2） |
|---|---|---|
| SoC / ABI | Amlogic A113，ARMv7 hard-float，glibc 2.25 | ARMv7 hard-float，glibc 2.25 |
| `speech.usock` 协议 | ✅ 支持（P0 + T2A 真机验证通过） | ✅ 支持 |
| `tts_play.sh` | 无 shebang，仅收第 1 参数，**不支持** `--notify-pns` → 用 `sh /usr/sbin/tts_play.sh {text}` | 支持 `--notify-pns` |
| ALSA 原始采集设备 | `hw:0,3`，8 声道 `S32_LE` 48 kHz（`dual` 模式经 `dsnoop`/`noop` 采集） | — |
| 分区与刷机约束 | A/B 双槽，分区尺寸必须以设备真实 `/proc/mtd` 为准，**严禁沿用 OH2P 分区尺寸** | 固件大小上限 `0x02800000` |
| `/data/mipns/dialog_continuous` | 刻意禁用（分段采集） | 刻意禁用（分段采集） |

---

## 6 个适配前提（换机型时逐条自查）

```sh
# 1) 用户态 ABI：需要 armv7 hard-float + glibc >= 2.25
uname -m                       # 内核可能报 aarch64，但用户态是 32 位 armhf
ldd --version | head -1        # 必须 >= 2.25，否则二进制报 GLIBC_2.28 之类缺失

# 2) 原生语音前端存在，且麦克风是它的（exclusive 模式必需）
ls -l /tmp/mico_aivs_lab/usock/          # 要有 speech.usock 与 common.usock
ps | grep -E "[m]ipns|[m]ico_aivs"       # mipns-* 必须是真麦克风的唯一读者

# 3) 送出来的是 PCM 而不是 Opus（start-agent.sh 会自动改，手工接管时要自己确认）
tr '\0' '\n' < "/proc/$(pidof mipns-xiaomi)/cmdline" | grep -n "opus32"
#   有输出 = 还在送 Opus；必须去掉 -r opus32，agent 才会收到 16k/mono/S16_LE PCM

# 4) pnshelper 的 ubus 事件存在（连续对话靠它请求下一次采集）
ubus -v list pnshelper | head

# 5) TTS 脚本的参数差异
head -30 /usr/sbin/tts_play.sh | grep -n "notify-pns\|usage\|getopts"

# 6) 有没有可用的块设备/网络文件系统（想做本地曲库时）
ls /proc/filesystems | grep -E "cifs|nfs"   # 大多数机型没有 → 不能直接挂 NAS
```

六条全过才谈「能跑」；任何一条不成立，就不是配置问题，是这个机型不适配。

## 新增机型的正确做法

1. 先只跑 [`flash-troubleshooting.md`](flash-troubleshooting.md) 里的只读自查，**不要**先装 agent；
2. 六条前提里 1/2/3/4 都过了，再考虑接管；
3. 接管前准备好回滚（`sh start-agent.sh stop`），并在本文件补一行「机型 / 固件 / 验证到哪一步」；
4. 只过了部分前提的，如实写「未验证」，不要写成「支持」。
