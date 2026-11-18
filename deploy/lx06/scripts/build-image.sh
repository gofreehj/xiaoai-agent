#!/usr/bin/env bash
# 用途：用小米官方 LX06 固件包做补丁系统镜像。只在电脑上做，不碰音箱。
#
# 用法：bash scripts/build-image.sh <官方 mico_all_*.bin> [输出目录]
#       SPEAKER_ROOT_PASSWORD=xxx bash scripts/build-image.sh ...
#
# 前置：
#   - 在 Linux 或 WSL 里运行（Windows Git Bash 没有 unsquashfs / mksquashfs）。
#   - 需要 python3、patch、openssl、unsquashfs、mksquashfs、cmp。
#   - 工作目录必须在 Linux 原生文件系统（脚本用 mktemp 建在 $TMPDIR，默认 /tmp）。
#     在 /mnt/d 这类 DrvFs 上解包/打包会把所有权限变成 0777，绝不能用。
#   - 补丁目录固定为本项目的 patches/core（通用 SSH / 登录 / OTA / 自启补丁）。
#     当前 LX06 发布链不接受外部补丁目录，避免误构建未经批准的镜像。
#
# 输出（默认 deploy/lx06/work/build）：
#   root-patched.squashfs  补丁系统，写进非活动槽的 system<SLOT>
#   boot.img               官方内核原样，写进同一槽的 boot<SLOT>
#   check-*.txt            核对记录
#   sha256sums.txt
#
# 回滚：无。本脚本只生成文件，不写设备、不改仓库。
#
# 任何一项核对失败立即退出，不要拿这个镜像去刷机。
set -euo pipefail
umask 0

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB="$HERE/lib"
PATCHES="$HERE/../patches/core"
EXPECTED="${EXPECTED_CHANGES:-$HERE/expected-changes-lx06.txt}"
OUT="${2:-$HERE/../work/build}"

die() { echo "❌ $*" >&2; exit 1; }

