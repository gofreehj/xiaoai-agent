# 小爱音箱 Pro（LX06）固件补丁与部署指南

> **适用机型**：小爱音箱 Pro（**LX06**，Amlogic A113，ARMv7 hard-float，A/B 双系统）  
> **已验证固件**：官方固件 `1.94.13`  
> **架构与运行原理**：统一沉淀于根目录 **[架构与交互设计](../../docs/architecture.md)**，本文仅保留 LX06 专属的刷机、构建与回滚索引。

---

## 文档导航

| 文档 | 用途 | 适用人群 |
|---|---|---|
| **[`FLASH_GUIDE.md`](./FLASH_GUIDE.md)** | 保姆级拆机、刷机、配网与部署指南（免编译、防变砖） | 普通用户 |
| **[`docs/01-verify.md`](./docs/01-verify.md)** | 完整手动验证手册：取官方包 → 6 分区备份 → 打补丁 → 刷写 → P0/T2A 验收 → 回滚 | 开发者 / 自行打包 |
| **[`../kws-server/python/README.md`](../kws-server/python/README.md)** | `dual` 通用外部 KWS 服务端参考（NAS / Docker / 树莓派 / PC） | 希望保留原生小爱 + 自由部署唤醒端 |
| **[`../kws-server/ax5/README.md`](../kws-server/ax5/README.md)** | `dual` 路由器参考实现：红米 AX5 Sherpa-ONNX 3.3M KWS 服务 | 使用 AX5 路由器作为外部 KWS |
| **[`../../docs/external-kws-protocol.md`](../../docs/external-kws-protocol.md)** | 通用外部 KWS 协议规范（UDP XPCM），方便自建任意外部唤醒服务 | 开发者 |
| **[`../../docs/architecture.md`](../../docs/architecture.md)** | `xiaoai-agent` 统一模块化架构、双模式数据流、热重载与自检说明 | 开发者 |
| **[`UPSTREAM.md`](./UPSTREAM.md)** | 与上游及 fork 同步边界与步骤 | 维护者 |
| [`docs/log/`](./docs/log/) & [`analysis/lx06-1.94.13/REPORT.md`](./analysis/lx06-1.94.13/REPORT.md) | LX06 1.94.13 真机 P0/T2A 实测证据与固件结构分析 | 历史证据存档 |

---

## 两种运行模式速览

- **`exclusive`（独占接管模式）**：直接复用 LX06 原生 `mipns` 唤醒与回声消除，原生小爱静默不抢答，无需额外外部 KWS 服务。使用配置模板 [`templates/agent.yaml.lx06.example`](./templates/agent.yaml.lx06.example)。
- **`dual`（外部 KWS 双助手共存模式）**：原生小爱保留日常功能（闹钟、倒计时、米家），音箱端同一个 `xiaoai-agent` 二进制内置采集并转发给任意外部 KWS 服务（NAS / Docker / PC / [红米 AX5](../kws-server/ax5/README.md)）触发大模型对话，只需在 `agent.yaml` 中设置 `runtime.mode: dual` 与 `dual.kws.endpoint`。

部署完成后可在音箱端执行一键自检：

```sh
/data/open-xiaoai/xiaoai-agent doctor -c /data/open-xiaoai/agent.yaml
```

---

## 刷机与运维硬红线

1. **只写非活动槽**：仅允许写入 `boot0/1`、`system0/1`，**永不写** `bootloader` / `tpl` / `data`。
2. **先备份再刷写**：刷机前必须只读备份全部 6 个分区；分区长度必须来自设备真实 `/proc/mtd`，**严禁沿用 OH2P 分区尺寸**。
3. **逐字节校验**：每次写入后必须读回比对，不一致立即停止，绝不切启动槽位；内核与系统镜像必须来自同一官方版本。
4. **禁止硬打补丁**：`patches/core/` 4 个补丁必须 `--fuzz=0` 应用，失败即停止排查。
5. **不要在音箱端跑本地 ASR/LLM**：A113 算力与内存有限，ASR/LLM 走外置 API，自定义 KWS 走路由器中继。
6. **配置与密钥安全**：`agent.yaml` 仅在设备上填入 API Key 并设权限 `600`，绝不提交到仓库；`device.tts_command` 必须是 `sh /usr/sbin/tts_play.sh {text}`（不要在 `{text}` 外手动加单引号，也不要加 `--notify-pns`）。

---

## 快速回滚矩阵

| 场景 | 操作 | 预期结果 |
|---|---|---|
| 临时停用 Agent | `/data/open-xiaoai/start-agent.sh stop` | 解除接管并恢复原生小爱 |
| 关闭开机自启 | 删除 `/data/init.sh` 中的启动行，或 `touch /data/open-xiaoai/disabled` 后重启 | 开机不启动 Agent |
| 切回原厂系统（SSH 可用） | `fw_env -s boot_part boot<原槽位> && reboot` | 切回未动过的官方槽位 |
| 切回原厂系统（SSH 不通） | Micro-USB 刷机模式执行 `bulkcmd "setenv boot_part boot<原槽位>"` 与 `saveenv` | 拔电重启回官方系统 |

> 目录内脚本不带执行位，请按手册使用 `bash`、`python3 -I` 或 `node` 调用；`work/` 存放本地构建产物，`backup/` 存放分区备份（请异地另存一份）。
