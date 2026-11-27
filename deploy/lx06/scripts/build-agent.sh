#!/usr/bin/env bash
# 构建并验证 LX06 所需的静态 ARMv7 musl Agent 二进制。
#
# 前置：rustup 已安装 armv7-unknown-linux-musleabihf target，PATH 中有 cargo、rustc、zig。
# 用法：bash scripts/build-agent.sh [输出目录]
#
# 只写本机 target/ 与输出目录；不连接或修改设备。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
AGENT_REPO="$REPO/xiaoai-agent"
OUT="${1:-$HERE/../work/agent-build}"
TARGET=armv7-unknown-linux-musleabihf
# The Windows GNU host toolchain avoids a collision between Rust's MSVC host
# toolchain and Git for Windows' GNU link.exe. Linux/WSL must retain the
# caller's default Rust toolchain unless RUSTUP_TOOLCHAIN is explicitly set.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) HOST_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable-x86_64-pc-windows-gnu}" ;;
  *) HOST_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-}" ;;
esac

die() { echo "❌ $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "缺少命令：$1"; }
need cargo
need zig
need file
need grep
need sort
need readelf

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    # cc-rs launches target compilers through Windows CreateProcess, which
    # cannot execute a POSIX .sh wrapper. Use .cmd wrappers on this host.
    WRAPPER="$OUT/zig-armv7-linker.cmd"
    cat > "$WRAPPER" <<'EOF'
@echo off
setlocal EnableExtensions
set "args="
set "is_compile=0"
:next_arg
if "%~1"=="" goto run
if /I "%~1"=="-c" set "is_compile=1"
if /I "%~1"=="--target=armv7-unknown-linux-musleabihf" goto skip_arg
if /I "%~1"=="--target=armv7-unknown-linux-gnueabihf" goto skip_arg
if /I "%~1"=="-nostartfiles" goto skip_arg
if /I "%~1"=="-nodefaultlibs" goto skip_arg
set "arg=%~1"
if /I "%arg:~0,4%"=="-Wl," set "arg=%arg:~4%"
set args=%args% "%arg%"
:skip_arg
shift
goto next_arg
:run
if "%is_compile%"=="1" zig.exe cc -target arm-linux-musleabihf -fno-sanitize=all %args%
if not "%is_compile%"=="1" zig.exe ld.lld -m armelf_linux_eabi %args%
EOF
    CXX_WRAPPER="$OUT/zig-armv7-cxx.cmd"
    cat > "$CXX_WRAPPER" <<'EOF'
@echo off
setlocal EnableExtensions
set "args="
set "is_compile=0"
:next_arg
if "%~1"=="" goto run
if /I "%~1"=="-c" set "is_compile=1"
if /I "%~1"=="--target=armv7-unknown-linux-musleabihf" goto skip_arg
if /I "%~1"=="--target=armv7-unknown-linux-gnueabihf" goto skip_arg
if /I "%~1"=="-nostartfiles" goto skip_arg
if /I "%~1"=="-nodefaultlibs" goto skip_arg
set "arg=%~1"
if /I "%arg:~0,4%"=="-Wl," set "arg=%arg:~4%"
set args=%args% "%arg%"
:skip_arg
shift
goto next_arg
:run
if "%is_compile%"=="1" zig.exe c++ -target arm-linux-musleabihf -fno-sanitize=all %args%
if not "%is_compile%"=="1" zig.exe ld.lld -m armelf_linux_eabi %args%
EOF
    ;;
  *)
    WRAPPER="$OUT/zig-armv7-linker.sh"
    cat > "$WRAPPER" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
args=()
is_compile=0
for arg in "$@"; do
  [ "$arg" = "-c" ] && is_compile=1
  case "$arg" in
    --target=armv7-unknown-linux-musleabihf|--target=armv7-unknown-linux-gnueabihf|-nostartfiles|-nodefaultlibs) continue ;;
    -Wl,*) args+=("${arg#-Wl,}") ;;
    *) args+=("$arg") ;;
  esac
done
if [ "$is_compile" = 1 ]; then
  exec zig cc -target arm-linux-musleabihf -fno-sanitize=all "${args[@]}"
fi
exec zig ld.lld -m armelf_linux_eabi "${args[@]}"
EOF
    chmod 0755 "$WRAPPER"
    CXX_WRAPPER="$OUT/zig-armv7-cxx.sh"
    cat > "$CXX_WRAPPER" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
args=()
is_compile=0
for arg in "$@"; do
  [ "$arg" = "-c" ] && is_compile=1
  case "$arg" in
    --target=armv7-unknown-linux-musleabihf|--target=armv7-unknown-linux-gnueabihf|-nostartfiles|-nodefaultlibs) continue ;;
    -Wl,*) args+=("${arg#-Wl,}") ;;
    *) args+=("$arg") ;;
  esac
done
if [ "$is_compile" = 1 ]; then
  exec zig c++ -target arm-linux-musleabihf -fno-sanitize=all "${args[@]}"
fi
exec zig ld.lld -m armelf_linux_eabi "${args[@]}"
EOF
    chmod 0755 "$CXX_WRAPPER"
    ;;
esac

export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$WRAPPER"
# aws-lc-sys builds C sources through cc-rs, which does not read Cargo's
# linker setting. Its target-specific variables use underscores here.
export CC_armv7_unknown_linux_musleabihf="$WRAPPER"
export CXX_armv7_unknown_linux_musleabihf="$CXX_WRAPPER"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_RUSTFLAGS="-C link-arg=-Wl,--as-needed"
if [ -n "$HOST_TOOLCHAIN" ]; then
  export RUSTUP_TOOLCHAIN="$HOST_TOOLCHAIN"
fi
export ZIG_GLOBAL_CACHE_DIR="${ZIG_GLOBAL_CACHE_DIR:-$OUT/zig-global-cache}"
mkdir -p "$ZIG_GLOBAL_CACHE_DIR"

echo "== 构建 $TARGET（宿主工具链：${HOST_TOOLCHAIN:-rustup 默认}）"
(cd "$AGENT_REPO" && cargo build --release --target "$TARGET")

BIN="$AGENT_REPO/target/$TARGET/release/xiaoai-agent"
[ -f "$BIN" ] || die "Cargo 未产出 $BIN"
cp "$BIN" "$OUT/xiaoai-agent"

echo "== ELF 验证"
ELF_INFO="$(file -b "$OUT/xiaoai-agent")"
printf '%s\n' "$ELF_INFO"
readelf -h "$OUT/xiaoai-agent" | tee "$OUT/elf-headers.txt"
grep -aEo 'GLIBC_[0-9.]+' "$OUT/xiaoai-agent" | sort -Vu | tail -1 > "$OUT/max-glibc.txt" || true
MAX_GLIBC="$(cat "$OUT/max-glibc.txt")"
echo "最高 GLIBC 符号版本：${MAX_GLIBC:-无（可能静态链接）}"

if [ -n "$MAX_GLIBC" ]; then
  die "静态 musl 产物不应包含 GLIBC 符号：$MAX_GLIBC"
fi

case "$ELF_INFO" in
  *ARM*EABI5*'statically linked'*) ;;
  *) die "产物不是 ARM EABI5" ;;
esac

sha256sum "$OUT/xiaoai-agent" > "$OUT/xiaoai-agent.sha256"
echo "✅ 构建完成：$OUT/xiaoai-agent"
