@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion

echo ======================================================
echo    小爱音箱 Pro (LX06) 一键刷机工具 (Windows 专版)
echo ======================================================
echo.

set "UPDATE_EXE="
if exist "update.exe" set "UPDATE_EXE=update.exe"
if not defined UPDATE_EXE (
    if exist "..\update.exe" set "UPDATE_EXE=..\update.exe"
)
if not defined UPDATE_EXE (
    if exist "C:\Program Files (x86)\Amlogic\USB_Burning_Tool\update.exe" (
        set "UPDATE_EXE=C:\Program Files (x86)\Amlogic\USB_Burning_Tool\update.exe"
    )
)

if not defined UPDATE_EXE (
    where update.exe >nul 2>nul
    if %errorlevel% equ 0 set "UPDATE_EXE=update.exe"
)

if not defined UPDATE_EXE (
    echo [错误] 未找到 update.exe 刷机工具！
    echo 请将本脚本与晶晨刷机工具 update.exe 放在同一文件夹下，
    echo 或安装 Amlogic USB Burning Tool。
    echo.
    pause
    exit /b 1
)

set "BOOT_IMG="
if exist "boot.img" set "BOOT_IMG=boot.img"
if not defined BOOT_IMG (
    if exist "boot.img.encrypt" set "BOOT_IMG=boot.img.encrypt"
)

set "ROOTFS_IMG="
if exist "root-patched.squashfs" set "ROOTFS_IMG=root-patched.squashfs"

if not defined BOOT_IMG (
    echo [错误] 未在当前目录找到 boot.img 内核镜像！
    pause
    exit /b 1
)

if not defined ROOTFS_IMG (
    echo [错误] 未在当前目录找到 root-patched.squashfs 系统镜像！
    pause
    exit /b 1
)

echo [1/4] 硬件检测与安全检查：
echo      - 刷机工具 : !UPDATE_EXE!
echo      - 内核镜像 : !BOOT_IMG!
echo      - 系统镜像 : !ROOTFS_IMG!
echo      - 目标分区 : boot0 / system0 (A/B槽位安全写入，保留官方槽位)
echo.
echo ======================================================
echo 请按以下步骤操作：
echo   1. 确保音箱已通过 Micro-USB 数据线连接到电脑；
echo   2. 确保音箱现在【拔掉】了 12V 电源插头；
echo ======================================================
echo.
echo 准备就绪后，请按任意键开始等待设备连接...
pause >nul

echo.
echo [2/4] 正在等待音箱连接...
echo 【请现在插上音箱的 12V 原装电源！】
echo.

:WAIT_LOOP
"!UPDATE_EXE!" identify >nul 2>nul
if %errorlevel% equ 0 goto DEVICE_FOUND
timeout /t 1 /nobreak >nul
goto WAIT_LOOP

:DEVICE_FOUND
echo ✅ 成功捕获到音箱设备！开始刷写...
echo.

echo [3/4] 正在写入固件到 Slot 0 (过程约 30~60 秒，请勿断电或拔线)...
echo  - 正在写入内核到 boot0...
"!UPDATE_EXE!" partition boot0 "!BOOT_IMG!"
if %errorlevel% neq 0 (
    echo [错误] boot0 写入失败！
    pause
    exit /b 1
)

echo  - 正在写入补丁系统到 system0...
"!UPDATE_EXE!" partition system0 "!ROOTFS_IMG!"
if %errorlevel% neq 0 (
    echo [错误] system0 写入失败！
    pause
    exit /b 1
)

echo.
echo [4/4] 切换启动分区与参数配置...
"!UPDATE_EXE!" bulkcmd "setenv boot_part boot0"
"!UPDATE_EXE!" bulkcmd "setenv bootdelay 5"
"!UPDATE_EXE!" bulkcmd "saveenv"

echo.
echo ======================================================
echo 🎉🎉🎉 恭喜！小爱音箱 Pro (LX06) 刷机大功告成！
echo ======================================================
echo.
echo 下一步操作指引：
echo   1. 拔掉 Micro-USB 数据线；
echo   2. 拔掉音箱电源，等待 5 秒重新插电开机；
echo   3. 打开手机米家 App 配网；
echo   4. 通过 SSH 连接音箱: ssh root^@^<音箱IP^> (密码: open-xiaoai)；
echo   5. 按照 FLASH_GUIDE.md 部署 XiaoAI-Agent 即可！
echo.
pause
