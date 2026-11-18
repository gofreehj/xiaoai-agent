# 2026-11-18 · 设备状态记录（只读检查与处置）

## 当前设备状态

- 固件：`LX06 1.94.13`，启动槽 `boot0`，根文件系统为 `system0`（`/dev/mtdblock4`，只读）。
- `system0` 内容与 `D:\Tools\Amlogic_Flash_Tool_v6.0.0\LX06_1.94.13_patched.squashfs` 一致（SHA256 `3FBCFCD8…4225`，38,453,248 字节）。该镜像能开 SSH，是当前运行的固件。
- SSH：`root@192.168.43.155` 可用（dropbear 在线）。
- `/etc/asound.conf`、`/bin/ota`、`/etc/init.d/dropbear` 与原版的差异，符合 `expected-changes-lx06.txt` 中的预期改动。
- `/usr/bin/mico_record` 不存在，这是正常的：它只是 ALSA 配置里的别名，不是程序。

## 发现的远程 client 与处置

- 启动路径：`/etc/rc.local` 第 4 行调用 `/data/init.sh`，由上游 idootop 项目提供。
- 运行的程序：`/data/<目录>/client`（pid 2602），启动参数 `ws://47.243.66.192:4399`（来自 `server.txt`）。它包含录音与 WebSocket 远程指令模块，不属于本项目。
- 处置（2026-11-18，经用户同意）：
  1. 停止 pid 2602；
  2. 将 `/data/init.sh` 移到 `/data/xa-disabled-20261118/init.sh`，`rc.local` 的 `[ -f ]` 判断因此失效；
  3. 将 client 目录移到 `/data/xa-disabled-20261118/client-dir/`。
- 未做的事：未修改只读的 `/etc/rc.local`，未删除任何文件，未检查 `/data` 下其他自启动项（超出本次范围）。

## 后续

- 备份目录 `/data/xa-disabled-20261118/` 可在确认无依赖后删除。
- 主线仍是 LX06 集成：按 `docs/01-verify.md` 推进，不做额外的设备排查。