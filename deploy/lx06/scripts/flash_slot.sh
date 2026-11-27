#!/usr/bin/env bash
# 用途：把补丁系统写进 LX06 的一边（A/B 中的 SLOT），另一边不动，留作退路。
#       1. 官方内核（boot.img）写进 boot<SLOT>，读回逐字节核对
#       2. 补丁系统（root-patched.squashfs）写进 system<SLOT>，读回逐字节核对
#       3. 两步都通过，才改启动参数 boot_part=boot<SLOT> 并 saveenv
#       镜像读回失败立即停止、不改启动参数；启动参数操作失败时槽位状态须另行确认。
#
# 用法：SLOT=0 UPDATE=/path/to/update.exe bash scripts/flash_slot.sh <build目录>
#       加 SKIP_BOOT=1 可只重写 system（boot 已在本轮写入并核对过）。
#       加 BOOTDELAY=15 会同时设置 u-boot 等待时间。
#
# 前置：
#   - 已跑 scripts/build-image.sh，build 目录里有 root-patched.squashfs 和 boot.img（或 boot.img.encrypt）。
#   - 已跑 scripts/discover-parts.sh，parts.env 里有 boot<SLOT> / system<SLOT> 的真实尺寸。
#   - 已用只读备份确认 boot<另一边> / system<另一边> 是当前运行（或官方）系统。
#   - 音箱已拆机接好 Micro-USB；流程同样是拔电 → 运行 → 提示后插电。
#
# 回滚：刷完用 SSH 执行 `fw_env -s boot_part boot<另一边> && reboot`；
#       SSH 不通时进刷机模式 `update.exe bulkcmd "setenv boot_part boot<另一边>"` + saveenv。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PARTS_FILE="${PARTS_FILE:-$HERE/../work/parts.env}"
UPDATE="${UPDATE:-update.exe}"
WAIT="${WAIT:-90}"
SLOT="${SLOT:-}"
ASSUME_OK="${ASSUME_OK:-0}"

die() { echo "❌ $*" >&2; exit 1; }

