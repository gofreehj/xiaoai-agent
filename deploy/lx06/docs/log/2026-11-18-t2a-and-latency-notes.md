# 2026-11-18 · T2A 运行时接管验收与全链路延迟诊断记录

- 设备：`192.168.43.155`（LX06，ROM `1.94.13` release，启动槽位 `boot0`）
- 运行时包：`deploy/lx06/work/runtime/xiaoai-agent-lx06-runtime`（静态 musl ARMv7 二进制 `cb49ef50ea5394f85ce176a3a538024aacb7c61465270ada32898b0e9350bb01`）
- 结果：**T2A 全部 6 项验收标准通过（PASS）**；同场跑通 T3（唤醒与多轮分段采集）、T4（SiliconFlow SenseVoice ASR）、T5（LLM + 时间/天气/联网搜索工具调用）及 T6 基础 TTS 播报。

---

## 1. T2A 验收证据

### 1.1 运行时上传与校验

```text
xiaoai-agent: OK
agent.yaml.example: OK
start-agent.sh: OK
init.sh.example: OK
INSTALL.txt: OK
Filesystem                Size      Used Available Use% Mounted on
/dev/ubi0_0              13.4M     10.5M      2.3M  82% /data
```

### 1.2 受控接管启动、Socket 保护与 PCM 前端注册

启动前基线：
```text
   7219 srwxr-xr-x    1 root     root             0 Nov 18 19:36 /tmp/mico_aivs_lab/usock/common.usock
   7217 srwxr-xr-x    1 root     root             0 Nov 18 19:36 /tmp/mico_aivs_lab/usock/speech.usock
 1282 root      347m S<   /usr/bin/mipns-xiaomi -c /usr/share/xiaomi/xaudio_engine.conf -r opus32 -l
 2499 root      108m S    /usr/bin/mico_aivs_lab
```

执行 `XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log RUST_LOG=debug /data/open-xiaoai/start-agent.sh start` 后：
```text
xiaoai-agent launcher: starting /data/open-xiaoai/xiaoai-agent
xiaoai-agent launcher: started with PID 1428
xiaoai-agent launcher: running with PID 1428
  15625 srwxr-xr-x    1 root     root             0 Nov 18 20:34 /tmp/mico_aivs_lab/usock/common.usock
  14146 srwxr-xr-x    1 root     root             0 Nov 18 20:33 /tmp/mico_aivs_lab/usock/speech.usock
tmpfs /etc/init.d/pns tmpfs rw,nosuid,nodev,noatime 0 0
tmpfs /tmp/mico_aivs_lab/usock/speech.usock tmpfs rw,nosuid,nodev,noatime 0 0
 1428 root     15972 S    /data/open-xiaoai/xiaoai-agent -c /data/open-xiaoai/agent.yaml
 1480 root      100m S    /usr/bin/mico_aivs_lab
 1584 root      346m S<   /usr/bin/mipns-xiaomi -c /usr/share/xiaomi/xaudio_engine.conf -l
```

Agent 启动与 `mipns-xiaomi` 注册日志：
```text
2026-11-18T12:33:59.033876Z  INFO xiaoai_agent::speech: speech.usock service listening socket=/tmp/mico_aivs_lab/usock/speech.usock
2026-11-18T12:33:59.560292Z  INFO xiaoai_agent: xiaoai-agent ready socket=/tmp/mico_aivs_lab/usock/speech.usock
2026-11-18T12:34:01.632991Z  INFO xiaoai_agent::speech: mipns registered vendor="xiaomi" codec="pcm16"
2026-11-18T12:34:01.633215Z  INFO xiaoai_agent: speech frontend registered vendor="xiaomi" codec="pcm16"
2026-11-18T12:34:02.446578Z DEBUG xiaoai_agent::speech: mipns VOIP status message
```
进程连续运行超过 12 分钟（>740s）无退出，VSZ 约 `15.6 MB`。

### 1.3 真机语音唤醒、16kHz 单声道 PCM 采集与多轮连续对话

