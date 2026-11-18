#!/usr/bin/env bash
# 用途：刷机前的只读备份——进刷机模式读出 bootloader、tpl、boot0/1、system0/1 六个分区。
#       只执行 identify / mread，不写设备。尺寸全部来自 discover-parts.sh 生成的真实分区表。
#
# 用法：UPDATE=/path/to/update.exe bash scripts/backup_readonly.sh [输出目录]
#       PARTS_FILE=deploy/lx06/work/parts.env（默认）
#       默认输出 deploy/lx06/backup/<时间戳>/
#
# 前置：已跑过 scripts/discover-parts.sh，得到 parts.env；音箱已拆机接好 Micro-USB。
#
# 流程：拔掉音箱电源 → 运行脚本 → 看到提示后插上电源。读完拔电再插，音箱正常开机。
#
# 回滚：无。本脚本只读。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PARTS_FILE="${PARTS_FILE:-$HERE/../work/parts.env}"
OUT="${1:-$HERE/../backup/$(date +%Y%m%d-%H%M%S)}"
UPDATE="${UPDATE:-update.exe}"
WAIT="${WAIT:-90}"

die() { echo "❌ $*" >&2; exit 1; }

[ -f "$PARTS_FILE" ] || die "找不到 $PARTS_FILE，先运行 scripts/discover-parts.sh"
command -v "$UPDATE" >/dev/null 2>&1 || [ -x "$UPDATE" ] || die "找不到刷机工具：$UPDATE"
for part in bootloader tpl boot0 boot1 system0 system1; do
  size="$(awk -v p="$part" '$1==p {print $2}' "$PARTS_FILE")"
  [[ "$size" =~ ^[0-9]+$ ]] && [ "$size" -gt 0 ] || die "parts.env 中 $part 尺寸无效、缺失或重复"
done
awk 'NF && $1 !~ /^#/ && ($1 !~ /^(bootloader|tpl|boot0|boot1|system0|system1)$/ || NF != 2) {exit 1}' "$PARTS_FILE" \
  || die "parts.env 包含不支持的分区或格式"
mkdir -p "$OUT"
[ -z "$(ls -A "$OUT")" ] || die "备份目录非空：$OUT；请指定新的空目录，保留旧备份"
cp "$PARTS_FILE" "$OUT/parts.env"
RAW_ENV="$(dirname "$PARTS_FILE")/uboot-env.txt"
[ ! -f "$RAW_ENV" ] || cp "$RAW_ENV" "$OUT/uboot-env.txt"
LOG="$OUT/backup.log"
say() { echo "$*" | tee -a "$LOG"; }

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

say "备份目录：$OUT"
say "分区表：$PARTS_FILE"
say "现在拔掉音箱电源，再插上电源（${WAIT} 秒内）"
wait_device || die "没有连上。拔掉电源，重新运行脚本，再插电。"
say "identify 成功"
"$UPDATE" identify 2>&1 | tee -a "$LOG" >/dev/null

failed=0
while read -r name size; do
  case "$name" in ''|'#'*) continue ;; esac
  [ -n "${size:-}" ] || continue
  hex="$(printf '0x%x' "$size")"
  say "== 读 $name（$hex 字节）"
  if ! "$UPDATE" mread store "$name" normal "$hex" "$OUT/$name.img" >>"$LOG" 2>&1; then
    say "   读取命令失败（详见 backup.log）"
    failed=1
    continue
  fi
  if [ -f "$OUT/$name.img" ]; then
    got="$(wc -c < "$OUT/$name.img" | tr -d ' ')"
  else
    got=0
  fi
  if [ "$got" = "$size" ]; then
    say "   成功（$got 字节）"
  else
    say "   失败：期望 $size 字节，实际 $got 字节（详见 backup.log）"
    failed=1
  fi
done < "$PARTS_FILE"

if [ "$failed" -ne 0 ]; then
  die "有分区没读成功，备份不完整。不要刷机；修复 USB/驱动后重做。"
fi

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT" && sha256sum ./*.img > sha256sums.txt)
else
  (cd "$OUT" && shasum -a 256 ./*.img > sha256sums.txt)
fi

say "完成。现在拔掉音箱电源再插上，正常开机。"
say "校验和：$OUT/sha256sums.txt"
