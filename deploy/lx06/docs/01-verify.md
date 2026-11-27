# 01 · LX06 完整刷机与验证流程

> 全链路：**取包校验 → 只读备份 → 构建镜像 → 刷非活动槽 → 首启确认 → P0 → T2A → 判定**。
> 每个阶段都有明确的通过条件和失败动作；任一 gate 不通过就停在该阶段，不要往下走。

---

## 0. 流程总览

| 阶段 | 在哪执行 | 输入 | 产物 | 通过条件 |
|---|---|---|---|---|
| R0 取包 | 电脑 | SN + 版本 | `mico_all_*.bin` | 大小与 MD5 与官方接口一致 |
| R1 只读备份 | 电脑 + USB | 音箱 | 6 个 `.img` + `sha256sums.txt` | 每个文件大小等于 `parts.env` 里的值 |
| R2 构建镜像 | WSL / Linux | 官方包 + `parts.env` | `root-patched.squashfs` + `boot.img` | 四项核对全过 |
| R3 刷写 | 电脑 + USB | build 目录 | `flash.log` + 读回文件 | 两次读回 `cmp` 一致，且 `boot_part` 生效 |
| R4 首启确认 | 音箱 | — | 人工观察 + 命令输出 | 原生小爱正常、SSH 可用、版本正确、OTA 已禁用 |
| R5 P0 | 电脑 SSH | 音箱 IP | `docs/log/p0.md` | G1–G4 全 PASS |
| R6 T2A | 电脑 SSH | 运行时包 | Agent 日志 | `start` 后收到 PCM，`stop` 后原生恢复 |
| R7 判定 | — | 以上证据 | `README.md` 状态表 | 全过才可进入 T3 |

以下顺序不能颠倒：**先备份，再构建，再刷写**。备份没做成之前不要碰任何写操作。

仓库中的脚本没有执行位，按示例通过 `bash`、`python3 -I` 或 `node` 调用。`work/` 可以清理；清理后要重新生成运行时包。设备备份保存在同级的 `backup/`，请另存一份。

---

## 1. 物料与前置

| 项 | 要求 |
|---|---|
| 音箱 | 小爱音箱 Pro（**LX06**），底部标签确认；LX06 与 L06A 不是同一型号 |
| 线 | **Micro-USB 数据线**（能传数据，不是纯充电线），需拆外壳，直连电脑不过 Hub |
| 刷机电脑 | Windows 用 `update.exe`（Amlogic USB Burning Tool 目录内），macOS 用 `update`；**必须本机 USB**，不能 Docker |
| 构建电脑 | WSL2 或 Linux，装 `squashfs-tools`、`patch`、`openssl`、`python3`（≥3.8）、`coreutils` |
| 网络 | 音箱与电脑同一局域网；路由器给音箱固定 IP |
| 账号 | 不需要小米账号，也不需要云服务账号 |

**进入 USB 刷机模式**（`identify` 能识别到设备即可）：拔掉音箱电源 → 在终端运行 `update.exe identify` → 看到 `Waiting for device` 后插上电源。识别窗口很短，识别不到就换端口重试；仍不行再检查驱动与数据线。

---

## 2. R0 · 取包与校验

```bash
# 只查询，不下载；SN 用 - 表示只按型号+版本查
mkdir -p deploy/lx06/work
python3 -I deploy/lx06/scripts/ota-query.py - 1.94.13 > deploy/lx06/work/ota.json
```

从返回 JSON 的 `data.currentInfo` 取 `link`、`hash`、`size`，下载后放到
`deploy/client-patch/assets/`，**保留原文件名**（解包脚本用文件名里的 MD5 片段校验）：

```bash
curl -L -o deploy/client-patch/assets/mico_all_911151b95_1.94.13.bin '<link>'
stat -c %s deploy/client-patch/assets/mico_all_911151b95_1.94.13.bin
md5sum   deploy/client-patch/assets/mico_all_911151b95_1.94.13.bin
```

通过条件：大小、MD5 与接口返回一致。1.94.13 的已知值为 **43,478,328 字节**、
MD5 `b202de2bbe149ce884aa500911151b95`。

失败动作：版本不对或校验不符就停，**不要**用别的版本或将就着刷。

---

## 3. R1 · 只读备份（必须先做）

分区尺寸只能来自设备本身。LX06 不能沿用 OH2P 的 `0x2800000 / 0x600000`。

