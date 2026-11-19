# 小爱音箱 Pro (LX06) 保姆级刷机与大模型部署指南

> **适用机型**：小爱音箱 Pro（型号 **LX06**，黑色大音箱，底部标贴印有 `LX06`）。  
> **适用固件**：官方固件 `1.94.13`（已在此版本完整验证并跑通全链路）。  
> **零基础承诺**：无需编译代码、无需安装 Linux 虚拟机、无需手动打包镜像。所有产物均提供开箱即用的预构建包。

---

## 目录
1. [刷机原理与防翻车保证（为什么绝不会变砖？）](#1-刷机原理与防翻车保证)
2. [前期物料与工具准备](#2-前期物料与工具准备)
3. [第一步：拆机与插线（仅需拧下螺丝）](#第一步拆机与插线)
4. [第二步：安装驱动与刷机（Windows 傻瓜式操作）](#第二步安装驱动与刷机)
5. [第三步：开机、配网与 SSH 连通确认](#第三步开机配网与-ssh-连通确认)
6. [第四步：部署 XiaoAI-Agent（变身大模型助手）](#第四步部署-xiaoai-agent)
7. [后悔药：如何一秒切回原厂官方系统？](#后悔药如何一秒切回原厂官方系统)

---

## 1. 刷机原理与防翻车保证

很多朋友担心刷机变砖。LX06 拥有硬件级的 **A/B 双系统槽位（Slot 0 与 Slot 1）**：
* 你的官方原厂系统完好无损地保存在其中一个槽位（例如 Slot 1）；
* 我们的刷机脚本**只写入空闲的另一个槽位（Slot 0）**；
* 写入后会立刻把数据从音箱中逐字节读回比对校验，只有 100% 吻合才会切换启动分区；
* **极度安全**：哪怕刷机中途中断，拔电重启依然会从官方原系统启动；即使刷完不想用了，一条命令就能瞬间切回官方系统。

---

## 2. 前期物料与工具准备

### 硬件准备
1. **小爱音箱 Pro (LX06)** 音箱本体及原装电源适配器。
2. **Micro-USB 数据线** 一根：
   > ⚠️ **非常重要**：必须是能传输数据的数据线（能连手机传文件那种），**绝不能用**买小风扇送的两芯纯充电线！
3. **十字螺丝刀** 一把（小号，拆底部螺丝用）。
4. **Windows 电脑** 一台（Win10 / Win11 均可）。

### 软件包准备（可在 GitHub Releases 直接下载）
从本项目的 [Releases 发布页面](https://github.com/stevenjoezhang/xiaoai-agent/releases) 下载以下三个整合包：
1. `Amlogic_USB_Burning_Tool_Driver.zip`（晶晨芯片通用 USB 驱动与 update 工具）；
2. `lx06-1.94.13-patched-firmware.zip`（已注入 SSH 与启动钩子的 LX06 补丁镜像包，解压后含 `root-patched.squashfs` 和 `boot.img`）；
3. `xiaoai-agent-lx06-runtime.tar.gz`（ARMv7 静态 musl 架构编译的端侧 Agent 运行时）。

---

## 3. 第一步：拆机与插线

LX06 的 Micro-USB 刷机接口隐藏在外壳内，需要拆开音箱底部内胆：

1. **撕下底座脚垫**：音箱倒置，底部有一圈环形防滑硅胶垫。用手指甲或小刀轻轻掀起揭下，露出固定螺丝。
2. **拧下底部螺丝**：拧下四周的 4 颗十字螺丝。
3. **抽出音箱内胆**：握住音箱顶部外壳，轻轻向外拉出内胆底盖。
4. **找到 Micro-USB 接口**：在主板侧面可以看到一个标准的 Micro-USB 接口。
5. **连接电脑**：
   * 将 Micro-USB 数据线一头插在音箱主板上，另一头直接插到 Windows 电脑的 USB 接口（**建议直接插主板背板，不要接拓展坞 Hub**）；
   * **此时先不要插音箱的电源线！**

---

## 4. 第二步：安装驱动与刷机

### 4.1 安装晶晨底层刷机驱动
1. 解压 `Amlogic_USB_Burning_Tool_Driver.zip`；
2. 运行里面的驱动安装程序 `InstallDriver.exe`，按提示点击“安装”直到完成。

### 4.2 一键刷入固件
1. 将下载的补丁固件包 `lx06-1.94.13-patched-firmware.zip` 解压到 `update.exe` 所在的目录下；
2. 打开 Windows 命令行（CMD 或 PowerShell），进入该目录；
3. 执行一键刷写脚本（或运行目录下的 `flash-lx06.bat`）：
   ```bat
   flash-lx06.bat
   ```
4. **关键步骤（触发引导模式）**：
   * 终端会提示：`Waiting for device... 请在 90 秒内插入音箱电源！`
   * **此时将音箱的 12V 原装电源插头插入音箱**；
   * 终端会自动捕获设备并显示 `firmware version: ...`，开始刷写；
   * 过程会自动写入 `boot0` 与 `system0` 分区，并读回进行 MD5 逐字节校验；
   * 当看到 `✅ 镜像内容读回核对一致！启动分区已成功切换为 boot0` 时，即代表刷机大功告成！
5. **拔线重启**：
   * 拔掉 Micro-USB 数据线；
   * 拔掉音箱电源，等待 5 秒后重新插上电源开机。

---

## 5. 第三步：开机、配网与 SSH 连通确认

1. **配网**：
   * 音箱上电开机后，指示灯亮橙色，提示“欢迎使用小爱音箱...”；
   * 打开手机上的【米家 App】，会自动发现音箱，按提示给音箱连接 Wi-Fi；
2. **查看音箱 IP**：
   * 配网成功后，在路由器后台或米家 App 设备设置 -> 更多设置 -> 网络信息 中查看音箱的 IP（例如 `192.168.1.100`）；
3. **SSH 登录确认**：
   * 在电脑打开终端，通过 SSH 登录音箱：
     ```bash
     ssh root@<音箱IP>
     ```
   * **默认密码**：`open-xiaoai`
   * 登录成功后，你会看到极具辨识度的字符终端欢迎画面，说明补丁系统已经完美运行！

---

## 6. 第四步：部署 XiaoAI-Agent

### 6.1 准备 API Key
本项目直接在端侧跑大模型，推荐使用成本极低且速度飞快的国内接口：
* **ASR 语音转文字**：[SiliconFlow (硅基流动)](https://cloud.siliconflow.cn/)，注册即送免费额度，使用 `FunAudioLLM/SenseVoiceSmall`；
* **LLM 大模型**：[DeepSeek](https://platform.deepseek.com/) 或 SiliconFlow 托管的 `deepseek-chat`。

### 6.2 配置文件
解压 `xiaoai-agent-lx06-runtime.tar.gz`，复制并重命名 `agent.yaml.example` 为 `agent.yaml`，填入你的配置：
```yaml
asr:
  provider: "open_ai"
  open_ai:
    base_url: "https://api.siliconflow.cn/v1"
    api_key: "sk-你的硅基流动APIKEY"
    model: "FunAudioLLM/SenseVoiceSmall"

llm:
  base_url: "https://api.deepseek.com/v1"
  api_key: "sk-你的DeepSeekAPIKEY"
  model: "deepseek-chat"
  max_tokens: 1024

agent:
  system_prompt: "你是小爱音箱端侧运行的AI助手。你的回答必须口语化、亲切自然，适合语音朗读。控制回答在1~3句话以内，严禁输出任何Markdown标记。"

device:
  tts_command: "sh /usr/sbin/tts_play.sh '{text}'; true"
```

### 6.3 上传并运行
在电脑终端执行以下命令将运行时包与配置传到音箱：
```bash
# 1. 在音箱上创建目标目录
ssh root@<音箱IP> "mkdir -p /data/open-xiaoai"

# 2. 上传二进制文件、启动脚本与配置文件
scp xiaoai-agent start-agent.sh agent.yaml root@<音箱IP>:/data/open-xiaoai/

# 3. 登录音箱启动 Agent
ssh root@<音箱IP>
cd /data/open-xiaoai
chmod +x xiaoai-agent start-agent.sh
./start-agent.sh start
```

看到终端输出 `xiaoai-agent ready`，对着音箱喊一声 **“小爱同学”**，问它一个大模型问题（例如：“请问为什么海水是咸的？”），你的音箱就已经成功进化为全功能 AI Agent！

---

## 7. 后悔药：如何一秒切回原厂官方系统？

如果因为任何原因你想回到小米官方原厂系统：

### 方法 A：在 SSH 里一条命令切回
```bash
fw_env -s boot_part boot1 && reboot
```
音箱重启后，便会从从未动过的官方 Slot 1 槽位启动，与刚出厂时一模一样。

### 方法 B：电脑上通过 update 工具切回
如果连不上 SSH，也可以通过 Micro-USB 连上电脑，执行：
```bat
update.exe bulkcmd "setenv boot_part boot1"
update.exe bulkcmd "saveenv"
```
拔电重启即可彻底恢复。
