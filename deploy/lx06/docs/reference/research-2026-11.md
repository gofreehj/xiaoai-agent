# 小爱音箱 Pro 接入自定义大模型：开源方案深度调研

> 调研时间：2026-11 ｜ 目标设备：小爱音箱 Pro（LX06，2019 款）
> 结论均来自各项目 README / 官方文档 / 社区实践，文末附原始链接。

---

## 0. 结论先行（TL;DR）

1. **你的设备是「小爱音箱 Pro」，型号 LX06**（音箱屁股上有丝印）。它是 Amlogic 方案、A/B 双系统，**原生不带大模型、不支持连续对话**。小米后续的「小米智能音箱 Pro（OH2P）」才原生接了大模型 —— 两者刷机方式不同，本文都覆盖。
2. 开源实现只有 **三条技术路线**，没有第四条：
   - **A. 云端接口层（不拆机）**：`xiaogpt` / `MiGPT` / `MiGPT-Next`。靠小米云 MiNA 接口轮询对话、再 TTS 播回去。
   - **B. 刷机接管音频（推荐）**：`open-xiaoai` 补丁固件 + Rust 客户端 + 服务端（`open-xiaoai-bridge` / `xiaozhi-esp32-server`）。直接拿到麦克风 PCM 和扬声器，真正做到**连续对话、随时打断、自定义唤醒词**。
   - **C. 不动音箱、外挂一个 AI 硬件**：ESP32-S3 跑「小智」，把小爱当普通蓝牙音箱。零风险、可逆。
3. **关键时间点**：作者 idootop 已于 **2026-04-04 归档** `open-xiaoai`、`mi-gpt`、`migpt-next` 三个仓库。**但社区 fork 接棒了**：
   - `coderzc/open-xiaoai`（刷机 + 客户端教程的维护版）
   - `coderzc/open-xiaoai-bridge`（服务端，**至今活跃**，已迭代出 OpenClaw / QwenPaw / 小智 AI 接入）
   - `yihong0618/xiaogpt`（云端路线，**至今活跃**，已重写为 Republic 多 provider 架构）
4. **"自定义大模型"在工程上就等于一个 OpenAI 兼容端点**（`/v1/chat/completions`）。DeepSeek、Qwen、Kimi、GLM、豆包、Ollama、vLLM、LM Studio、one-api/new-api 网关，全部同一套配置，改 `base_url` + `model` + `api_key` 即可。
5. **给 LX06 的建议**：走**路线 B**（刷机 30–60 分钟 + 服务端 Docker 30 分钟），因为只有它解决"小爱抢话 / 不能打断 / 每轮都要唤醒"这三个致命问题。若你不愿拆机、或设备还在保修期 → 路线 A（`xiaogpt`，10 分钟起跑）。若你连账号都不想碰 → 路线 C。

---

## 1. 先认清设备：LX06 vs OH2P

| 项目 | 小爱音箱 Pro（LX06，2019） | 小米智能音箱 Pro（OH2P，2025） |
|---|---|---|
| 刷机接口 | **Micro-USB 调试口，需拆外壳**（主板左上角） | **底部 Type-C，免拆机** |
| 平台 | Amlogic（USB 量产模式 + U-Boot `bulkcmd`） | Amlogic（同上） |
| 系统分区 | A/B 双系统（`system0` / `system1`，`boot0` / `boot1`） | 同左 |
| 原生能力 | 无大模型、无连续对话；家电控制靠小爱云 | 原生接大模型、支持连续对话 |
| 刷机后原生功能 | **保留**（补丁不改原生逻辑，仅关掉 OTA） | 保留 |
| 可回滚 | 切 `boot1` 即回原厂系统 | 同左 |

> 重要细节（来自 `docs/flash.md`）：
> - 补丁固件默认开启 SSH，**root 密码 `open-xiaoai`**，连接需要 `-o HostKeyAlgorithms=+ssh-rsa`。
> - 刷机只写 `system0`；想恢复原厂，执行 `fw_env -s boot_part boot1` 重启即可。
> - 补丁固件会**关闭系统更新**，避免 OTA 覆盖掉补丁。

---

## 2. 开源项目全景（2026-11 实测状态）

