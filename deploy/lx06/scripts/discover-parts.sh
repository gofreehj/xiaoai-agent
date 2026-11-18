#!/usr/bin/env bash
# 用途：只读读取设备的 u-boot 环境变量，拿到真实的 mtd 分区表，生成 parts.env。
#       刷机前必须先做这一步：LX06 的分区尺寸不能沿用 OH2P 的 0x2800000 / 0x600000。
#
# 用法：UPDATE=/path/to/update.exe bash scripts/discover-parts.sh [输出文件]
#       默认输出 deploy/lx06/work/parts.env，原始环境变量存 deploy/lx06/work/uboot-env.txt
#
# 前置：
#   - 音箱已拆机，Micro-USB 数据线直连电脑（不过 Hub），Windows 已装 Amlogic USB 驱动。
#   - UPDATE 指向 Amlogic 刷机工具（Windows 上是 update.exe；macOS 上是 update）。
#   - 音箱电源可拔插。
#
# 流程：拔掉音箱电源 → 运行本脚本 → 看到提示后插上电源。
#
# 回滚：无。本脚本只执行 identify / bulkcmd printenv，不写设备。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HERE/../work/parts.env}"
RAW="$(dirname "$OUT")/uboot-env.txt"
UPDATE="${UPDATE:-update.exe}"
WAIT="${WAIT:-90}"

die() { echo "❌ $*" >&2; exit 1; }

command -v "$UPDATE" >/dev/null 2>&1 || [ -x "$UPDATE" ] \
  || die "找不到刷机工具：$UPDATE（用 UPDATE=... 指定 update.exe 的路径）"
mkdir -p "$(dirname "$OUT")"

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

echo "== 等待刷机设备"
echo "   现在拔掉音箱电源，再插上电源（${WAIT} 秒内）"
if ! wait_device; then
  die "没有连上。拔掉电源，重新运行本脚本，再插电。"
fi
echo "   identify 成功"
"$UPDATE" identify 2>&1 | tee "$RAW" >/dev/null

echo "== 读取 u-boot 环境"
{
  "$UPDATE" bulkcmd "printenv mtdparts" 2>&1 || true
  "$UPDATE" bulkcmd "printenv bootargs" 2>&1 || true
  "$UPDATE" bulkcmd "printenv" 2>&1 || true
} | tee -a "$RAW" >/dev/null

MTD="$(tr -d "\r'" < "$RAW" | grep -o 'mtdparts=[^ "]*' | head -1 | cut -d= -f2- || true)"
[ -n "$MTD" ] || {
  echo "❌ 环境变量里没有 mtdparts。原始输出已保存到：$RAW"
  echo "   把该文件内容发出来，或从设备启动后 cat /proc/mtd 提供分区表。"
  exit 1
}

# 只解析第一个控制器，去掉 nand0: 这类前缀。
MTD="${MTD%%;*}"
MTD="${MTD#*:}"

to_bytes() {
  local v="$1" number multiplier=1
  case "$v" in
    *[kK]) multiplier=1024; v="${v%[kK]}" ;;
    *[mM]) multiplier=1048576; v="${v%[mM]}" ;;
    *[gG]) multiplier=1073741824; v="${v%[gG]}" ;;
  esac
  if [[ "$v" =~ ^0[xX][0-9a-fA-F]+$ ]]; then
    number=$((v))
  elif [[ "$v" =~ ^[0-9]+$ ]]; then
    number=$((10#$v))
  else
    return 1
  fi
  [ "$number" -gt 0 ] && [ "$number" -le $((9223372036854775807 / multiplier)) ] || return 1
  echo $((number * multiplier))
}

echo "== 分区表"
echo "   mtdparts=$MTD"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
printf '%s\n' "$MTD" | tr ',' '\n' | while IFS= read -r part; do
  [ -n "$part" ] || continue
  name="$(printf '%s' "$part" | sed -n 's/.*(\([^)]*\)).*/\1/p')"
  spec="$(printf '%s' "$part" | sed 's/(.*//')"
  size="${spec%%@*}"
  [ -n "$name" ] || continue
  # 备份只需要这六个分区；末尾的 -(data) 无固定长度。
  case "$name" in bootloader|tpl|boot0|boot1|system0|system1) ;; *) continue ;; esac
  bytes="$(to_bytes "$size")" || die "分区 $name 的长度无效：$size"
  grep -q "^$name " "$TMP" && die "分区重复：$name"
  printf '%s %s\n' "$name" "$bytes" | tee -a "$TMP"
done

missing=""
for p in bootloader tpl boot0 boot1 system0 system1; do
  grep -q "^$p " "$TMP" || missing="$missing $p"
done
[ -z "$missing" ] || {
  echo "❌ 分区表缺少：$missing"
  echo "   原始输出：$RAW"
  exit 1
}

cp "$TMP" "$OUT"
echo
echo "✅ 已写入：$OUT"
awk '{printf "   %-12s %10d 字节 (%d MB)\n", $1, $2, $2/1024/1024}' "$OUT"
echo "   原始环境变量：$RAW"
echo "   下一步：bash scripts/backup_readonly.sh"
