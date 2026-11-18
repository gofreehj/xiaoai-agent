#!/usr/bin/env bash
# 用途：把已验证的 ARMv7 xiaoai-agent 二进制封装成可人工上传的 LX06 运行时包。
# 此脚本只写本机输出目录；不会连接、修改或启动音箱。
#
# 用法：
#   bash scripts/package-runtime.sh <ARMv7 xiaoai-agent> [输出目录]
#
# 先用 scripts/build-agent.sh 生成 ARMv7 二进制，再调用本脚本封装。
#
# 产物内容：
#   xiaoai-agent / agent.yaml.example / start-agent.sh / init.sh.example / SHA256SUMS
#
# 安装时先人工上传并执行 start-agent.sh start 完成 T2A。只有 T2A 验收通过后，
# 才可将 init.sh.example 改名为 /data/init.sh 以启用自启。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
AGENT_REPO="$REPO/xiaoai-agent"
BIN="${1:-}"
OUT="${2:-$HERE/../work/runtime}"

die() { echo "❌ $*" >&2; exit 1; }

[ -n "$BIN" ] || { sed -n '2,14p' "$0"; exit 2; }
[ -f "$BIN" ] || die "找不到二进制：$BIN"
[ -f "$HERE/../templates/agent.yaml.lx06.example" ] || die "找不到 LX06 agent.yaml 模板"
[ -f "$AGENT_REPO/start-agent.sh" ] || die "找不到 start-agent.sh"
command -v file >/dev/null || die "缺少 file"
command -v sha256sum >/dev/null || die "缺少 sha256sum"
command -v grep >/dev/null || die "缺少 grep"

INFO="$(file -b "$BIN")"
case "$INFO" in
  *ARM*EABI5*'statically linked'*) ;;
  *) die "二进制必须是静态链接的 ARM EABI5：$INFO" ;;
esac

if grep -aEq 'GLIBC_[0-9.]+' "$BIN"; then
  die "二进制包含 GLIBC 符号，不能部署到 LX06：$(grep -aEo 'GLIBC_[0-9.]+' "$BIN" | sort -Vu | tail -1)"
fi

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
STAGE="$OUT/xiaoai-agent-lx06-runtime"
[ ! -e "$STAGE" ] || die "输出目录已存在：$STAGE（请换输出目录或手动移走旧产物）"
mkdir -p "$STAGE"

cp "$BIN" "$STAGE/xiaoai-agent"
cp "$HERE/../templates/agent.yaml.lx06.example" "$STAGE/agent.yaml.example"
tr -d '\r' < "$AGENT_REPO/start-agent.sh" > "$STAGE/start-agent.sh"
chmod 0755 "$STAGE/xiaoai-agent" "$STAGE/start-agent.sh"

cat > "$STAGE/init.sh.example" <<'EOF'
#!/bin/sh
# 仅在 T2A 的手动 start / stop 验收完成后才把本文件安装为 /data/init.sh。
AGENT_HOME=/data/open-xiaoai
[ -f "$AGENT_HOME/disabled" ] && exit 0
export XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log
exec "$AGENT_HOME/start-agent.sh" start
EOF
chmod 0755 "$STAGE/init.sh.example"

cat > "$STAGE/INSTALL.txt" <<'EOF'
T2A 手动探针（不要先安装 /data/init.sh）：
  1. 创建 /data/open-xiaoai，上传 xiaoai-agent、start-agent.sh、agent.yaml。
  2. chmod 700 xiaoai-agent start-agent.sh; chmod 600 agent.yaml。
  3. XIAOAI_AGENT_LOG=/tmp/xiaoai-agent.log ./start-agent.sh start；完成唤醒、PCM、stop 恢复原生小爱的验收。
  4. 验收通过后，才将 init.sh.example 安装为 /data/init.sh 开启自启。

不要把 API 密钥提交到版本库或复制到日志。agent.yaml 从 agent.yaml.example 建立并在设备上设为 600。
EOF

for text in "$STAGE/start-agent.sh" "$STAGE/init.sh.example" "$STAGE/agent.yaml.example" "$STAGE/INSTALL.txt"; do
  if LC_ALL=C grep -q "$(printf '\r')" "$text"; then
    die "文本包含 CR 字符：$text；请修正源文件后使用新输出目录重新打包"
  fi
done

(cd "$STAGE" && sha256sum xiaoai-agent agent.yaml.example start-agent.sh init.sh.example INSTALL.txt > SHA256SUMS)
echo "✅ 已生成本地运行时包：$STAGE"
echo "   下一步：仅在 P0 全通过后按 INSTALL.txt 执行 T2A 手动探针。"
