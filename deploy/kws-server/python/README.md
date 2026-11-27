# 外部 KWS 参考实现 #2：通用 Python / Docker 服务端（NAS · 树莓派 · PC · Mac）

本目录提供一套不绑定任何路由器或专用硬件的 **通用 Sherpa-ONNX UDP XPCM KWS 服务端**，实现 [docs/external-kws-protocol.md](../../../docs/external-kws-protocol.md)。
只要局域网内有一台 NAS、树莓派、Linux 主机或开发电脑，几分钟即可为小爱音箱开启 `dual` 双助手模式（保留原生小爱 + 自定义热词唤醒大模型）。

---

## 1. 准备唤醒模型与热词表

以官方开源 3.3M 中英文流式唤醒模型 `sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20` 为例：

```bash
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2
tar -xjf sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2
```

新建 `keywords.txt`（中文需使用带声调拼音音素，**不要写 `#` 注释行**，且不要与“小爱同学”重叠）：

```text
d òu b āo d òu b āo @豆包豆包
t uì ch ū @__CMD_EXIT__
```

---

## 2. 直接运行（Python）

```bash
pip install -r requirements.txt
python3 server.py \
  --model-dir ./sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20 \
  --keywords-file ./keywords.txt \
  --port 5005
```

或使用 Docker：

```bash
docker build -t xiaoai-kws-server .
docker run --rm -p 5005:5005/udp \
  -v $(pwd)/sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20:/models:ro \
  -v $(pwd)/keywords.txt:/keywords.txt:ro \
  xiaoai-kws-server --model-dir /models --keywords-file /keywords.txt
```

---

## 3. 配置音箱并验证连通性

在音箱的 `agent.yaml` 中启用 `dual` 模式并填入该主机地址：

```yaml
runtime:
  mode: dual

dual:
  kws:
    endpoint: "<服务器局域网IP>:5005"
```

在音箱上执行一键探测：

```sh
/data/open-xiaoai/xiaoai-agent doctor -c /data/open-xiaoai/agent.yaml
# 看到 external_kws: reachable (X ms) 即说明 UDP XPCM 心跳通路正常
```