| 项目 | 规模 | 状态 | 原理 | 要刷机？ | 连续对话/打断 | 备注 |
|---|---|---|---|---|---|---|
| [idootop/open-xiaoai](https://github.com/idootop/open-xiaoai) | 2.6k★ | ⛔ 2026-04 归档 | 补丁固件 + Rust Client + 示例 Server | ✅ | ✅ / ✅ | 一切后续方案的"祖师爷"，文档仍是最好读的 |
| [coderzc/open-xiaoai](https://github.com/coderzc/open-xiaoai) | fork | ✅ 维护中 | 同上（刷机/客户端教程 + 示例） | ✅ | ✅ / ✅ | 原仓库的接棒主线 |
| [coderzc/open-xiaoai-bridge](https://github.com/coderzc/open-xiaoai-bridge) | 新项目 | ✅ **活跃** | Python 服务端：VAD+KWS+ASR+LLM+TTS | 需先装 Client | ✅ / ✅ | **当前最优解**，支持 OpenAI 兼容 / 小智 / OpenClaw / QwenPaw |
| [idootop/mi-gpt](https://github.com/idootop/mi-gpt) | 12.5k★ | ⛔ 2026-04 归档 | MiNA 轮询 + TTS 回播 | ❌ | 半支持 / ❌ | 用户量最大，教程最多，但抢话无解 |
| [idootop/migpt-next](https://github.com/idootop/migpt-next) | — | ⛔ 归档 | 同上，支持自定义消息回复 | ❌ | ❌ | 移除了连续对话/流式，因为"做不到" |
| [yihong0618/xiaogpt](https://github.com/yihong0618/xiaogpt) | 活跃 | ✅ **活跃** | MiNA 轮询 + `--mute_xiaoai` + 第三方 TTS | ❌ | 支持"持续对话"模式 | 已重构为 Republic 多 provider，模型接入最省心 |
| [samyeei/xiaoai-gpt](https://github.com/samyeei/xiaoai-gpt) | 5★ | ⚠️ 单次提交快照（2026-10） | LX06 → Mac → Codex App Server，WebRTC + SSH aplay | ✅ | ✅ / 半双工 | **只接 Codex**、macOS 专用、非商业许可；LX06 实测 |
| [kslr/xiaoai-plus](https://github.com/kslr/xiaoai-plus) | 中 | ⛔ 停更 | **端侧直连**：音箱本机 C++ → 豆包实时语音 API | ✅ | ✅ / ⚠️ 宣称全双工 | **无后端**、自研 4ch + WebRTC APM、自定义唤醒词；**仅 OH2P**（下游 `xiaoai-doubao` 实证其 AEC 与 ONNX 自旋在真机未跑通） |
| [rolazd/xiaoai-doubao](https://github.com/rolazd/xiaoai-doubao) | — | ✅ 活跃（`2026-11-09` 公开，已发 `v0.1.1`） | 基于 `xiaoai-plus` 深度工程重构 + 豆包 `o2` + 儿童记忆后端 | ✅ | ✅ / 半双工（`half` + 本地退出词/按键） | **仅 OH2P + 1.62.2**；实测无可用 AEC 退回半双工；修 ONNX 自旋（`~270%→~80%`）、改 `s32_shift=16`、加 KWS-AGC；另含实验 NLMS 与 `speech.usock` 分支（均未作为默认/发布主路） |
| [xinnan-tech/xiaozhi-esp32-server](https://github.com/xinnan-tech/xiaozhi-esp32-server) | 大 | ✅ 活跃 | 完整语音后端：ASR/LLM/TTS/MCP/记忆 | 配 ESP32 或小爱 Client | ✅ / ✅ | 生态最大；可作 open-xiaoai 的"大脑" |
| [78/xiaozhi-esp32](https://github.com/78/xiaozhi-esp32) | 大 | ✅ 活跃 | ESP32 端固件 | —（换硬件） | ✅ / ✅ | 路线 C 的基础 |
| [hanxi/xiaomusic](https://github.com/hanxi/xiaomusic) | 中 | ✅ | 让小爱播 NAS 音乐 | ❌ | — | 常和上面方案搭配（本地曲库） |

**补充：第三种架构「端侧直连」**

前面所有方案都假设"家里有台机器跑后端"。`kslr/xiaoai-plus` 提出了另一条路：**C++ 程序直接跑在音箱上**，麦克风/扬声器/KWS 全在设备内完成，音频自己连豆包的实时语音 API，**不需要任何中转服务器**（`rolazd/xiaoai-doubao` 后续指出 OH2P 上 `kslr` 的 WebRTC AEC 实际不可用，默认改为半双工 `duplex_mode = half`，并在 `v0.1.0` 发布包之外还试验过直接消费 `speech.usock` 的 `xiaoai_native_speaker`）。

| 架构 | 后端 | 模型自由度 | 延迟 | 代表 |
|---|---|---|---|---|
| 云端轮询 | 不需要后端，但依赖小米云 | 任意（但每次要喊小爱） | 2–5s | xiaogpt / MiGPT |
| 服务端桥接 | **需要常开机器** | 任意 OpenAI 兼容 | 1.5–3s | open-xiaoai-bridge |
| 端侧直连 | **不需要**（可选记忆后台） | **锁死豆包实时语音** | 最低 | xiaoai-plus / xiaoai-doubao |

代价是模型被锁死（`kslr/xiaoai-plus` 的 `config.ini` 为旧版 `app_id/access_token/secret_key/model=1.2.1.0`，`rolazd/xiaoai-doubao` 已迁至豆包 `protocol = o2` + `X-Api-Key`，两者均无通用 `base_url`）。所以"要不要后端"不是绝对的，而是**用模型自由度换运维成本**。

**一句话选型**：想"问得爽" → `xiaogpt`；想"像真人对话"（连续、可打断、自定义唤醒词） → `open-xiaoai` + `open-xiaoai-bridge`；想"接 Agent/MCP/家电控制" → 后端换 `xiaozhi-esp32-server`。

---

## 3. 路线 A：云端接口层（不拆机，10 分钟起跑）

### 3.1 工作原理（也是它的天花板）

```
你说"小爱同学，请介绍一下量子计算"
        │
        ▼
小爱音箱 → 小米云 ASR → 云端对话记录 (MiNA)
                              │  轮询 conversation（1~2s 一次）
                              ▼
                     xiaogpt / MiGPT（本机）
                              │
                       LLM（你的自定义模型）
                              │
                      TTS（edge/豆包/OpenAI）
                              ▼
              调用 MIoT 指令让小爱"播放"这段音频
```

核心依赖：`MiService` / `miservice`（[Yonsm/MiService](https://github.com/Yonsm/MiService)）+ 小米账号密码，通过 MIoT 规范（[home.miot-spec.com](https://home.miot-spec.com)）拿到 `ttsCommand`、`wakeUpCommand`。

### 3.2 xiaogpt 最小可用命令

```powershell
pip install -U --force-reinstall "xiaogpt[locked]"
pip install miservice==3.0.1
$env:MI_USER="你的小米ID"   # 注意：不是手机号/邮箱
$env:MI_PASS="密码"
miservice list             # 打印设备列表，找到 LX06 的 DID
$env:MI_DID="xxxxx"

# DeepSeek 示例
$env:OPENAI_API_KEY="sk-..."   # 或 REPUBLIC_DEEPSEEK_API_KEY
xiaogpt --hardware LX06 --mute_xiaoai --stream --model deepseek:deepseek-chat

# 任意 OpenAI 兼容服务（通义/Groq/PPIO/自建 vLLM…）
xiaogpt --hardware LX06 --mute_xiaoai --stream `
  --model openai:qwen-plus --api_format chat `
  --api_base https://dashscope.aliyuncs.com/compatible-mode/v1 `
  --api_key $env:QWEN_KEY

# 更好的音色 / 持续对话
xiaogpt --hardware LX06 --mute_xiaoai --tts edge
# 对音箱说："开始持续对话" / "结束持续对话"
```

### 3.3 MiGPT 的配置方式（Docker 最常见）

```js
// .migpt.js
export default {
  bot:   { name: "傻妞", profile: "性别女，性格乖巧可爱" },
  master:{ name: "我",   profile: "主人" },
  speaker: {
    userId: "987654321",
    password: "123456",
    did: "小爱音箱 Pro",
    ttsCommand: [5, 1],
    wakeUpCommand: [5, 3],
    callAIKeywords: ["请", "傻妞"],
    wakeUpKeywords: ["召唤傻妞", "打开傻妞"],
    exitKeywords:  ["退出傻妞", "关闭傻妞"],
    streamResponse: true,
  },
};
```

```bash
# .env
OPENAI_API_KEY=sk-xxx
OPENAI_MODEL=deepseek-chat
OPENAI_BASE_URL=https://api.deepseek.com/v1
```

```bash
docker run -d --env-file $(pwd)/.env -v $(pwd)/.migpt.js:/app/.migpt.js idootop/mi-gpt:latest
```

### 3.4 这条路线的硬伤（必须知道）

| 问题 | 原因 | 缓解 |
|---|---|---|
| **小爱抢话** | 原生回答和你的 TTS 同时播放；云端没有"停止"原子操作 | `--mute_xiaoai` / 播放静音音频（MiGPT 的 `kAreYouOK` hack）→ 只能"曲线救国" |
| **延迟 2–5s** | 轮询间隔 + 云端往返 + TTS 合成 | `--stream`、edge TTS、短 prompt |
| **不能真正打断** | 没有麦克风流 | 无解，只能等它说完 |
| **每轮都要唤醒** | 拿不到音频流 | xiaogpt 的"持续对话"是伪连续（靠状态机） |
| **账号风控** | 第三方客户端登录小米账号，异地/多端登录会触发验证甚至封号 | 固定出口 IP、别频繁重启、小号 |
| **API 变更即失效** | 依赖非公开 MiNA 接口 | 关注项目 issue；备选刷机路线 |

> 作者自己在 `how-it-works.md` 里写得很直白：「此问题需要通过刷机才能完美解决」。

---

## 4. 路线 B：刷机接管音频（推荐，一次到位）

### 4.1 架构（Client + Server）

```
┌──────────────────── 小爱音箱（已刷补丁固件，root + SSH）────────────────────┐
│  麦克风 ──PCM──► open-xiaoai-client (Rust)  ──WebSocket──►  │   │
│  扬声器 ◄──PCM──  (接管采集/播放，原生小爱并行运行)          │   │
└──────────────────────────────────────────────────────────────┼───┘
                                                               │ ws://LAN:4399
┌──────────────────────────── 服务端（NAS/小主机/常开 PC）──────▼─────────────┐
│  open-xiaoai-bridge                                                          │
│   VAD（起止检测）→ KWS（自定义唤醒词）→ ASR → LLM → TTS → 回传播放          │
│   可选后端：OpenAI 兼容 / 小智 AI / OpenClaw / QwenPaw                       │
│   可选 TTS：小爱原生 / 豆包 / OpenAI 兼容 / 本地 MLX-Audio                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

关键设计：**服务端不碰小米云**，音频走局域网；**原生小爱仍在**，所以"打开客厅灯"这类家电指令继续由小爱处理，AI 只接管你唤醒的那部分。这也是它比路线 A 安全的根本原因（不用登录小米账号）。

### 4.2 第一步：刷机（LX06 需拆机，30–60 分钟）

准备：能传数据的 Micro-USB 线（**不是纯充电线**）、螺丝刀/撬片、Windows + [Git Bash]、[Amlogic Flash Tool v6.0.0](https://androidmtk.com/download-amlogic-flash-tool)、补丁固件 `root_patched.squashfs`。

```bash
# 0) 装 AMLogic USB 驱动（AMLLogic driver installer.exe）
# 1) 拔电源→重新上电，立刻执行，直到打印固件版本
./update.exe identify
#    This firmware version is 0-7-0-16-0-0-0-0

# 2) 设置 U-Boot 等待 15s，并从 boot0 启动
./update.exe bulkcmd "setenv bootdelay 15"
./update.exe bulkcmd "setenv boot_part boot0"
./update.exe bulkcmd "saveenv"

# 3) 刷 system0
./update.exe partition system0 root_patched.squashfs
```

```bash
# 4) 重启后 SSH（补丁固件默认密码 open-xiaoai）
ssh -o HostKeyAlgorithms=+ssh-rsa root@192.168.31.227
```

回滚：`fw_env -s boot_part boot1` 或 `./update.exe bulkcmd "setenv boot_part boot1"`。

> ⚠️ 刷机 = 失去保修 + 极小概率变砖。A/B 双系统是你的安全网：最坏情况还能切回 `boot1`。别买二手"已刷机"设备当小白鼠。

### 4.3 第二步：音箱端装 Client

```bash
ssh root@<音箱IP>
mkdir -p /data/open-xiaoai
echo 'ws://<服务端IP>:4399' > /data/open-xiaoai/server.txt

# 一键安装 + 启动（走 gitee，国内可用）
curl -sSfL https://gitee.com/idootop/artifacts/releases/download/open-xiaoai-client/init.sh | sh

# 开机自启
curl -L -o /data/init.sh https://gitee.com/idootop/artifacts/releases/download/open-xiaoai-client/boot.sh
reboot
```

### 4.4 第三步：服务端（Docker Compose，10 分钟）

```bash
mkdir open-xiaoai-bridge && cd open-xiaoai-bridge
curl -O https://raw.githubusercontent.com/coderzc/open-xiaoai-bridge/main/config.py
curl -O https://raw.githubusercontent.com/coderzc/open-xiaoai-bridge/main/docker-compose.yml
# 国内加速：image 换成 ghcr.nju.edu.cn/coderzc/open-xiaoai-bridge:latest
docker compose up -d
docker compose logs -f     # 看到 "server started on 0.0.0.0:4399" 即成功
```

### 4.5 第四步：接你的自定义大模型

```python
# config.py
APP_CONFIG = {
    "openai": {
        "base_url": "http://192.168.31.10:8000/v1",   # 自建 vLLM / Ollama / one-api
        "api_key": "sk-xxx",
        "model": "deepseek-chat",
        "input_mode": "xiaoai_asr",   # 复用音箱原生 ASR（省算力）或 "local_asr"
        "session_key": "default",
        "system_prompt": "你是一个简洁实用的语音助手，回答口语化，控制在 200 字以内。",
        "temperature": 0.7,
        "max_tokens": 512,
        "history_max_messages": 20,   # 多轮上下文条数
        "tts_provider": "xiaoai",     # xiaoai / doubao / openai / mlx_audio
        "tts_speaker": "xiaoai",
    },
    "wakeup": {"keywords": ["小黑同学", "你好小黑"]},
    "vad": {"threshold": 0.05, "min_silence_duration": 1000},
    "audio_input": {"gain": 2.0},
}
```

```python
# 唤醒路由：自定义唤醒词 → 指定后端；也可多唤醒词 → 多模型/多 Agent
async def before_wakeup(speaker, text, source, app):
    if source == "kws" and "小黑" in text:
        await speaker.play(text="我在")
        return "openai"                      # 进入连续对话
    if source == "xiaoai" and text == "召唤小黑":
        await speaker.abort_xiaoai()         # 立刻让小爱闭嘴
        return "openai"
    if source == "xiaoai" and "问小黑" in text:
        await speaker.abort_xiaoai()
        await app.send_to_openai_and_play_reply(text.replace("问小黑", ""))
        return None
    return None                              # 其余交还原生小爱
```

启用开关（`docker-compose.yml`）：

```yaml
environment:
  - OPENAI_ENABLE=1
  - API_SERVER_ENABLE=1        # HTTP API，可远程让音箱播报（告警/自动化）
  - OPEN_XIAOAI_TOKEN=xxxx     # 生产环境务必开鉴权
```

配套能力（同一套服务端，按需开）：

| 能力 | 开关 | 用途 |
|---|---|---|
| 小智 AI 实时音频流 | `XIAOZHI_ENABLE=1` | 接 `xiaozhi-esp32-server` 的完整生态（MCP、记忆、知识库） |
| OpenClaw | `OPENCLAW_ENABLE=1` | 接本地 AI Agent，可多唤醒词路由到不同 Agent |
| QwenPaw | `QWENPAW_ENABLE=1` | 阿里 AgentScope 任务型 Agent |
| HTTP API | `API_SERVER_ENABLE=1` | `:9092`，远程播报文字/音频（监控告警、门铃、TTS 广播） |

### 4.6 ASR / TTS 选型（决定体验和成本）

| 环节 | 方案 | 延迟 | 算力 | 隐私 | 适用 |
|---|---|---|---|---|---|
| ASR | `xiaoai_asr`（复用音箱原生识别） | 低 | 0 | ❌ 音频出网 | 新手首选 |
| ASR | `local_asr` + SenseVoice-Small | 中 | 2C4G 可跑 | ✅ 全本地 | 隐私优先 |
| ASR | Paraformer / FireRedASR | 中 | 更高 | ✅ | 中文精度优先 |
| ASR | 豆包云端 ASR | 低 | 0 | ❌ | 追求准确率 |
| TTS | `xiaoai` 原生 | 最低 | 0 | ❌ | 音色一般但零成本 |
| TTS | `doubao` | 中 | 0 | ❌ | 音色最好，需火山 AK/SK |
| TTS | `openai` 兼容 | 中 | 0 | 取决于服务 | 已有 TTS 服务 |
| TTS | `mlx_audio` | 低 | Mac 本地 | ✅ | 苹果生态本地化 |

### 4.7 想更"Agent"一点：后端换 xiaozhi-esp32-server

`xiaozhi-esp32-server` 本质是一个完整的语音 Agent 后端：

- 架构：`manager-api`(8002) + `manager-web`(8001) + `xiaozhi-server`(8000 WS / 8003 OTA) + MySQL + Redis；
- **任意 OpenAI 兼容 LLM** 都能接（DeepSeek/通义/智谱/Gemini/火山…），VLLM 视觉模型、MCP 工具、声纹、记忆（mem0/PowerMem）、RAGFlow 知识库；
- 支持流式 ASR/TTS，官方称流式化后**响应快约 2.5 秒**；
- 再配合 `open-xiaoai` 的 `examples/xiaozhi`（Python 端，VAD+KWS+WebSocket）或 bridge 的 `XIAOZHI_ENABLE=1`，小爱就变成"小智"的一个终端。

代价：组件多、要吃 4C8G（本地 FunASR 时），且要维护 MySQL/Redis。个人家用建议先用 bridge 的 OpenAI 直连模式，需要 MCP/记忆时再升级。

### 4.8 体验边界（实测社区反馈）

- ✅ 连续对话、喊"小爱同学"随时打断、自定义唤醒词、多唤醒词路由多模型。
- ⚠️ 唤醒词灵敏度依赖 KWS 模型和麦克风增益，需调 `vad.threshold` / `audio_input.gain`；英文唤醒词建议拆音节（`open ai`）。
- ⚠️ 首次启动要加载 VAD/KWS/ASR 模型，等 30s 再试。
- ⚠️ 原生小爱与自定义唤醒词可能"同时响应"，靠 `before_wakeup` 里 `abort_xiaoai()` 处理。
- ⚠️ 4399 端口是**无 TLS 的裸 WebSocket**，只在局域网用，别做公网端口映射。

---

## 5. 路线 C：不动音箱（零风险，可逆）

| 方案 | 成本 | 说明 |
|---|---|---|
| ESP32-S3 + 小智固件 | ¥50–150 | 独立麦克风/喇叭，接 `xiaozhi-esp32-server` 或官方 `xiaozhi.me`；小爱完全不动 |
| 小爱当蓝牙音箱 | ¥0 | 手机/电脑连蓝牙，用任意语音助手；失去远场唤醒 |
| Home Assistant + 小米官方集成 | ¥0 | `ha_xiaomi_home` / `hass-xiaomi-miot` 做家电控制与自动化，语音入口交给手机/中控屏 |

适合：设备在保修期、不想拆机、或只想"让家里多个能对话的音箱"而不执着于用这台小爱。

---

## 6. 通用：把"自定义大模型"接到任何一条路线上

### 6.1 唯一需要的契约：OpenAI 兼容

```bash
POST {base_url}/chat/completions
{ "model": "...", "messages": [...], "stream": true, "temperature": 0.7 }
```

| 部署形态 | base_url 示例 | 适用 |
|---|---|---|
| 云厂商 | `https://api.deepseek.com/v1` | 省事、效果好 |
| 本地 Ollama | `http://192.168.31.10:11434/v1` | 断网可用、隐私 |
| 本地 vLLM / SGLang | `http://192.168.31.10:8000/v1` | 并发高、可控 |
| LM Studio | `http://<PC>:1234/v1` | 桌面快速验证 |
| 网关（one-api/new-api） | `http://<NAS>:3000/v1` | 多模型统一 key、计费、限流、故障切换 |

> 语音场景选型：优先**小参数快模型**（Qwen3-4B/8B、GLM-4-Flash、豆包 Lite、DeepSeek-chat），TTFT 比"聪明"更重要；长回答会让 TTS 拖 10s+。

### 6.2 语音场景的 Prompt 规范（很影响体感）

```
你是家庭语音助手。规则：
1. 口语化，一句话说清，最多 3 句 / 80 字；
2. 禁止 Markdown、列表、代码块、URL；
3. 数字、单位、时间读法口语化（"3.5 公里"→"三公里半"）；
4. 不确定就说"我不确定"，不要编；
5. 涉及设备控制时只输出简短确认。
```

### 6.3 记忆与工具

- 短期：`history_max_messages`（bridge）/ `streamResponse`（MiGPT）/ session key 隔离多助手。
- 长期：`xiaozhi-esp32-server` 的 mem0 / PowerMem 记忆总结；或自建向量库。
- 工具调用：xiaozhi-server 支持 MCP（客户端/服务端/接入点三种协议）与插件热加载；bridge 的 HTTP API 也能反向控制音箱播报。
- ⚠️ 工具调用会增加 0.5–2s 延迟，语音场景建议"少而准"，复杂任务路由到 Agent（OpenClaw/QwenPaw）。

---

## 7. 端到端时延预算（决定"像不像人"）

| 环节 | 典型值 | 优化手段 |
|---|---|---|
| VAD 判定说完 | 300–1000 ms | 调 `min_silence_duration`（600–1000ms） |
| ASR | 200–800 ms（本地）/ 500–1500 ms（云） | SenseVoice INT8、流式 ASR |
| LLM 首 token | 300–1500 ms | 小模型、就近机房、流式、限制 `max_tokens` |
| TTS 首包 | 200–600 ms | 小爱原生 / 流式 TTS / 边生成边播 |
| 网络 + 播放启动 | 100–300 ms | 全走局域网、固定 IP |
| **合计（首字出声）** | **约 1.5–4 s** | 流式 + 短句 + 本地 ASR 可压到 2s 内 |

对比：官方小爱的"云端固定回答"约 1–2s，但它不会思考、不会多轮。**2s 左右的 AI 回答已经是可接受体感**。

---

## 8. 实施路线图（一个周末）

**Phase 0 · 准备（30 min）**
- [ ] 确认型号丝印（LX06 / OH2P）
- [ ] 备好数据线、拆机工具、Windows + Git Bash
- [ ] 服务端选一台常开设备（NAS / 小主机 / N100），固定 IP
- [ ] 路由器给音箱分配固定 DHCP
- [ ] 备份：截图原固件版本（`identify` 输出）

**Phase 1 · 刷机（30–60 min）**
- [ ] 装驱动 → `identify` 成功
- [ ] `bootdelay 15` + `boot_part boot0` + `saveenv`
- [ ] 刷 `system0` → 重启 → SSH 成功
- [ ] 记录回滚命令（`fw_env -s boot_part boot1`）

**Phase 2 · 打通链路（30 min）**
- [ ] 音箱端 `server.txt` + `init.sh` + 开机自启
- [ ] 服务端 `docker compose up -d`
- [ ] `OPENAI_ENABLE=1`，先用云端模型验证
- [ ] 能听到 AI 回答即链路通

**Phase 3 · 接自定义模型（30 min）**
- [ ] 部署 Ollama/vLLM 或配置 one-api 网关
- [ ] 改 `base_url` / `model` / `api_key`
- [ ] 加 system_prompt、`history_max_messages`
- [ ] 验证多轮 + 打断

**Phase 4 · 调优（持续）**
- [ ] 唤醒词（中文 3–4 字最佳；英文拆音节）
- [ ] `vad.threshold` / `min_silence_duration` / `gain`
- [ ] TTS 音色（豆包音色质变）
- [ ] 多唤醒词 → 多模型路由（如"小黑"=闲聊、"小码"=写代码）
- [ ] HTTP API 接入告警/门铃播报

**验收清单**
- [ ] 连续 3 轮对话不需重复唤醒
- [ ] 说"小爱同学"能立刻打断 AI
- [ ] 原生"打开客厅灯"仍可用
- [ ] 断电重启后自动恢复
- [ ] 首字出声 < 3s

---

## 9. 风险、合规与坑

| 风险 | 说明 | 应对 |
|---|---|---|
| 变砖 / 失保 | 刷机是改系统分区 | A/B 双系统切 `boot1` 回滚；别在关键设备上试 |
| 固件漂移 | 小米新版固件可能让补丁失效 | 刷前记录版本；关注仓库 issue；别开 OTA |
| 账号风控 | **仅路线 A** 需要登录小米账号，异地登录会触发验证/封号 | 固定 IP、小号、别频繁重启；或用刷机路线彻底绕开 |
| 隐私 | `xiaoai_asr` 与云 TTS 会让音频/文本出网 | 全本地：`local_asr` + 本地 LLM + 本地 TTS |
| 安全 | 4399 裸 WS、9092 API | 只在内网、开 `OPEN_XIAOAI_TOKEN`、别做公网映射 |
| 维护性 | 原仓库已归档，社区 fork 可能再次停更 | 优先选活跃 fork；把刷机产物/配置备份到 git |
| 合规 | 破解固件属逆向，非官方支持 | 仅个人研究使用，遵守当地法律与小米用户协议 |
| 音质/回声 | 免提外放时麦克风拾到自播 | 降低音量、TTS 播报时暂停 VAD（一般已内置） |

---

## 10. 参考链接

- Open-XiaoAI（原仓库，已归档）：https://github.com/idootop/open-xiaoai
- 刷机教程：https://github.com/idootop/open-xiaoai/blob/main/docs/flash.md
- 社区维护 fork：https://github.com/coderzc/open-xiaoai
- 桥接服务端（推荐）：https://github.com/coderzc/open-xiaoai-bridge
- 部署实操（腾讯云社区）：https://developer.cloud.tencent.com/article/2716215
- MiGPT：https://github.com/idootop/mi-gpt ｜ MiGPT-Next：https://github.com/idootop/migpt-next
- xiaogpt：https://github.com/yihong0618/xiaogpt
- MiService：https://github.com/Yonsm/MiService ｜ MIoT 规范：https://home.miot-spec.com
- xiaozhi-esp32-server：https://github.com/xinnan-tech/xiaozhi-esp32-server
- xiaozhi-esp32 固件：https://github.com/78/xiaozhi-esp32
- 小米音箱本地音乐：https://github.com/hanxi/xiaomusic
- 底层研究：https://github.com/jialeicui/open-lx01 ｜ https://github.com/duhow/xiaoai-patch

---

## 11. 补充评估：zongzhu-bot（宗主）

仓库：https://github.com/Liu-zhongxian/zongzhu-bot （2026-10-16 创建，v2.5.2.9，257★ / 95 fork，MIT，活跃）

**它是什么**：不是又一个刷机方案，而是**「小智生态」的二创整合版** —— 把 `78/xiaozhi-esp32`（固件）和 `xinnan-tech/xiaozhi-esp32-server`（后端）整仓 vendor 进来，在上面加了一层**网关**：

```
ESP32-S3 圆屏板 ─┐                      电脑后端（:8003 /gateway）
                  ├─ 语音 ─▶ X-ASR ─▶ 网关 ┬ 点歌/暂停/音量 → 本机 mpv 执行
小米智能音箱 Pro ─┘                        └ 其余交给 Grok Bot(Webhook) → 朗读回复
```

**三个真正值得借鉴的设计**

1. **刷机流水线比原版安全一个量级**（见 `zongzhu-speaker/`）：

| 环节 | `open-xiaoai` 原版 | `zongzhu-speaker` |
|---|---|---|
| 固件来源 | 下载别人做好的 `root_patched.squashfs` | 用 SN 查官方 OTA 包（免登录）→ 自己打 4 个 patch |
| 完整性 | 无 | 校验 CRC32+MD5；要求**只有 8 个文件**不同、条目/权限/属主不变 |
| 刷前 | 直接刷 | **只读备份 6 个分区**，先判断当前跑的是 A 还是 B |
| 写入 | 写 `system0` | **只写非活动槽**，活动槽保持官方 |
| 校验 | 无 | 每一步**读回逐字节 cmp**，全过才切 `boot_part` |
| 密码 | 固定默认 | **用户自设** root 密码 |

  对 LX06 同样适用（分区结构一致），这是目前公开文档里最稳的刷机姿势。
2. **网关 + 异步任务模型**：语音不是"一问一答"，而是"派活" —— 提交 Webhook、屏幕显示已等秒数、断线补发、说话中途打断后接着念。这比单纯接聊天模型更接近"助手"。
3. **可运维性**：固件签名校验（公钥写死）、板型/芯片/版本三重校验拒绝错刷、网关页改唤醒词免重刷、token 鉴权。

**边界与坑**

- ⚠️ 官方口径只写了 **小米智能音箱 Pro（OH2P）+ 固件 1.62.2** 完整验证；但**LX06 在原理上同一套**（同为 Amlogic + mico squashfs + A/B 双系统 + 同架构 armv7 客户端），作者也确认可用。真正的差异点见下方「LX06 适配四件事」。
- ⚠️ **Mac-first**：刷机（`update` + libusb）和点歌（mpv / MLX）都按 Apple Silicon 写；Docker 里摸不到 USB。LX06 走 Windows 时用 Amlogic Flash Tool 的 `update.exe` 替代。
- ⚠️ 2026-10 才建仓、单人维护，跟上游同步靠 `UPSTREAM.json` 记基线，长期维护性未知。
- ⚠️ Bot 需要"能往这台电脑写文件"，**语音能触发 Bot 的全部权限**（发消息、改文件、花钱）。
- ⚠️ 音箱端无屏幕、无 MCP 工具，网关上调不了它的音量/亮度。

#### 作者对 LX06 的原话与含义

> 「小爱音响PRO（型号：LX 06）也是可以的，**只需要保证系统版本号相同**就好了」
> —— 作者 X 长文《全网首发：我把 Grok Bot 装进了小爱音箱！》

这句话的准确含义是**版本三件套必须一致**：你下载的官方包版本 = 音箱当前运行版本 = 刷进去的内核版本。作者自己就是 1.60.3 查不到完整包 → 先在米家升到 1.62.2 → 再用 1.62.2 的官方包做补丁。所以对 LX06 而言，要匹配的是 **LX06 自己的版本线（1.94.x）**，不是照抄 OH2P 的 1.62.2。

但"版本相同"只保证**镜像能用**，不保证**音频能通**。作者同时说明「其他型号、版本和 Windows 我没有完整试过」——所以 LX06 属于"口头认可、未实测"，下面四件事仍需自己做。

#### LX06 适配四件事（代码级核对结果）

1. **OTA 型号**：`zongzhu-speaker/firmware/ota_query.py` 里 `MODEL = "OH2P"` 写死 → 改 `LX06`。上游 `client-patch/src/ota.ts` 的型号表就是 `["LX06","OH2P"]`，接口 `/grayupgrade/v2/{model}` 是型号参数化的，LX06 的官方包版本约 **1.94.13**（OH2P 是 1.58/1.62 系）。
2. **音频补丁（关键，zongzhu 仓库里没有）**：`coderzc/open-xiaoai` 的 `packages/client-patch/patches/LX06/` 有 3 个专用补丁 —— `01-asound.patch`（把 `period_size 480 / buffer_size 3840` 改成 `384 / 6144`）、`02-asound-dts.patch`、`03-libxaudio_engine.sh`。LX06 是 6 麦阵列、声卡与 DTS 配置和 OH2P 不同，**不打这 3 个补丁，麦克风/回采通路不对**。
3. **分区大小写死**：`build_image.sh` 的 `SYSTEM_MAX=0x2800000`(40MB)、`flash_slot.sh` 的 readback `0x600000 / 0x2800000`、网关 `flash.py` 的 `PARTITIONS`（注释明写"OH2P 实测"）都要换成 LX06 的 `/proc/mtd` 实测值，否则读回长度校验直接失败。
4. **改动清单校验**：`build_image.sh` 断言"只有 8 个文件不同"，这是 OH2P 1.62.2 的基线；LX06 加上音频补丁后是另一组文件，**不改这个断言会误报"不要刷"**。

**给你的结论**

- **LX06 可用，但不是开箱即用**：正确组合是「coderzc 的 LX06 补丁 + zongzhu 的刷机安全流程 + bridge/宗主后端」。
- 若音箱底部是 **OH2P** → 直接用它或 `coderzc/open-xiaoai`，zongzhu 的刷机脚本更稳妥。
- 刷机物理差异：LX06 要**拆机 + Micro-USB 调试口 + Windows 版 Amlogic 工具**（zongzhu 的网页刷机只认 macOS）。
- 若你只是想要"桌面机器人 + 音箱双终端" → 买块 ESP32-S3 跑宗主固件，小爱完全不刷，零风险。
- 若你只想要 Agent 能力 → 不必换栈，bridge 的 HTTP API + 多唤醒词路由已能实现"派活"，参考它的任务状态机即可。

---

*本文为技术调研，不构成对任何固件破解行为的鼓励。刷机有风险，操作前请评估保修与设备价值。*
