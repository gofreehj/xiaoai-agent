#!/usr/bin/env bash
# P0 前置验证：判断 LX06 能否进入受控运行时接管探针。
#
# 用法：
#   bash scripts/p0-check.sh <音箱IP>
#   认证由 SSH 交互输入或 SSH key 处理；不要把密码放进命令行、日志或脚本参数。
#
# 前置：设备已刷补丁固件、SSH 可用、与执行机同网。
# 特性：只读，不打开 ALSA 设备，不改变设备进程状态。
# 回滚：无（不修改设备状态）。
#
# 输出：四条 gate 的 PASS/FAIL + 是否可进入 T2A，并写入 docs/log/p0.md。

set -euo pipefail

IP="${1:-}"
[ -n "$IP" ] || { echo "用法: bash scripts/p0-check.sh <音箱IP>"; exit 2; }

SSH_BASE=(
  ssh
  -o HostKeyAlgorithms=+ssh-rsa
  -o PubkeyAcceptedAlgorithms=+ssh-rsa
  -o ConnectTimeout=10
  -o StrictHostKeyChecking=accept-new
  "root@${IP}"
)

run() {
  "${SSH_BASE[@]}" 'sh -s'
}

echo "== P0 验证：$IP"
echo "   只读检查，不会修改设备"
echo

SSH_OK=1
REPORT="$(run <<'REMOTE'
say() { printf '%s\n' "$*"; }
processes() { ps w 2>/dev/null || ps; }

say "### G0 基本信息"
say "model=$(micocfg_model 2>/dev/null || echo unknown)"
say "version=$(grep -m1 ROM /usr/share/mico/version 2>/dev/null | tr -d '\r' || echo unknown)"
say "kernel=$(uname -a 2>/dev/null)"
say "boot_part=$(fw_env -g boot_part 2>/dev/null || echo unknown)"
say "storage=$(df -h /data 2>/dev/null | tail -1)"
say "mem=$(free -m 2>/dev/null | head -2 | tail -1)"
say ""

say "### G1 原生语音前端"
if processes | grep -i mipns | grep -v grep >/dev/null 2>&1; then
  say "G1=PASS"
  processes | grep -i mipns | grep -v grep | head -5
else
  say "G1=FAIL"
  processes | grep -i -E 'mico|aivs|pns' | grep -v grep | head -10
fi
say ""

say "### G2 标准 usock 服务路径"
SPEECH=/tmp/mico_aivs_lab/usock/speech.usock
COMMON=/tmp/mico_aivs_lab/usock/common.usock
if [ -S "$SPEECH" ] && [ -S "$COMMON" ]; then
  say "G2=PASS"
  ls -li "$SPEECH" "$COMMON"
else
  say "G2=FAIL"
  ls -l /tmp/mico_aivs_lab/usock/ 2>/dev/null
fi
say ""

say "### G3 系统 TTS 与播放控制"
TTS=no; MP=no
# LX06 的 tts_play.sh 没有 shebang，使用 sh 解释执行。
[ -f /usr/sbin/tts_play.sh ] && TTS=yes
command -v mphelper >/dev/null 2>&1 && MP=yes
say "tts_play_file=$TTS mphelper=$MP"
if [ "$TTS" = yes ] && [ "$MP" = yes ]; then
  say "G3=PASS"
else
  say "G3=FAIL"
fi
say ""

say "### G4 PNS 启动参数（只读）"
PIDS=$(processes | grep -i mipns | grep -v grep | awk '{print $1}')
if [ -n "$PIDS" ]; then
  say "G4=PASS"
  for PID in $PIDS; do
    if [ -r "/proc/$PID/cmdline" ]; then
      printf 'pid=%s cmdline=' "$PID"
      tr '\000' ' ' < "/proc/$PID/cmdline"
      printf '\n'
    fi
  done
else
  say "G4=FAIL"
fi
say ""
say "### 分区表（备份必须用它，不要用 OH2P 的值）"
cat /proc/mtd 2>/dev/null || echo "no /proc/mtd"
REMOTE
)" || SSH_OK=0

printf '%s\n' "$REPORT"

# ---- 判定 ----
get() { printf '%s\n' "$REPORT" | grep -m1 "^$1=" | cut -d= -f2 || true; }
G1=$(get G1); G2=$(get G2); G3=$(get G3); G4=$(get G4)

echo
echo "== 判定"
echo "   G1 原生语音前端 : ${G1:-?}"
echo "   G2 标准 usock 路径: ${G2:-?}"
echo "   G3 TTS/播放控制 : ${G3:-?}"
echo "   G4 PNS 启动参数 : ${G4:-?}"
echo

if [ "$SSH_OK" = 1 ] && [ "$G1" = PASS ] && [ "$G2" = PASS ] && [ "$G3" = PASS ] && [ "$G4" = PASS ]; then
  BRANCH="T2A"
  echo "   → 可进入 T2A：执行受控 speech.usock 接管探针；P0 不证明协议已兼容"
else
  BRANCH="STOP"
  [ "$SSH_OK" = 1 ] || echo "   SSH 检查未完整执行，不能通过 P0。"
  echo "   → 停止端侧实施：记录证据后转 bridge"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOG_DIR="$SCRIPT_DIR/../docs/log"
mkdir -p "$LOG_DIR"
{
  echo "# P0 验证记录"
  echo
  echo "- 时间：$(date -Iseconds)"
  echo "- 设备：$IP"
  echo "- 分支：**$BRANCH**"
  echo
  echo '```'
  printf '%s\n' "$REPORT"
  echo '```'
} > "$LOG_DIR/p0.md"

echo
echo "   记录已写入 $LOG_DIR/p0.md"
echo "   下一步：继续按 deploy/lx06/docs/01-verify.md 执行 T2A 验收。"
