# 历史存档（勿作为实现依据）

本目录是重构前的原始文档，**仅作背景与取证用**。与上层 `docs/00–04` 冲突时，以上层为准。

| 文件 | 原文件名 | 内容 | 现状 |
|---|---|---|---|
| `research-2026-11.md` | 小爱音箱Pro-自定义大模型-深度调研.md | 全项目全景、路线对比、各仓库评估、合规风险 | 已被 `docs/00-decision.md` 与 `docs/04-reference.md` 吸收 |
| `design-notes-2026-11.md` | LX06-端侧本地助手-设计方案.md | 端侧闭环的早期设计（含方案 A/B/C） | 已被 `docs/02-architecture.md` 取代 |

## 什么情况下来这里查

- 想了解**为什么否掉某个项目**（xiaogpt / MiGPT / xiaoai-plus / zongzhu / xiaoai-gpt / xiaoai-doubao）→ 先看 `04-reference.md` §5，细节看 `research-2026-11.md`。
- 想确认**某个上游文件的原始出处** → `research-2026-11.md` 的参考链接。
- 想**改回双助手方案**（保留原生小爱 + 桥接后端）→ 参考 `design-notes-2026-11.md`；它需要重新验证音频采集与设备兼容性。

## 不要做的事

- 不要把这里的结论直接抄进实现——所有关键事实都已在 `docs/01-verify.md` 里转成"必须先实测的 gate"。
- 不要把这里的旧路径（`xiaoai-bridge/`、`speaker/`）当成现有仓库结构，上游已改名为 `zongzhu-speaker/`。