```bash
# 1) 从 u-boot 环境解析真实分区表，生成 parts.env（只读，不写设备）
UPDATE=/path/to/update.exe bash deploy/lx06/scripts/discover-parts.sh

# 2) 按 parts.env 逐个 mread，六分区备份 + sha256sums.txt
UPDATE=/path/to/update.exe bash deploy/lx06/scripts/backup_readonly.sh
```

两个脚本都会提示"拔电 → 运行 → 插电"，`WAIT`（默认 90 秒）可调。
`discover-parts.sh` 只跑 `identify` 与 `bulkcmd printenv`，`backup_readonly.sh` 只跑 `mread`。
备份默认保存在 `deploy/lx06/backup/<时间戳>/`，其中同时保存分区表及存在时的 U-Boot 原始输出；请另存一份备份。`work/` 用于可清理的构建与中间文件。

通过条件：

- `deploy/lx06/work/parts.env` 含 `bootloader / tpl / boot0 / boot1 / system0 / system1` 六项；
- 六个 `.img` 的大小逐字节等于 `parts.env` 中的值（脚本自检，不一致直接退出）；
- `sha256sums.txt` 已生成。

失败动作：尺寸不符说明分区表或读取有误，修好驱动/线材后**整套重做**。长度不对的备份等于没有备份。

> 此时先记下 `system0` 与 `system1` 的字节数，R2 的 `SYSTEM_MAX` 要用它。

---

## 4. R2 · 构建补丁镜像（WSL / Linux）

前提：`TMPDIR` 必须在 Linux 原生文件系统（默认 `/tmp`）。在 `/mnt/d` 这类 DrvFS 上解包会把权限全改成 0777，脚本会直接拒绝。

```bash
cd /path/to/xiaoai-agent  # 改为本机仓库根目录

# 两槽取较小值；实际刷写仍须核对目标槽。
SYSTEM_MAX=$(awk '$1=="system0" || $1=="system1" {if (!n || $2<min) min=$2; n++} END {if (n==2) print min}' deploy/lx06/work/parts.env)
[ -n "$SYSTEM_MAX" ] || { echo '缺少 system0 或 system1 尺寸'; exit 1; }

SPEAKER_ROOT_PASSWORD='<自定 root 密码>' \
SYSTEM_MAX="$SYSTEM_MAX" \
TMPDIR=/tmp \
bash deploy/lx06/scripts/build-image.sh \
  deploy/client-patch/assets/mico_all_911151b95_1.94.13.bin \
  deploy/lx06/work/build
```

脚本固定只应用 `deploy/lx06/patches/core/` 的 4 个补丁（SSH、root 登录、禁用 OTA 写入、`/data/init.sh` 启动入口），不读取其它补丁目录，也不接受外部覆盖。

产物在 `deploy/lx06/work/build/`：

| 文件 | 用途 |
|---|---|
| `root-patched.squashfs` | 写入 `system<SLOT>` |
| `boot.img` 或 `boot.img.encrypt` | 写入 `boot<SLOT>`，与系统同版本 |
| `check-build.txt` / `check-files.txt` | 改动集合与条目数核对记录 |
| `sha256sums.txt` | 两个产物的校验和 |

通过条件（四项全过才允许刷）：

1. 镜像大小 ≤ `system` 分区尺寸（`SYSTEM_MAX` 已设置时脚本自检）；
2. 与官方系统比对，**改动文件集合等于** `scripts/expected-changes-lx06.txt` 的 8 个路径；
3. 条目数、权限、属主不变（默认 2137 条目）；
4. `shadow` 里的密码串能用你输入的密码验证通过（脚本自动回验）。

失败动作：补丁打不上就停，**不许** `patch --fuzz`。核对失败时看 `check-build.txt`，不要手工绕过。

需要保留解包现场排查时加 `KEEP_WORK=1`，脚本会打印临时目录路径。

---

## 5. R3 · 刷入非活动槽

```bash
# 1) 确认当前从哪边启动
fw_env -g boot_part                 # 设备侧，SSH 可用时
# 或
update.exe bulkcmd "printenv boot_part"

# 2) 只写另一边（当前是 boot0 就写 SLOT=1，反之亦然）
SLOT=1 UPDATE=/path/to/update.exe bash deploy/lx06/scripts/flash_slot.sh deploy/lx06/work/build
```

脚本会先校验产物尺寸不超过 `boot<SLOT>` / `system<SLOT>`，再要求输入 `YES` 确认。

脚本行为：