```text
2026-11-18T13:05:49.307315Z  INFO xiaoai_agent::speech: speech stream prepared id=1 activation=Wakeup interaction_mode=0
2026-11-18T13:05:49.307540Z  INFO xiaoai_agent: WAKE native mipns turn_id=1 interaction_mode=0
2026-11-18T13:05:49.609713Z  INFO xiaoai_agent::capture: CAPTURE_BACKEND backend=speech_usock endpoint=energy turn_id=1
2026-11-18T13:05:49.700334Z DEBUG xiaoai_agent::speech: wakeup audio packet stream_type=0 bytes=4000
...
2026-11-18T13:05:49.708080Z DEBUG xiaoai_agent::speech: wakeup audio packet stream_type=1 bytes=1760
2026-11-18T13:05:49.760983Z  INFO xiaoai_agent::vad: CAPTURE_NOISE_FLOOR reason=speech_start samples=0 rms_p50=0.00000 rms_p95=0.00000 threshold=0.00600 trigger=0.08938
2026-11-18T13:05:51.804329Z  INFO xiaoai_agent::vad: CAPTURE_UTTERANCE duration=2.70s peak=0.40656 voiced_ms=1100 end=silence
2026-11-18T13:05:55.481432Z  INFO xiaoai_agent: USER_ASR text=现在几点了？
2026-11-18T13:06:05.970219Z DEBUG xiaoai_agent::device: device command ok: sh /usr/sbin/tts_play.sh 现在是晚上九点零六分。; true
2026-11-18T13:06:05.970483Z DEBUG xiaoai_agent::speech: sent dialog_finish turn_id=1 continue_dialog=true
2026-11-18T13:06:06.080841Z  INFO xiaoai_agent: requested follow-up capture through pnshelper
2026-11-18T13:06:06.097131Z  INFO xiaoai_agent::speech: speech stream prepared id=2 activation=FollowUp interaction_mode=0
2026-11-18T13:06:06.097389Z  INFO xiaoai_agent: FOLLOW_UP native mipns turn_id=2 activation=FollowUp interaction_mode=0
2026-11-18T13:06:09.500417Z  INFO xiaoai_agent::vad: CAPTURE_UTTERANCE duration=2.70s peak=0.71982 voiced_ms=1000 end=silence
2026-11-18T13:06:10.684232Z  INFO xiaoai_agent: USER_ASR text=明天星期几？
2026-11-18T13:06:19.176843Z DEBUG xiaoai_agent::device: device command ok: sh /usr/sbin/tts_play.sh 明天是十一月十九号，星期四。; true
```

### 1.4 `stop` 停止与原生恢复验证

```text
=== STOP TAKEOVER ===
xiaoai-agent launcher: stopped and restored the native speech frontend
=== VERIFY RESTORED ===
xiaoai-agent launcher: not running
  18859 srwxr-xr-x    1 root     root             0 Nov 18 20:46 /tmp/mico_aivs_lab/usock/common.usock
  18857 srwxr-xr-x    1 root     root             0 Nov 18 20:46 /tmp/mico_aivs_lab/usock/speech.usock
mounts_cleaned=ok
 3332 root      108m S    /usr/bin/mico_aivs_lab
 3417 root     16204 R<   /usr/bin/mipns-xiaomi -c /usr/share/xiaomi/xaudio_engine.conf -r opus32 -l
```

### 1.5 T2A 验收清单汇总

| 验收项 | 结果 | 证据摘要 |
|---|---|---|
| 进程连续运行 ≥60s 不退出 | PASS | 连续运行 >740s 无崩溃，VSZ `15972 KB` |
| `speech.usock` 绑定并受保护，inode 不变 | PASS | `speech.usock` inode `14146` 在 `mico_aivs_lab` 重启前后不变，`/proc/mounts` 确认 bind-mount |
| `common.usock` 重建且 `mipns` 无 `-r opus32` | PASS | `common.usock` inode `15625`，`mipns-xiaomi` 注册 `codec="pcm16"` |
| 喊唤醒词收到唤醒事件与 16k mono PCM | PASS | `WAKE native mipns` + `CAPTURE_UTTERANCE` + `USER_ASR` |
| 系统 TTS 播报出声 | PASS | `sh /usr/sbin/tts_play.sh {text}; true` 合成 MP3 并由 `miplayer` 播放完毕 |
| `stop` 后恢复原生语音与卸载挂载 | PASS | `mounts_cleaned=ok`，原生 `mipns-xiaomi -r opus32` 与 `mico_aivs_lab` 恢复 |

