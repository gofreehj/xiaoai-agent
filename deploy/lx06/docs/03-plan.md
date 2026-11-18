# 03 · 任务分解与验收

> 规则：**一个任务 = 一个可独立验证的增量**。未通过验收不许进入下一项。
> 状态列由实现者更新：⬜ 未开始 / 🟡 进行中 / ✅ 通过 / ❌ 失败（附原因）。

---

## 任务总览

| ID | 任务 | 依赖 | 预估 | 状态 |
|---|---|---|---|---|
| T0 | 仓库与工具链准备 | — | 0.5d | ✅ |
| T1 | 刷机 + P0 验证 | T0 | 1d | 🟡（P0 ✅；6 分区备份待补登） |
| T2A | 运行时接管探针与基线二进制上机 | T1 | 1d | ✅ |
| T3 | 唤醒接入与分段状态机骨架 | T2A | 1d | ✅（真机唤醒与 `pnshelper` 多轮分段采集已跑通） |
| T4 | ASR 接入（超时/重试/降级） | T3 | 1d | ✅（SiliconFlow SenseVoice 与 HTTP 连接池防假死已上机） |
| T5 | LLM 接入 + 提示词 + 文本清洗切分 | T4 | 1d | ✅（LLM、Client 复用、时间注入、音乐工具按需裁剪与天气/搜索工具已上机） |
| T6 | TTS 播放 + 打断（barge-in） | T5 | 1.5d | 🟡（`tts_play.sh` 播报与 `PlaybackGate` 唤醒打断已上机；长文本分句流水线可选） |
| T7 | 自启 / 看门狗 / 日志轮转 / stop 恢复 | T6 | 1d | ⬜（`stop` 恢复已验证；`/data/init.sh` 自启待最终开启） |
| T8 | 浸泡与最终验收 | T7 | 1d | ⬜ |

---

## T0 · 仓库与工具链

**步骤**
1. 建 `NOTICE`，记录上游来源与许可证（xiaoai-agent LGPL-3.0、open-xiaoai MIT）。
2. 安装 Rust 1.96 + `armv7-unknown-linux-musleabihf` + Zig。
3. 验证：`bash scripts/build-agent.sh` 能产出静态 ARMv7 musl 二进制。
4. 建 `docs/log/`。

**验收**
- `file target/.../xiaoai-agent` 显示 `ARM, EABI5`；
- `file` 显示 `ARM, EABI5` 与 `statically linked`，且二进制不含 `GLIBC_` 符号。

**进度（2026-11-16）：✅ 全部完成**
- `NOTICE`、`docs/log/` 已建。
- Rust 1.96.0 与 Zig 0.17.1 已可经 `scripts/build-agent.sh` 构建
  `armv7-unknown-linux-musleabihf`；发布候选是静态 musl 产物，不能用动态 GNU 产物替代。
- `patches/core/` 的 4 个补丁在 1.94.13 上 `--fuzz=0` 全部应用，镜像自检通过：
  2137 条目不变、8 处预期改动。期间修复了 `02-login.patch` 的 hunk 行数错误
  （原文件必须 fuzz=1 才能打上）。
- 详细命令与输出：`docs/log/2026-11-16-toolchain-and-core-patch.md`。

---

## T1 · 刷机与 P0

按 `docs/01-verify.md` 执行。

**验收**
- 6 分区备份完整（尺寸 == `/proc/mtd`）；
- 已刷非活动槽，原生小爱仍应答；
- SSH 可用；
- `docs/log/p0.md` 含四条 gate 的命令与输出；
- README 状态表已写明能否进入 T2A。

**失败处理**：任何读回不一致 → 停止，切回原槽，报告。

---

## T2A · 运行时接管探针与基线二进制上机

