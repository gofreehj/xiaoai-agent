# XiaoAI Agent 架构与交互设计

> 本文档是 `xiaoai-agent` 运行时架构、双模式语音交互设计与模块边界的**唯一权威说明**。
> 工程不变量与测试红线见 [AGENTS.md](../AGENTS.md)；机型硬件前提见 [models.md](models.md)；控制面 API 见 [control-api.md](control-api.md)。

---

## 1. 设计目标与运行模式

`xiaoai-agent` 运行在小爱音箱端侧（ARMv7 Linux，glibc 2.25），通过统一的模块化内核原生支持两种一等运行模式（`runtime.mode`）：

| 维度 | `exclusive`（独占接管模式） | `dual`（双助手共存模式） |
|---|---|---|
| **典型场景** | 整机完全由大模型接管，原生小爱静默不抢答 | 原生小爱处理闹钟/倒计时/米家日常，外部 KWS + 大模型负责深度对话 |
| **唤醒前端** | 固件原生 `mipns-*`（远场 AEC/NS/AGC + VAD） | 内置 `ExternalKwsEngine`（`arecord` + 2.56s 环形缓冲）+ 外部 UDP XPCM KWS 服务 |
| **唤醒词** | 原生唤醒词（如“小爱同学”） | 自定义热词（如“豆包豆包”，不得与“小爱同学”重叠） |
| **音频传输** | `/tmp/mico_aivs_lab/usock/speech.usock`（16 kHz mono PCM） | 进程内零拷贝直传 + 通用 [UDP XPCM 协议](external-kws-protocol.md)发往 `dual.kws.endpoint` |
| **原生小爱状态** | 被 `speech.usock` bind-mount 隔离，仅保留 `common.usock` 供 TTS 使用 | 完整保留并正常运行；检测到硬件回采 `sys_active` 时自动门控防误触发 |
| **部署形态** | 音箱端单二进制 `/data/open-xiaoai/xiaoai-agent` | 同左（零外部中继进程），外部 KWS 可运行在路由器/NAS/PC/Docker 等任意设备 |

> **分工边界**：倒计时、闹钟、提醒等设备原生高频日常功能归属原生小爱（`dual` 模式下直接喊“小爱同学”），Agent 不重复造轮子；Agent 内置轻量**本地语音音量控制**（`mphelper volume_up/down/set`）、**媒体播放控制**与**会话退出**路由，零网络延迟响应。

---

## 2. 模块分层结构

`xiaoai-agent/src/` 按职责划分为 6 个清晰层级，跨层仅通过冻结契约通信：

```text
xiaoai-agent/src/
├── main.rs                # CLI 入口（run / doctor）、组件装配与信号监听
├── config.rs              # 配置 schema、默认值、collect_startup_warnings 预检
├── frontend/              # 1. 语音前端适配层（屏蔽底层协议差异）
│   ├── mod.rs             #    VoiceFrontend 契约、FrontendEvent、AudioStreamChunk
│   ├── mipns.rs           #    exclusive 适配器（speech.usock + pnshelper + 陈旧帧排空）
│   └── external_kws.rs    #    dual 适配器（arecord 采集/下采样/回采检测 + UDP XPCM 客户端 + 2.56s 缓冲）
├── audio/                 # 2. 音频与输出协调层
│   ├── mod.rs             #    AudioCoordinator（LED 状态灯、唤醒打断、earcon、静音窗口、音频焦点）
│   └── text_prep.rs       #    Markdown/URL/Emoji 清洗 + 中英文/小数安全流式切句
├── session/               # 3. 会话编排与交互流水线
│   ├── mod.rs             #    SessionOrchestrator（多轮状态机、双模式统一主循环）
│   ├── pipeline.rs        #    TurnPipeline（ASR → 本地意图 → LLM 流式 → 1.2s 思考提示音 → 分句 TTS）
│   └── intent.rs          #    LocalIntentRouter（音量/暂停/继续/下一首/退出，零延迟直达）
├── brain/                 # 4. 认知与工具层（agent.rs / asr.rs / mcp.rs / tools.rs / weather.rs）
├── control/               # 5. 本地控制面（control/mod.rs + server.rs + panel.html）
└── runtime/               # 6. 运行时基础设施
    ├── reload.rs          #    SIGHUP 合并式异步热重载与不可热更段回滚
    ├── fault.rs           #    故障分级与让位决策（YieldOutcome 0/3/4/5 契约）
    └── telemetry.rs       #    阶段耗时与状态快照
```

底层支撑模块：`speech.rs`（`speech.usock` 服务端与 `errno` 分级重试）、`vad.rs`（自适应底噪与动态判停）、`capture.rs`（音频采集与 `CaptureOutcome`）、`tts.rs`（`PlaybackGate`、`FinishMode`、`/tmp` 音频上限控制）、`music.rs`（Navidrome 与 `EndReached` 自动切歌）、`device.rs`（设备脚本执行与音量命令）、`airplay.rs`。

---

## 3. 核心交互流水线与边界处理

### 3.1 单轮对话状态流