[ $# -ge 1 ] || { sed -n '2,9p' "$0"; exit 2; }
BIN="$1"
[ -f "$BIN" ] || die "找不到固件包：$BIN"

for cmd in python3 patch openssl unsquashfs mksquashfs; do
  command -v "$cmd" >/dev/null || die "缺少 $cmd（见脚本头部前置条件）"
done
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' || die "python3 版本太低"
[ -d "$PATCHES" ] || die "找不到批准的 core-4 补丁目录：$PATCHES"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/lx06-build.XXXXXX")"
cleanup() { [ "${KEEP_WORK:-0}" = 1 ] || rm -rf "$WORK"; }
trap cleanup EXIT
case "$WORK" in
  /mnt/*) die "工作目录在 DrvFs 上（$WORK），权限会被改坏。请把 TMPDIR 指到 Linux 原生路径。" ;;
esac
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

echo "== 1/6 解开官方包（校验 CRC32 与文件名 MD5）"
python3 -I "$LIB/mico_extract.py" "$BIN" "$WORK/extracted"

echo "== 2/6 解开官方系统"
unsquashfs -no-xattrs -ignore-errors -d "$WORK/rootfs" "$WORK/extracted/root.squashfs" >/dev/null 2>&1 || true
[ -f "$WORK/rootfs/etc/shadow" ] || die "解开 root.squashfs 失败"
cp -a "$WORK/rootfs" "$WORK/rootfs-patched"

if [ -z "${SPEAKER_ROOT_PASSWORD:-}" ]; then
  read -rsp "音箱 root 密码（SSH 登录用）：" SPEAKER_ROOT_PASSWORD; echo
  read -rsp "再输一次：" again; echo
  [ "$SPEAKER_ROOT_PASSWORD" = "$again" ] || die "两次输入的密码不一样"
fi
[ -n "$SPEAKER_ROOT_PASSWORD" ] || die "密码不能为空"
SALT="$(openssl rand -base64 12 | tr -dc 'A-Za-z0-9' | cut -c1-8)"
HASH="$(printf '%s' "$SPEAKER_ROOT_PASSWORD" | openssl passwd -1 -salt "$SALT" -stdin)"

echo "== 3/6 打补丁（LX06 自有通用补丁集，fuzz=0）"
apply_one() {
  local file="$1"
  case "$file" in
    *.patch)
      echo "   $(basename "$(dirname "$file")")/$(basename "$file")"
      sed "s|{SSH_PASSWORD}|$HASH|g" "$file" \
        | (cd "$WORK/rootfs-patched" && patch -p1 --fuzz=0 --no-backup-if-mismatch) \
        || die "补丁打不上：$file（不要 --fuzz）"
      ;;
    *.sh)
      echo "   $(basename "$(dirname "$file")")/$(basename "$file")（脚本）"
      (cd "$WORK/rootfs-patched" && bash "$file") || die "脚本补丁失败：$file"
      ;;
  esac
}
for f in "$PATCHES"/*.patch "$PATCHES"/*.sh; do
  [ -e "$f" ] && apply_one "$f"
done

echo "== 4/6 重新打包（xz / 128K，参数同官方镜像）"
mksquashfs "$WORK/rootfs-patched" "$WORK/root-patched.squashfs" \
  -comp xz -b 131072 -noappend -all-root -no-xattrs \
  -p "dev/console c 600 0 0 5 1" -quiet -no-progress

echo "== 5/6 核对"
if ! python3 -I "$LIB/verify_build.py" "$WORK/rootfs" "$WORK/rootfs-patched" \
  "$WORK/extracted/root.squashfs" "$WORK/root-patched.squashfs" \
  "$EXPECTED" | tee "$OUT/check-build.txt"; then
  die "镜像核对失败，不要刷这个镜像"
fi
python3 -I "$LIB/tree_diff.py" "$WORK/rootfs" "$WORK/rootfs-patched" > "$OUT/check-files.txt"

line="$(head -1 "$WORK/rootfs-patched/etc/shadow" | cut -d: -f2)"
salt="$(printf '%s' "$line" | cut -d'$' -f3)"
[ "$(printf '%s' "$SPEAKER_ROOT_PASSWORD" | openssl passwd -1 -salt "$salt" -stdin)" = "$line" ] \
  || die "shadow 里的密码串验证不过"

SIZE="$(stat -c %s "$WORK/root-patched.squashfs")"
echo "   补丁镜像大小：$SIZE 字节"
if [ -n "${SYSTEM_MAX:-}" ]; then
  MAX=$((SYSTEM_MAX))
  [ "$SIZE" -lt "$MAX" ] || die "镜像 $SIZE 字节，不小于 system 分区 $MAX 字节"
  echo "   分区检查通过（残留 $((MAX - SIZE)) 字节）"
else
  echo "   ⚠️ 未校验 system 分区大小：SYSTEM_MAX 没设置。刷机前必须用设备真实 /proc/mtd 复核。"
fi

echo "== 6/6 复制产物"
cp "$WORK/root-patched.squashfs" "$OUT/root-patched.squashfs"
if [ -f "$WORK/extracted/boot.img" ]; then
  cp "$WORK/extracted/boot.img" "$OUT/boot.img"
  BOOT_NAME=boot.img
elif [ -f "$WORK/extracted/boot.img.encrypt" ]; then
  cp "$WORK/extracted/boot.img.encrypt" "$OUT/boot.img.encrypt"
  BOOT_NAME=boot.img.encrypt
else
  die "官方包里既没有 boot.img 也没有 boot.img.encrypt"
fi
( cd "$OUT" && sha256sum root-patched.squashfs "$BOOT_NAME" > sha256sums.txt )

echo
echo "✅ 完成："
echo "   系统镜像：$OUT/root-patched.squashfs（$SIZE 字节）"
echo "   官方内核：$OUT/$BOOT_NAME"
echo "   核对记录：$OUT/check-build.txt"
echo "   下一步：备份设备分区并确认 SYSTEM_MAX，再用 scripts/flash_slot.sh 写非活动槽。"
