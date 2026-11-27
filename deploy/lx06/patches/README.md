# LX06 自有固件补丁

本目录是 `deploy/lx06` 的唯一默认补丁来源。`scripts/build-image.sh` 只应用 `core/` 中的文件，不读取或修改仓库其他位置的补丁目录。

`core/` 包含四个经过最小化处理的通用补丁：启用 SSH、配置 root 登录、禁用 OTA 写入、在启动完成后调用 `/data/init.sh`。它们不会触及 LX06 的 ALSA、`mico_record`、`noop` 或 `libxaudio_engine.so`。

每次修改补丁集合都必须同步更新 `scripts/expected-changes-lx06.txt`，并针对目标官方包运行完整构建验证。任何补丁上下文不匹配都会使构建立刻失败；不得使用 `patch --fuzz`。