```text
Wakeup / ControlCommand::Chat
  │
  ├─► AudioCoordinator::on_wakeup()
  │     • PlaybackGate::begin() 推进 token 并清除旧静音窗口
  │     • 立即中止在播 TTS/音频（abort_current_output），释放焦点，点亮唤醒 LED
  │
  ├─► Capture（exclusive: mipns PCM / dual: 内置 AlsaCaptureWorker）
  │     • 自适应底噪环（40-block ring + 0.75 迟滞）+ 动态短/长停顿判停
  │     • 静音窗口开启或硬件回采 sys_active 期间丢弃 PCM，防止自回声误触发
  │     • 区分 CaptureOutcome::Utterance(pcm) 与 CaptureOutcome::IdleEnd（正常收尾，不计失败）
  │
  ├─► ASR（OpenAI HTTP / OpenAI Realtime WS）
  │     • 4xx 非重试快速失败；Realtime 连接支持自动重连
  │
  ├─► LocalIntentRouter（零网络延迟）
  │     • 命中「调大/调小/设置音量」「暂停/继续/下一首」「退出/退下」→ 直接执行，不请求 LLM
  │
  └─► TurnPipeline（LLM 流式 + 分句 TTS）
        • 1.2s 思考延迟提示音：若首句尚未到达且配置了存在的 `dual.thinking_earcon` 文件则播放轻量提示音
        • 流式文本经 text_prep 清洗 Markdown/URL/Emoji 并按中英文安全标点切句
        • 逐句下发 TtsRouter：
            - 中间句使用 FinishMode::WaitForEnd（防并发争抢 miplayer）
            - 尾句在外部 TTS 下使用 FinishMode::DetachForQuietWindow，配合 EndReached 精准收紧静音窗口
        • 流式中途出错且已播报部分内容时，自动播放简短收口提示而非戛然而止
```

### 3.2 关键体验保障机制

1. **打断双保险**：`PlaybackGate` 单调递增 token 拦截尚未开播的合成任务，`device.abort_current_output()` 杀死已启动的外部播放进程；被中断的轮次返回 `Ok(())`，绝不计入连续失败让位计数。
2. **外部 TTS 自回声抑制**：非 `pns` 路径播放时开启 capture 侧静音窗口，由后台轮询 `/tmp/xiaoai-tts-player.log` 的 `EndReached` 信号收紧至 `playback_tail_ms`；未命中信号时自动退回上限时长兜底。唤醒词打断不受影响。
3. **非阻塞网络与超时分层**：
   - `llm.timeout_s` 默认 `20.0s`，流式读取采用 `read_timeout`（空闲超时）+ 总硬上限双重保护；
   - `asr.open_ai.timeout_s` 默认 `10.0s`，全包体读取受超时保护；
   - MCP 服务在启动时异步连接，不阻塞音箱语音就绪；IP 定位缓存 30 分钟，避免频繁外网查询。
4. **双模式协同（`dual`）**：
   - `ExternalKwsEngine` 通过标准 [UDP XPCM 协议](external-kws-protocol.md)向外部服务发送待机 PCM 与 `XPCM_FLAG_PING` 心跳（30s 保活），唤醒后自动发送 `XPCM_FLAG_RESET` 并回放 2.56s 唤醒缓冲实现 One-shot 连说；
   - `capture_dual_utterance` 依据硬件 DAC 回采通道计算的 `sys_active` 门控语音起点，原生小爱外放时绝不误收录回声；所有参考服务端（Python / Docker、红米 AX5）均位于 `deploy/kws-server/`。
5. **tmpfs 空间保护**：外部 TTS 临时音频限制最长保留 600s 且最多保留 5 个文件，防止 50 MB `/tmp` 被撑爆。

---

## 4. 故障隔离、热重载与运维自检

### 4.1 故障处理边界（双主分工）

- **启动脚本看门狗（`start-agent.sh`）**：只负责进程级守护（进程退出、`speech.usock` inode 替换、保护挂载消失），动作唯一为重启恢复；两个模式均通过 `acquire_lock` 串行化，避免并发竞争。
- **Agent 内部让位（`runtime/fault.rs`）**：只负责协议级不可恢复错误（`SpeechServiceEvent::Fatal`）与连续多轮失败；`speech.rs` 对瞬态 `errno` 先做有界重试，确认不可恢复后通过 `run_detached` 调用 `start-agent.sh stop` 将麦克风安全归还原生小爱。退出码 `0/3/4/5` 与 `SocketOwnership` inode 事实共同决定是否退出（详见 [AGENTS.md](../AGENTS.md)）。

### 4.2 `SIGHUP` 配置热重载

通过 `kill -HUP` 或 `POST /reload` 触发，在独立异步任务中执行并自动合并并发请求：
- **热生效（下一轮生效）**：`llm`、`agent`、`music`、`capture`、`tts`、`runtime.session_idle_timeout_s`、`mcp`、`dual.{enter_prompt,exit_prompt,*_earcon,session_timeout_s,exit_keywords}`（`AgentView` 原子替换配置与工具集，保留当前对话历史）。
- **需重启（保持旧值并告警）**：`device`、`asr`、`airplay`、`server`、`runtime.mode`、`dual.kws`、`dual.capture`。
- **拒绝修改**：`runtime.speech_socket`。
- YAML 语法错误时保留旧配置继续运行，绝不导致音箱失声。

### 4.3 启动预检与 `xiaoai-agent doctor`

- 启动时自动执行 `collect_startup_warnings`（仅告警不阻断启动），提前暴露空/占位 `api_key`、`llm.timeout_s < 10s`、`tts_command` 重复加单引号 `'{text}'`，以及 `dual` 模式下 `dual.kws.endpoint` 格式或采集参数越界问题。
- 运行 `xiaoai-agent doctor -c <agent.yaml>` 可一键离线+在线自检：配置解析与预检告警、运行模式/Socket/PID 状态、`/tmp` 剩余空间，以及 ASR / LLM / 天气 / MCP 与（`dual` 模式下）外部 KWS 服务 UDP RTT 连通性。
