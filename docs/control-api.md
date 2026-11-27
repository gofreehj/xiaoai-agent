# 本地控制 API（`server` 段）

默认**关闭**：`server.enabled: false` 时进程不 bind 任何端口。开给 Home Assistant 或运维用，才打开。

```yaml
server:
  enabled: true
  host: 127.0.0.1     # 只绑回环；要给 HA 用就填内网地址
  port: 8765
  token: "换成一个足够长的随机串"
```

`server` 段属于**改完要重启**的段（监听器启动时 bind）。`token` 为空时：

| host | 结果 |
|---|---|
| 回环（`127.0.0.1` / `localhost` / `::1`） | 允许启动，不鉴权 |
| 非回环（含 `0.0.0.0`） | **拒绝启动控制面**（语音不受影响，见下） |

## 端点

| 端点 | 说明 |
|---|---|
| `GET /health` | 活体判据：`speech.usock` 是否仍归本进程所有 |
| `GET /status` | 同上 + `pid` + 配置路径 + 阶段/最近一轮时延 |
| `GET /music/status` | 当前音乐播放状态（音乐服务不可用时 503） |
| `POST /stop` | 打断当前会话并停止全部输出（TTS + 音乐），幂等 |
| `POST /speak` | body `{"text":"..."}`：让音箱朗读这段文字（排队执行） |
| `POST /chat` | body `{"text":"..."}`：把文字当作用户提问，走完整 LLM → TTS 管线（排队执行，返回 `{"ok":true,"queued":true}`） |
| `POST /reload` | 等价 `SIGHUP`，热加载 `agent.yaml` |
| `POST /volume` | body `{"action":"up"\|"down"\|"mute"\|"set","value":0-100}`；`set` 必须带 `value`，否则 400 |
| `POST /music/pause` `resume` `next` `previous` `stop` | 音乐控制 |
| `POST /history/clear` | 清空对话历史 |

所有写操作**只接受 POST**（GET 返回 405），且和读接口一样受 Bearer 鉴权。`exclusive` 与 `dual` 两种模式都已接线。未接线的端点返回 `503`（`/stop` 除外）。在 `dual` 模式下，当 `NativeWakeMonitor` 检测到原生小爱被唤醒时，正在执行的 `/speak` / `/chat` 任务会立即被中止并静默让位，且原生活跃或冷却保护窗内新到达的 `/speak` / `/chat` 请求会被自动跳过（避免与原生小爱同时出声）。

鉴权：`Authorization: Bearer <token>`（`token` 为空时不校验）。

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/health
# {"status":"ok","uptime_s":128,"socket":{"path":"/tmp/.../speech.usock","owned":true},
#  "speech":{"last_packet_age_s":3,"service_restarts":0}}
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"text":"今天天气怎么样"}' http://127.0.0.1:8765/chat
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"set","value":40}' http://127.0.0.1:8765/volume
```

`/health` 判的是 **inode 归属**，不是「进程还在」：让位、被原生抢回、socket 被删掉重建，三种「管线已死但进程活着」都会让它返回 **503**（`status: socket_not_owned`）。

`last_packet_age_s` / `service_restarts` 是**诊断字段**，不参与 200/503 判定——没人说话时长时间没有音频包是正常的。

控制台页面把「10 分钟内有过音频包」当作**活跃**，超过则显示「在线 · 静默」（空心绿点）。这个 600 秒阈值**只存在于页面里**（`panel.html` 的 `QUIET_S`），服务端不返回、也不做任何判断——改它只影响文案，不影响 API 行为。

## 排障：三种「控制面不可用」的含义不同

| 现象 | 含义 | 往哪查 |
|---|---|---|
| **connection refused** | 控制面**没启动**（`enabled: false`，或配置非法被拒） | `agent.yaml` 的 `server` 段；日志里找 `控制面启动失败` / `控制面已禁用` |
| **503**（`socket_not_owned`） | 控制面活着，但 `speech.usock` 不归本进程了（让位 / 被原生抢回 / socket 被删） | 语音链路，不是控制面配置 |
| **401** | token 不匹配或没带 | `Authorization: Bearer <token>` |

**配置写错不会让音箱失能**：控制面起不来时只记一条 `error!` + 一条 `warn!`，语音照常工作。这是刻意的取舍——安全上没有损失（端口压根没被监听），而故障半径从「整机哑」缩到「控制功能不可用」。`main.rs` 的 `a_misconfigured_control_plane_does_not_take_the_agent_down` 钉住了这个决定。

## 未实现

`/play/*`、`/wakeup` 还没做。

## 自检

`xiaoai-agent doctor -c agent.yaml` 离线检查配置警告、socket/PID、`/tmp` 剩余空间，并探测 ASR/LLM/天气/MCP 的连通性与 RTT；`dual` 模式下还会探测外部 KWS UDP 连通性并输出 `native_interlock` 监听源与日志文件状态。