[ "$SLOT" = 0 ] || [ "$SLOT" = 1 ] || die "必须指定 SLOT=0 或 SLOT=1"
[ $# -ge 1 ] || die "用法：SLOT=0 bash scripts/flash_slot.sh <build目录>"
BUILD="$1"
ROOTFS="$BUILD/root-patched.squashfs"
if [ -f "$BUILD/boot.img" ]; then
  KERNEL="$BUILD/boot.img"
elif [ -f "$BUILD/boot.img.encrypt" ]; then
  KERNEL="$BUILD/boot.img.encrypt"
else
  die "找不到 $BUILD/boot.img 或 boot.img.encrypt"
fi
[ -f "$ROOTFS" ] || die "找不到 $ROOTFS"
[ -f "$PARTS_FILE" ] || die "找不到 $PARTS_FILE，先运行 scripts/discover-parts.sh"
command -v "$UPDATE" >/dev/null 2>&1 || [ -x "$UPDATE" ] || die "找不到刷机工具：$UPDATE"

part_size() { awk -v p="$1" '$1==p {print $2}' "$PARTS_FILE"; }
BOOT_SIZE="$(part_size "boot$SLOT")"
SYSTEM_SIZE="$(part_size "system$SLOT")"
[[ "$BOOT_SIZE" =~ ^[0-9]+$ ]] && [ "$BOOT_SIZE" -gt 0 ] || die "parts.env 里 boot$SLOT 尺寸无效或重复"
[[ "$SYSTEM_SIZE" =~ ^[0-9]+$ ]] && [ "$SYSTEM_SIZE" -gt 0 ] || die "parts.env 里 system$SLOT 尺寸无效或重复"

KERNEL_SIZE="$(wc -c < "$KERNEL" | tr -d ' ')"
ROOTFS_SIZE="$(wc -c < "$ROOTFS" | tr -d ' ')"
[ "$KERNEL_SIZE" -le "$BOOT_SIZE" ] || die "内核 $KERNEL_SIZE 字节，超过 boot$SLOT（$BOOT_SIZE）"
[ "$ROOTFS_SIZE" -le "$SYSTEM_SIZE" ] || die "系统镜像 $ROOTFS_SIZE 字节，超过 system$SLOT（$SYSTEM_SIZE）"

OUT="$BUILD/flash-$(date +%Y%m%d-%H%M%S)"
mkdir "$OUT" || die "无法创建独立刷写记录目录：$OUT"
LOG="$OUT/flash.log"
say() { echo "$*" | tee -a "$LOG"; }

OTHER=$((1 - SLOT))
say "将写入 boot$SLOT / system$SLOT"
say "boot$SLOT   : $KERNEL_SIZE / $BOOT_SIZE 字节"
say "system$SLOT : $ROOTFS_SIZE / $SYSTEM_SIZE 字节"
say "boot$OTHER / system$OTHER 不会被改动。"
if [ "$ASSUME_OK" != 1 ]; then
  echo
  echo "确认 boot$OTHER / system$OTHER 是当前正在运行（或官方）系统后，输入 YES 继续："
  read -r ans
  [ "$ans" = YES ] || die "已取消，没有写任何东西。"
fi

wait_device() {
  local end=$((SECONDS + WAIT))
  while [ "$SECONDS" -lt "$end" ]; do
    if "$UPDATE" identify 2>&1 | grep -qi "firmware version"; then
      return 0
    fi
    sleep 0.2
  done
  return 1
}

write_and_verify() {
  local part="$1" file="$2" size="$3" hex got
  hex="$(printf '0x%x' "$size")"
  say "== 写 $(basename "$file") -> $part（分区长度 $hex 字节）"
  [ ! -e "$OUT/$part.readback" ] || { say "   读回文件已存在，停止"; return 1; }
  "$UPDATE" partition "$part" "$file" >>"$LOG" 2>&1 || return 1
  "$UPDATE" mread store "$part" normal "$hex" "$OUT/$part.readback" >>"$LOG" 2>&1 || return 1
  [ -f "$OUT/$part.readback" ] || return 1
  got="$(wc -c < "$OUT/$part.readback" | tr -d ' ')"
  [ "$got" = "$size" ] || { say "   读回长度不符：$got / $size"; return 1; }
  head -c "$(wc -c < "$file" | tr -d ' ')" "$OUT/$part.readback" | cmp -s - "$file" || return 1
  say "   镜像内容读回核对一致"
}

say "现在拔掉音箱电源，再插上电源（${WAIT} 秒内）"
wait_device || die "没有连上，什么都没有写。"
say "identify 成功"
"$UPDATE" identify 2>&1 | tee -a "$LOG" >/dev/null

if [ "${SKIP_BOOT:-0}" = 1 ]; then
  say "== 跳过 boot$SLOT（已在本轮写入并核对过）"
else
  write_and_verify "boot$SLOT" "$KERNEL" "$BOOT_SIZE" \
    || die "boot$SLOT 写入/核对失败，停下。启动参数没改。"
fi
write_and_verify "system$SLOT" "$ROOTFS" "$SYSTEM_SIZE" \
  || die "system$SLOT 写入/核对失败，停下。启动参数没改。"

say "== 改启动参数"
run_bulkcmd() {
  local status=0
  BULK_OUTPUT="$("$UPDATE" bulkcmd "$1" 2>&1)" || status=$?
  printf '%s\n' "$BULK_OUTPUT" | tee -a "$LOG"
  [ "$status" -eq 0 ] || return "$status"
  ! printf '%s\n' "$BULK_OUTPUT" | grep -qiE 'error|failed|not found'
}
if [ -n "${BOOTDELAY:-}" ]; then
  run_bulkcmd "setenv bootdelay $BOOTDELAY" || die "设置 bootdelay 失败；尚未切槽，检查 $LOG"
fi
run_bulkcmd "setenv boot_part boot$SLOT" || die "设置 boot_part 失败；启动参数状态未知，检查日志"
run_bulkcmd "saveenv" || die "saveenv 失败；启动参数持久化状态未知，检查日志"
run_bulkcmd "printenv boot_part" || die "无法读回 boot_part；切槽状态未知，停止并检查"
printf '%s\n' "$BULK_OUTPUT" | tr -d '\r' | grep -Eq "(^|[[:space:]])boot_part=boot$SLOT([[:space:]]|$)" \
  || die "boot_part 读回不匹配；切槽状态未知，停止并检查"

say "完成。拔掉电源再插上，等开机后用 SSH 检查："
say "  fw_env -g boot_part              # 应为 boot$SLOT"
say "  grep ROM /usr/share/mico/version # 应为官方包版本"
say "  grep -c 'no upgrade' /bin/ota    # 1 表示自动升级已关"