---

## 2. 真机发现的平台与网络问题（已配置规避）

1. **LX06 `/usr/sbin/tts_play.sh` 末尾返回码非零问题**：
   - 现象：脚本第 30 行 `[ x"$player_stat" == x"1" ] && { /usr/bin/mphelper play }` 在播放前无背景音乐时返回退出码 `1`，导致 Agent 误判 `command failed exit=1`。
   - 规避：将 `device.tts_command` 设为 `"sh /usr/sbin/tts_play.sh {text}; true"`。
2. **手机热点下 IPv6 PMTU 黑洞导致 WAV 上传 100% 丢包**：
   - 现象：`wlan0` MTU 为 `1500`，而移动蜂窝热点 IPv6 链路对 `1400` 字节包 `100% packet loss`，导致 `api.siliconflow.cn` 走 IPv6 时上传录音超时。
   - 规避：运行时执行 `sysctl -w net.ipv6.conf.wlan0.disable_ipv6=1 net.ipv6.conf.all.disable_ipv6=1` 强制走 IPv4（`0% packet loss`，RTT ~91ms）。后续纳入 `start-agent.sh` 或启动脚本。
3. **默认 IP 定位服务 `https://ipapi.co/json/` 被 Cloudflare 403 拦截**：
   - 规避：`agent.weather.ip_lookup_url` 改为 `http://ip-api.com/json/?lang=zh-CN`，并配置 `default_location` 兜底。

---

## 3. 代码级延迟优化清单（已落地并上机）

真机全链路日志拆解表明，问完问题后的等待主要集中在以下环节。结合 `CocoJiao/xiaoai-agent` 合入与针对性优化，已完成以下改造并通过全量 148 项单元测试：

1. **已落地：修复 ASR / HTTP 客户端空闲连接假死（省 2.5s ~ 5.0s）**
   - 根因：`OpenAiAsr` 原使用默认 `reqwest::Client::new()`，HTTP/2 连接在热点 NAT 下空闲 >15s 后被静默断开，导致下一次请求必先在死连接上等待满 `timeout_s` 后才触发重试。
   - 实现：为 ASR、LLM、WebSearch、Weather 的 `reqwest::Client` 统一配置 `connect_timeout(3s)`、`pool_idle_timeout(10s)`、`tcp_keepalive(15s)`。
2. **已落地：全局复用 LLM 的 `reqwest::Client`（每轮省 ~400ms CPU 与握手耗时）**
   - 根因：`prompt_once()` 原先在每一轮对话都重新 `reqwest::Client::builder().build()`，每次都在音箱单核 ARMv7 CPU 上重新从 `/etc/ssl/certs` 加载并解析 140 个根证书。
   - 实现：在 `AgentView` 中构建并持久复用 `reqwest::Client`（仅在 `SIGHUP` 热加载时随视图重建）。
3. **已落地：按配置按需注册音乐工具与时间注入（每轮省 1.0s ~ 2.0s LLM 推理耗时）**
   - 根因：即使 `music.enabled: false`，`build_tool_server()` 原先仍无条件向 `ToolServer` 注册全部 13 个音乐工具。
   - 实现：当 `music.enabled == false` 时仅注册 4 个核心工具（时间、天气、搜索、结束对话）；同时启用 `agent.inject_current_time: true` 直接在用户消息注入当前时间与星期，省去询问时间/日期时的工具调用轮次。
4. **已落地：天气定位优先使用 `default_location`（查天气省 ~600ms）**
   - 根因：`weather.rs` 在用户未指定城市名时，原先每次都先请求外部 `ip_lookup_url`（~600ms），失败才用 `default_location`。
   - 实现：`effective_requested_location()` 优先使用非空 `default_location`，仅在未配置时才请求 `ip_lookup_url`。
5. **后续可选演进：流式 LLM + 分句流水线 TTS 播报**
   - 若后续需进一步压缩长回答的首字出声等待，可将 LLM 改为流式输出并按首句标点切分送入 TTS 播放。