- 写 `boot<SLOT>` → `mread` 读回 → `cmp` 逐字节比对；
- 写 `system<SLOT>` → 读回 → `cmp`；
- 两步都通过才 `setenv boot_part boot<SLOT>` + `saveenv`，并读回 `boot_part`。

补充参数：`SKIP_BOOT=1` 只重写 system（本轮已写过并核对过 boot）；`BOOTDELAY=15` 同时延长 u-boot 等待时间。

通过条件：两段镜像内容读回一致，`boot_part` 读回为目标槽，且退出码为 0。

失败动作：任一步失败立即停。若失败发生在设置启动参数期间或之后，启动槽状态未知；先读回并确认，再决定恢复操作。

---

## 6. R4 · 首启确认

拔电重插，等它正常开机。按顺序确认：

1. **先喊"小爱同学"**，确认原生应答正常——验证系统能启动、语音链路未被破坏；
2. 再连 SSH：

```bash
ssh -o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa root@<IP>
```

3. 在设备上确认四项：

```sh
fw_env -g boot_part                 # 应等于 boot<SLOT>
grep ROM /usr/share/mico/version    # 应等于目标版本 1.94.13
grep -c "no upgrade" /bin/ota       # 1 表示自动升级已禁用
cat /proc/mtd                       # 记录真实分区表，用于补存 data
```

补存 data 分区（只读）：

```bash
cat /proc/mtd | grep data           # 先确认真实 mtdN 与大小
# 替换为 R1 生成的实际备份目录，确保目标文件不存在。
BACKUP_DIR='deploy/lx06/backup/替换为实际时间戳'
[ -d "$BACKUP_DIR" ] && [ ! -e "$BACKUP_DIR/data-full.img" ] || { echo '备份目录不存在或目标文件已存在'; exit 1; }
ssh -o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa root@<IP> \
  'dd if=/dev/mtdN 2>/dev/null' > "$BACKUP_DIR/data-full.img"
```

把 `mtdN` 换成上一条命令实际输出的编号；动手前确认电脑端有足够空间。

失败动作：

- 开不了机或原生不应答 → 进刷机模式 `update.exe bulkcmd "setenv boot_part boot<另一边>"` + `saveenv`，回原槽；
- SSH 不通 → 先 `nc -vz <IP> 22` 区分"端口没开"与"握手不兼容"：端口不通多半是槽位或启动问题，端口通但握手失败则确认两条 `+ssh-rsa` 参数已带上。

---

## 7. R5 · P0 验证

```bash
bash deploy/lx06/scripts/p0-check.sh <IP>
```

P0 只读：不打开 ALSA 设备，不连接或劫持原生 `speech.usock`，也不改变设备进程状态。

| # | 检查 | 通过含义 |
|---|---|---|
| G1 | 原生语音前端 `mipns` 在跑 | 唤醒可继续验证 |
| G2 | `/tmp/mico_aivs_lab/usock/{speech,common}.usock` 都是 socket | 值得进入受控接管探针 |
| G3 | `tts_play.sh` 是普通文件且 `mphelper` 可用 | 具备播报前提（LX06 该脚本无 shebang，需用 `sh`）；实际播报需在 R6 验证 |
| G4 | 能读到 `mipns` 的 `/proc/<pid>/cmdline` | 可确认 `-r opus32` 转换条件 |

结果自动写入 `deploy/lx06/docs/log/p0.md`（含原始输出）。

| 结果 | 后续 |
|---|---|
| G1–G4 全 PASS | 进入 R6 T2A |
| 任一 FAIL | 停在这里，记录证据；端侧路线不成立则转 bridge |

G2 只说明"值得做探针"，**不证明协议兼容**；真正的兼容性由 R6 判定。

---

## 8. R6 · T2A 运行时探针

P0 全过后才做，且全程手工，不装自启。

```bash
# 1) 构建静态 ARMv7 musl 二进制（WSL / Linux）
bash deploy/lx06/scripts/build-agent.sh deploy/lx06/work/agent-build

# 2) 封装运行时包（强制校验静态链接、ARM EABI5、无 GLIBC 符号）
RUNTIME_DIR="deploy/lx06/work/runtime-$(date +%Y%m%d-%H%M%S)"
bash deploy/lx06/scripts/package-runtime.sh \
  deploy/lx06/work/agent-build/xiaoai-agent \
  "$RUNTIME_DIR"

# 3) 上传（老 sshd 的 scp 需要 -O）
SSH_OPTS=(-o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa)
ssh "${SSH_OPTS[@]}" root@<IP> 'mkdir -p /data/open-xiaoai'
scp -O "${SSH_OPTS[@]}" "$RUNTIME_DIR"/xiaoai-agent-lx06-runtime/* root@<IP>:/data/open-xiaoai/

# 4) 设备侧准备配置（用本目录模板，不要用上游默认值）
ssh "${SSH_OPTS[@]}" root@<IP>
cd /data/open-xiaoai
cp agent.yaml.example agent.yaml
chmod 700 xiaoai-agent start-agent.sh
chmod 600 agent.yaml
```