**步骤**
1. 使用仓内 `xiaoai-agent`，按 `UPSTREAM.md` 核对来源和融合边界。
2. 用 `bash scripts/build-agent.sh` 构建，再用 `bash scripts/package-runtime.sh <静态 musl 二进制>` 打包；仅将该运行时包的二进制上传到 `/data/open-xiaoai/xiaoai-agent`。
3. 使用运行时包中的 `start-agent.sh`；不得以普通客户端方式连接 socket。
4. 从运行时包的 `agent.yaml.example` 建立最小 `agent.yaml`，socket 路径为 `/tmp/mico_aivs_lab/usock/speech.usock`，并在设备上填入密钥后设为 600。
5. 手动执行一次 `start` / 唤醒 / `stop`，不加自启。

**验收**
- 进程能启动且不崩溃 60s；
- Agent 成功绑定标准 socket；`speech.usock` 已被保护且 inode 在重启 `mico_aivs_lab` 后不变；
- `common.usock` 在受保护启动后存在，PNS 不含 `-r opus32`，mipns 完成注册并发送 PCM；
- 喊唤醒词，日志出现一次唤醒事件与一段 PCM；
- `start-agent.sh stop` 后原生小爱恢复应答。

**失败处理**：记录 socket、进程、PNS 参数及 errno；执行 `stop` 恢复原生语音。停止端侧实施并转 bridge。

---

## T3 · 唤醒与状态机

**验收**
- 喊一次唤醒词 → 状态 `Idle→Waking→Listening`，日志时间戳连续；
- 15s 无语音 → 回 `Idle`；
- 连续喊两次 → 不叠加会话（幂等）。
- 每个连续对话轮次在前一轮 DialogFinish 后，通过 `pnshelper` 请求独立采集；不得恢复 `dialog_continuous`。

---

## T4 · ASR

**验收**
- 说 5 句话，4 句以上识别正确（中文短句）；
- 断网时 5s 内报错并播报降级提示，不卡死；
- 单次耗时 P50 < 1.2s（记录到日志）。

---

## T5 · LLM

**验收**
- 回复口语化、≤ 3 句（抽查 10 条）；
- 无 Markdown 残留（`**`、`#`、`-` 不得被 TTS 念出）；
- 首 token P50 < 1.5s；
- 超时 → 播报"网络有点问题"，状态回 `Idle`。

---

## T6 · TTS 与打断

**验收**
- 长回答（300 字）能被正确切分并连续播放，无截断；
- 播放中说话 → 500ms 内停止播放并进入 `Listening`；
- 播放中喊唤醒词 → 立即打断；
- 连续 3 轮对话后需重新唤醒。

---

## T7 · 自启与恢复

**验收**
- 断电重启 → 60s 内可唤醒；
- `kill -9` 进程 → 看门狗 10s 内拉起；
- `start-agent.sh stop` → 原生小爱恢复应答，且 PNS 覆盖与 socket 保护挂载均已解除（**必须验证**）；
- 日志轮转生效（>2MB 切分，保留 3 份）。

---

## T8 · 浸泡与终验

**验收**
- 连续 8 小时，每 30 分钟唤醒一次，成功率 ≥ 90%；
- 内存无增长（首末差 < 10%）；
- 无重启循环、无设备无响应；
- 全部测试用例（§测试计划）通过。

---

## 测试计划

| # | 用例 | 期望 |
|---|---|---|
| 1 | 静默喊唤醒词 | 3s 内回应"在" |
| 2 | 问一个需要 3 句回答的问题 | 完整播报，不截断 |
| 3 | 播报中插话 | 500ms 内停止，听新问题 |
| 4 | 连续问 3 个问题 | 每轮完成后可继续对话；达到 3 轮上限后必须重新唤醒 |
| 5 | 说"退出/再见" | 回 Idle，不再响应 |
| 6 | 拔网线 | 播报"连不上"，不卡死 |
| 7 | 断电重启 | 60s 内恢复 |
| 8 | `kill -9` | 10s 内自愈 |
| 9 | 原生小爱是否出声 | **全程不出声**（被旁路） |
| 10 | `stop` 后喊"小爱同学" | 原生正常应答 |

---

## 完成定义（DoD）

- [ ] T0–T8 全部 ✅
- [ ] 10 条测试用例全过，证据在 `docs/log/`
- [ ] `README.md` 状态表更新
- [ ] `NOTICE` 完整
- [ ] 一份"用户手册"：如何改模型、如何停用、如何回滚
