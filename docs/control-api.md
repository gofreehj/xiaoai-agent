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
| `GET /` | 4-Tab 单页 Web 管理后台（总览控制 / 对话管理 / 配置管理 / 诊断日志），无外链、Token 仅存浏览器 `localStorage` |
| `GET /health` | `exclusive` 判 `speech.usock` inode 归属（503 时说明失权）；`dual` 直接返回 200 及 `native_interlock` 实时互锁状态 |
| `GET /status` | 同上 + `pid` + `mode` + 配置路径 + 阶段/最近一轮时延 + `native_interlock` |
| `GET /music/status` | 当前音乐播放状态与待播队列（最多 10 首；音乐服务不可用时 503） |
| `GET /history` | 读取内存对话记录（`?limit=1..150`）及 LLM 短期上下文水位（`context_turns` / `context_ttl_s`），不写 `/data` 闪存 |
| `GET /config` | 读取当前磁盘配置（所有密钥/Token 脱敏为 `__REDACTED__`）、常用表单视图、`autostart` 标记状态、启动告警与待重启段 |
| `GET /doctor` | 在线运行自检：`/tmp` 与 `/data` 剩余空间、配置告警、互锁文件检查及 ASR/LLM/天气/MCP/KWS 并行连通性探测 |
| `GET /logs` | 只读读取运行日志末尾 64 KiB（`?lines=1..500&level=&q=`，默认 `/tmp/xiaoai-agent.log`） |
| `GET /metrics/latency` | 读取最近 `1..100` 轮全链路耗时明细（VAD静音/ASR/LLM首字与首句/工具次数/TTS/体感E2E）及各阶段 `avg/p50/p95/max` 聚合统计 |
| `POST /stop` | 打断当前会话并停止全部输出（TTS + 音乐），幂等 |
| `POST /speak` | body `{"text":"..."}`：让音箱朗读这段文字（排队执行，支持 TTS 试听） |
| `POST /chat` | body `{"text":"..."}`：把文字当作用户提问，走完整 LLM → TTS 管线（排队执行，返回 `{"ok":true,"queued":true}`） |
| `POST /reload` | 等价 `SIGHUP`，热加载 `agent.yaml` |
| `POST /volume` | body `{"action":"up"\|"down"\|"mute"\|"set","value":0-100}`；`set` 必须带 `value`，否则 400 |
| `POST /music/pause` `resume` `next` `previous` `stop` | 音乐控制 |
| `POST /history/clear` | 清空 LLM 短期上下文；传 `{"clear_log":true}` 时一并清空内存对话记录列表 |
| `POST /history/delete` | body `{"id":123}`：删除单条内存对话记录 |
| `POST /config/form` | 提交常用热配置表单，先校验再原子写入 `agent.yaml`（自动留一份 `0600` 的 `.bak`），热段自动触发重载 |
| `POST /config/yaml` | body `{"yaml":"...","validate_only":false}`：校验或保存完整 YAML（自动回填 `__REDACTED__` 原密钥） |
| `POST /config/rollback` | 从 `agent.yaml.bak` 校验并原子恢复上一版配置，随后触发热重载 |
| `POST /autostart` | body `{"enabled":true\|false}`：删除或创建配置同目录下的 `disabled` 标记文件，即时控制 `/data/init.sh` 开机自启 |
| `POST /metrics/latency/clear` | 清空内存中的全链路耗时采样记录 |

所有写操作**只接受 POST**（GET 返回 405），且除 `/` 外的所有 API 均由路由级中间件 `require_token` 统一做 Bearer 鉴权。`exclusive` 与 `dual` 两种模式都已接线。未接线的端点返回 `503`（`/stop` 除外）。在 `dual` 模式下，当 `NativeWakeMonitor` 检测到原生小爱被唤醒时，正在执行的 `/speak` / `/chat` 任务会立即被中止并静默让位，且原生活跃或冷却保护窗内新到达的 `/speak` / `/chat` 请求会被自动跳过（避免与原生小爱同时出声）。

鉴权：`Authorization: Bearer <token>`（`token` 为空时不校验）。

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/health
# {"status":"ok","mode":"exclusive","uptime_s":128,"socket":{"path":"/tmp/.../speech.usock","owned":true},
#  "speech":{"last_packet_age_s":3,"service_restarts":0}}
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"text":"今天天气怎么样"}' http://127.0.0.1:8765/chat
curl -s -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"action":"set","value":40}' http://127.0.0.1:8765/volume
```

- `exclusive` 模式下，`/health` 判的是 **inode 归属**，不是「进程还在」：让位、被原生抢回、socket 被删掉重建，三种「管线已死但进程活着」都会让它返回 **503**（`status: socket_not_owned`）。
- `dual` 模式下，进程不接管 `speech.usock`，`/health` 返回 **200** 及外部 KWS 地址与 `native_interlock` 实时互锁状态（`idle` / `active` / `cooldown` / `disabled`）。

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
