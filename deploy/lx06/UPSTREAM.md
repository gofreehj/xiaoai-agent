# LX06 与上游 / CocoJiao fork 的升级约定

本目录（`deploy/lx06/`）是 LX06 机型的全部增量。目标是：无论上游 `stevenjoezhang/xiaoai-agent`
还是 `CocoJiao/xiaoai-agent` 前进，同步进来时都不需要手工重放 LX06 的工作。

## 1. 布局边界

LX06 的新增内容只落在这几处：

```text
deploy/lx06/
├── AGENTS.md              # LX06 作业纪律
├── NOTICE                 # 来源与许可
├── README.md              # 唯一权威规格
├── UPSTREAM.md            # 本文件
├── docs/                  # ADR、验证、架构、计划、上游调研
├── patches/core/          # 唯一批准的 4 个补丁
├── scripts/               # 取包、构建、备份、刷写、P0、打包
└── templates/             # agent.yaml.lx06.example
```

以下位置保持不动，避免与上游冲突或在升级时被覆盖：

- `xiaoai-agent/agent.example.yaml` 与 `xiaoai-agent/src/config.rs` 的默认值（LX06 差异只放进本目录模板）。
- `deploy/client-patch/`（OH2P 流水线；LX06 不往这条链里塞补丁）。
- `xiaoai-agent/src/` 的 Rust 源码（本目录不持有修改后的源码副本）。

对仓库根 `README.md`、`deploy/README.md`、`.gitignore` 只做追加式小节，便于三方合并时整段保留。

## 2. 永不提交的内容

`deploy/lx06/work/` 与 `deploy/lx06/analysis/` 已在根 `.gitignore` 中忽略，后者包含官方固件与解包 rootfs。

## 3. 同步上游

一次性添加远端：

```bash
git remote add upstream https://github.com/stevenjoezhang/xiaoai-agent.git
git remote add cherry   https://github.com/CocoJiao/xiaoai-agent.git
```

日常同步（保留 LX06 分支历史，合并而非变基）：

```bash
git fetch upstream cherry
git switch -c codex/upgrade-$(date +%Y%m%d)
git merge upstream/main --no-ff
```

升级到 CocoJiao 版本同理，把 `upstream/main` 换成 `cherry/main`。

预期冲突只出现在根 `README.md` 的代码结构清单、`deploy/README.md` 的机型说明和 `.gitignore`
末尾几行；处理方式是在冲突块里保留 LX06 那一段，其余取上游版本。

## 4. 合入 CocoJiao 运行时增强时的检查点

CocoJiao fork 的收益集中在 Rust 运行时：故障让位、命令超时、`PlaybackGate` 打断、`SIGHUP`
热加载、零拷贝与测试。合入前后逐条核对：

1. `xiaoai-agent/start-agent.sh` 的接管流程是否仍保护 `speech.usock`、是否仍以临时副本去掉 `-r opus32`、是否仍能恢复 `mico_aivs_lab`。三件事任一缺失就回退该文件的上游版本。
2. `device.tts_command` 保持 `sh /usr/sbin/tts_play.sh {text}`，不要被 `--notify-pns` 覆盖。
3. 外部 TTS 的静音窗参数（`playback_startup_grace_ms`、`playback_tail_ms`）在 OH2P 上标定的值不能直接用于 LX06，需真机重测。
4. `runtime.speech_socket` 必须是 `/tmp/mico_aivs_lab/usock/speech.usock`。
5. 适配完成后跑 `bash -n` 覆盖本目录所有 `*.sh`，并用 `scripts/build-agent.sh` 重新产出静态 musl 二进制。

## 5. 验收顺序

刷机链路（T1）先于运行时增强：先用本目录基线完成 6 分区备份、非活动槽刷机、`p0-check.sh`
与手工 `start-agent.sh start/stop`，确认 LX06 的 `speech.usock` 协议可用，再谈 fork 的稳定性改造。
