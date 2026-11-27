# 外部 KWS 参考实现 #1：红米 AX5 路由器（Sherpa-ONNX 3.3M）

本目录是 [通用外部 KWS 协议（UDP XPCM）](../../../docs/external-kws-protocol.md) 的**路由器端参考实现之一**：在 **红米 AX5**（IPQ60xx，4 核 Cortex-A7/A53 `armv7l` musl，~95 MB 可用 RAM）上常驻运行 **Sherpa-ONNX 3.3M Zipformer**，监听 UDP `:5005` 接收任意小爱音箱（`runtime.mode: dual`）发来的音频帧，并将唤醒结果原路回传给发送方。

> 其他平台（NAS / Docker / 树莓派 / Mac / PC）推荐使用更轻便的 **[Python 通用参考服务端](../python/README.md)**。

---

## 1. 工作模式与音箱配置

自单二进制架构重构起，音箱端 **无需再安装 `lx06-kws-relay` 或 `ax5_kws.env`**，`xiaoai-agent` 原生内置音频采集与 UDP XPCM 客户端。只需在音箱的 `agent.yaml` 中启用 `dual` 模式并指向 AX5 地址即可：

```yaml
runtime:
  mode: dual

dual:
  kws:
    endpoint: "192.168.31.1:5005" # 红米 AX5 的局域网 IP:端口
```

---

## 2. 关键技术细节（AX5 实测）

1. **Musl 路由器运行 Glibc C++ 动态库**：
   AX5 固件为 `musl`，而 `sherpa-onnx` 官方预编译库依赖 `glibc`。打包脚本将 Debian `armhf` 的 `glibc 2.36` 运行时、C++ 库与 3.3M 模型打进 `/data/kws/kws_runtime.tar.gz`（~14 MB），开机由 `/data/kws/start-ax5-kws.sh` 解压到 `/tmp/kws/`（tmpfs）运行。
2. **标准 UDP 原路回包**：
   `ax5_kws_server.c` 默认 `--wake-port 0`，即直接回包（`XPONG\n` / `WAKE:<seq>:<kw>\n`）给发送端 UDP 源地址端口 `src_addr`；天然支持多台音箱同时连接及 `xiaoai-agent doctor` 并发探测。
3. **唤醒词音素格式（必须带声调）**：
   - `d òu b āo d òu b āo @豆包豆包` ✅
   - `d ou b ao d ou b ao @豆包豆包` ❌（缺少声调）
   - `dual` 模式下请勿在 `keywords.txt` 启用“小爱同学”，避免与原生小爱抢答。
   - **禁止在 `keywords.txt` 中写入 `#` 注释行**（`sherpa-onnx` 会把 `#` 解析为单条阈值）。

---

## 3. 文件清单

- `src/ax5_kws_server.c`：AX5 端 UDP XPCM KWS 服务端（动态 `dlopen` 加载 `libsherpa-onnx-c-api.so`，支持 400ms pre-roll、系统回声抑制窗口与双包防丢）。
- `keywords.txt`：带声调拼音唤醒词配置。
- `start-ax5-kws.sh`：AX5 启停管理脚本（`start | stop | restart | status`）。
- `deploy_to_ax5.py`：一键交叉编译并部署到 AX5 路由器的脚本。

---

## 4. 日常运维

```sh
# 路由器端查看状态与日志
/data/kws/start-ax5-kws.sh status
tail -f /tmp/ax5-kws.log

# 音箱端探测 AX5 KWS 连通性与 RTT
/data/open-xiaoai/xiaoai-agent doctor -c /data/open-xiaoai/agent.yaml
```
