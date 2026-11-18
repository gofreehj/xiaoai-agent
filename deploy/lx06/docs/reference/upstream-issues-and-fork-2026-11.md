# 上游 Issues 与 CocoJiao Fork 调研（修正版，2026-11-16）

> 来源：`stevenjoezhang/xiaoai-agent` 的 Issue #1–#20，以及 `CocoJiao/xiaoai-agent` fork。
> 证据分两类：**真机实测**指作者或用户在真实音箱上验证过；**静态推断**指只基于固件文件分析。两者不得混用。

## 一、对 LX06 的结论

1. **L06A 真机证据，不是 LX06 真机证据。** 上游 #3 中 `njhsi` 在 L06A `1.94.13` 上跑通了 `speech.usock` 方案，唤醒效果优于旧 `libvpm` 方案。LX06 与 L06A 同属 1.94.13 固件线，这一点是维护者根据静态分析给出的推断（#3 维护者回复），在 LX06 上仍未实测。
2. **路线一致。** #3 的两条结论与本仓库现有方案吻合：`tts_command` 去掉 `--notify-pns`，改用 `/usr/sbin/tts_play.sh {text}`；pns 必须重启以完成 `speech.usock` 注册。本仓库 `templates/agent.yaml.lx06.example` 与 `start-agent.sh` 的接管流程已按此实现，但尚未在真机验证。
3. **不要回到 ALSA 直采。** #6、#19 讨论的"共存"与"故障让位"与本路线不冲突，但 ALSA 三件套不在批准集内。

## 二、已在本地核实的事实

- 原厂 `analysis/lx06-1.94.13/extracted/root.squashfs` 超级块：magic `hsqs`，block 131072，压缩 4（xz），flags `0x6C0`，其中 EXPORT 位为 1，ALWAYS_FRAG 与 NO_FRAG 位均为 0。即原厂**开启了** exportable，**未开启** always-use-fragments。
- 当前 `scripts/build-image.sh` 已移除 `-always-use-fragments -no-exports`。审查复核的 `unsquashfs -s` 显示重打包超级块与原厂一致；本次修复未重新构建镜像。

## 三、Issues 盘点

### 与 LX06 / 固件构建 / 刷机相关

| 编号 | 状态 | 主题 | 要点 | 对本项目的影响 |
|---|---|---|---|---|
| #3 | open | 其他型号依赖与配置 | L06A 1.94.13 真机跑通；`tts_command` 去掉 `--notify-pns`；需重启 pns 完成注册 | 直接佐证；见第一节 |
| #4 | open | OH2P 刷机后 SSH 不通 | 作者定位为 22 端口未开；维护者建议先 `nc -vz` 再 `ssh -vvv`；老 sshd 需 `+ssh-rsa` | `p0-check.sh` 保留两条 ssh 兼容参数是对的 |
| #9 | closed (PR) | `squashfs.sh` 大小校验在 Linux 下失效 | `stat -f %z` 是 BSD 语法；fork 已修，上游未合 | 本仓库用 `stat -c %s`，不受影响 |
| #14 | open | Linux/Docker 打包与免账号 OTA | 社区称公开接口可免账号取固件；本仓库 `ota-query.py` 同类 | 仅作参考，具体细节未逐条核实 |
| #17 | open | 三篇文档：型号、刷机排障、设备脚本 | 指出 `config.rs` 默认 `--notify-pns` 对 L06A 报错；列出 BusyBox 限制；`scp` 需 `-O`；烧录窗口很短 | 文档与排障清单可借鉴；fork PR #1 已有草稿 |
| #20 | open | `npm run build` 不把 `.env` 的 `SSH_PASSWORD` 传给补丁脚本 | 静默回退默认密码，且无 shadow 哈希校验；`.env` 带 BOM/CRLF 会失败 | 本仓库 `build-image.sh` 用强制输入与 shadow 反验规避 |

### Rust 运行时与架构

| 编号 | 状态 | 主题 | 要点 |
|---|---|---|---|
| #6 | open | 与原生小爱共存、按唤醒词分流 | 与 ADR-004 的独占接管冲突；fork 以故障让位折中 |
| #8 | closed (PR) | 工具开关、shell 超时、队列健壮性 | fork 已合；上游未合。含 `tools_enabled`、30s 超时、`kill_on_drop`、队列满丢帧、`base_url` 不再强加 `/v1` |
| #10 | open | 流式 TTS 与打断 | 整段生成后再阻塞播放，长回答易超时；建议 `PlaybackGate` 与按句播报 |
| #11 | open | 配置热加载 | fork 已实现 `SIGHUP`；`device`、`asr`、`airplay` 仍需重启 |
| #12 | open | 对话历史结构化 | fork PR 已落地 |
| #13 | open | 音频缓冲池化 | fork PR 已落地，`Bytes` 零拷贝 |
| #15 | open | 本地 NAS 曲库 | 索引放 `/tmp` 以免写 NAND |
| #16 | open | QEMU 无刷机调试 | 只能验证脚本与 rootfs，无法验证麦克风与 AEC |
| #18 | open | 单元与回归测试 | fork 已补，84+ 个测试 |
| #19 | open | 故障时把麦克风还给原厂 | fork 已实现让位与退出码契约 |

其余 #1（演示视频）、#2（空 PR）、#5（用户反馈，回退原生小爱的需求）、#7（Renovate 配置 PR）为非技术项，不影响方案。

## 四、Fork 的状态

- 仓库页面显示 main 分支共 146 个提交。此数字是总提交数，不是相对上游的领先数；早先报告的"领先 95 commits"口径不同，已弃用。
- 固件侧 `deploy/client-patch/src/squashfs.sh` 仍只放行 OH2P；`docs/models.md` 中 LX06 标为"⚠️ 有线索但未验证"，L06A 标为"未在本项目验证"。
- 运行时主线已覆盖：故障让位（`start-agent.sh` 退出码 0/3/4/5）、`ShellRunner` 超时、`PlaybackGate`、外部 TTS 静音窗、`SIGHUP` 热加载、`/health` 与状态面板、零拷贝、单元测试。
- 静音窗的 3.25 秒启动延迟来自 OH2P 实测，LX06 需要重新测量。
- 未核实：早先报告的 fork issue 范围"#6–#37"。页面可见的 open issue 为 9 个，编号范围未经核对，此处不再引用。

## 五、对本仓库的建议

1. **构建参数已修正。** 后续构建仍需检查 `check-build.txt` 和分区尺寸。
2. **不改变 T1 / T2A 顺序。** 使用重新打包且校验过换行符的运行时包，依次完成备份、刷机、P0 和手动探针。
3. **T2A 之后再考虑 fork 增强。** 故障让位、超时、`PlaybackGate` 与热加载可在 LX06 底座确认后再合入。合入时保持 `sh /usr/sbin/tts_play.sh {text}`，不使用 fork 默认的 `--notify-pns`。
4. **外部 TTS 静音窗需 LX06 重测。** 3.25 秒延迟不能直接搬到 LX06。
5. **文档对齐。** 当前批准集是 core-4，不包含 LX06 音频补丁。

## 六、出处

- 上游 Issues：https://github.com/stevenjoezhang/xiaoai-agent/issues （#3、#4、#6、#8、#10、#11、#17、#19、#20 已逐条打开核对）
- Fork：https://github.com/CocoJiao/xiaoai-agent （`docs/models.md` 已读取）
- 原厂超级块：`analysis/lx06-1.94.13/extracted/root.squashfs` 前 32 字节