`agent.yaml` 必须确认：`runtime.speech_socket` 为 `/tmp/mico_aivs_lab/usock/speech.usock`，
`device.tts_command` 为 `sh /usr/sbin/tts_play.sh {text}`（不能带 `--notify-pns`），
ASR / LLM 的 key 在设备上填写，不要提交。

```bash
# 5) 受控启动
XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log ./start-agent.sh start

# 6) 恢复原生（每次探针结束都要做）
XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log ./start-agent.sh stop
```

验收清单（全部满足才算 T2A 通过）：

- 进程连续运行 60 秒不退出；
- 日志显示 socket 绑定成功，且 `speech.usock` 的 inode 在 `mico_aivs_lab` 重启后不变；
- `common.usock` 恢复存在，`mipns` 命令行不再带 `-r opus32`；
- 喊一次唤醒词，日志出现一次唤醒事件和一段 16k mono PCM；
- 播报能出声（验证 `tts_play.sh` 模板正确）；
- `stop` 之后原生小爱恢复应答。

失败动作：记录 `ps`、`ls -l /tmp/mico_aivs_lab/usock/`、`/proc/<pid>/cmdline` 与日志片段，执行 `stop` 恢复原厂。T2A 失败说明该固件不适合端侧接管，转 bridge。

---

## 9. 失败处置矩阵

| 阶段 | 现象 | 立即动作 | 证据 |
|---|---|---|---|
| R0 | 版本不存在或 MD5 不符 | 停止，换正确版本 | `ota.json` |
| R1 | `mread` 长度不符 | 修驱动/线材，整套重做 | `backup.log` |
| R2 | 补丁打不上 / 核对失败 | 停止，不得用 `--fuzz` | `check-build.txt` |
| R3 | boot 或 system 读回不一致 | 停止；启动参数未改，仍在原槽 | `flash.log`、`*.readback` |
| R3 | 启动参数写入或读回失败 | 停止；先用 `printenv boot_part` 确认槽位状态，再决定恢复操作 | `flash.log` 尾部及读回输出 |
| R4 | 无法开机 / 原生不应答 | 进刷机模式切回 `boot<另一边>` | `update identify` 输出 |
| R4 | SSH 不通 | 先 `nc -vz <IP> 22` 再 `ssh -vvv` | 两段输出 |
| R5 | 任一 gate FAIL | 停在 P0，端侧路线判定失败 | `docs/log/p0.md` |
| R6 | start 失败或收不到 PCM | `stop` 恢复原厂，记录证据 | Agent 日志 + socket 列表 |

---

## 10. 回滚

三条独立路径，按能用哪条选：

```bash
# A. 系统侧：切回另一个槽（SSH 可用时）
fw_env -s boot_part boot<另一边> && reboot

# B. 系统侧：SSH 不通时进刷机模式
update.exe bulkcmd "setenv boot_part boot<另一边>"
update.exe bulkcmd "saveenv"

# C. 只停 Agent，保留补丁系统
touch /data/open-xiaoai/disabled && reboot
```

回滚后仍要喊一次"小爱同学"确认原生恢复应答。

---

## 11. 本阶段完成标准

- [ ] R0：官方包大小与 MD5 与接口一致
- [ ] R1：六分区备份完整，尺寸与 `parts.env` 一致，`sha256sums.txt` 已生成
- [ ] R2：构建四项核对全过，改动集合等于 8 个路径
- [ ] R3：两次读回 `cmp` 一致，`fw_env -g boot_part` 已是新槽
- [ ] R4：原生小爱正常、SSH 可用、版本 1.94.13、`/bin/ota` 含 `no upgrade`
- [ ] R5：G1–G4 全 PASS，`docs/log/p0.md` 已生成
- [ ] R6：T2A 六条验收全过，`stop` 后原生恢复
- [ ] 失败项已按第 9 节留证据，并写明停在哪一步
