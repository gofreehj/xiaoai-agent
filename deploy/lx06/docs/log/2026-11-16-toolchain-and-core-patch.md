# 2026-11-16 · 交叉编译工具链 + `patches/core` 镜像验证

> 设备**未连接、未刷机**。本文证明两件事：ARMv7 交叉编译环境可用；`patches/core` 的
> 4 个补丁在 LX06 1.94.13 上用 `--fuzz=0` 能干净打上并通过镜像自检。
> 设备侧分区尺寸、备份、刷写、音频通路都不在本文结论内。

## 环境

- Windows 11 + Git Bash（只做入口，实际命令都在 WSL 里跑）
- WSL2 Ubuntu 26.04 LTS，root 用户
- 工具链全部装在 WSL 内，不污染 Windows

## 1. 安装

```bash
apt-get update && apt-get install -y build-essential cmake clang lld pkg-config unzip xz-utils
```

Rust 走 rsproxy 镜像（直连 `static.rust-lang.org` 约 20 KB/s，装不完）：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
  --default-toolchain 1.96.0 --profile default
rustup target add armv7-unknown-linux-gnueabihf --toolchain 1.96.0
```

crates 索引也走镜像（直连 crates.io 报 `HTTP/2 framing layer` 断流），写入
`/root/.cargo/config.toml`：

```toml
[source.crates-io]
replace-with = "rsproxy"

[source.rsproxy]
registry = "sparse+https://rsproxy.cn/index/"

[http]
multiplexing = false
timeout = 60

[net]
retry = 5
```

其余两个组件：

```bash
# zig 0.17.1，官方 tar.xz 解到 /opt
curl -LO https://ziglang.org/download/0.17.1/zig-x86_64-linux-0.17.1.tar.xz
tar -xf zig-x86_64-linux-0.17.1.tar.xz -C /opt
ln -sfn /opt/zig-x86_64-linux-0.17.1/zig /usr/local/bin/zig

# cargo-zigbuild 0.24.1，用 PyPI wheel（清华镜像），不自己编译
python3 -m pip install --break-system-packages -i \
  https://pypi.tuna.tsinghua.edu.cn/simple cargo-zigbuild
```

版本：

```text
rustc 1.96.0 (ac68faa20 2026-05-25)
zig 0.17.1
cargo-zigbuild 0.24.1
```

## 2. 交叉编译

```bash
cd xiaoai-agent
CARGO_TARGET_DIR=/tmp/xiaoai-target \
  cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

```text
Finished `release` profile [optimized] target(s) in 3m 01s
```

产物已复制到仓库内（`target/` 被 gitignore）：

```text
xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent
7145596 字节
sha256 50252edaf2b073f1eea052103b50ad3e769619893ec6f52fed36c50893abfdb9
```

```text
ELF 32-bit LSB pie executable, ARM, EABI5 version 1 (SYSV), dynamically linked,
interpreter /lib/ld-linux-armhf.so.3, for GNU/Linux 2.0.0, stripped
NEEDED: libm.so.6, libc.so.6, libpthread.so.0, libdl.so.2
GLIBC 版本需求：2.4 2.7 2.9 2.10 2.12 2.15 2.16 2.17 2.18 2.25
最高 GLIBC_2.25，没有 2.26+
```

与 AGENTS.md 里"已知可用产物"一致，T0 验收通过。

## 3. `patches/core` 镜像验证

用 `scripts/build-image.sh` 跑 4 个补丁。为了让验证不产出可刷机镜像，root 密码用
`openssl rand` 现场生成、不保存，镜像核对完就删掉（只留 `check-build.txt`）。

```text
条目数 2137 -> 2137
内容改动 8 个（预期 8 个）：
  bin/flash.sh 4959 -> 4992
  bin/ota 10712 -> 10825
  etc/init.d/dropbear 5218 -> 4638
  etc/init.d/wireless 42563 -> 42601
  etc/inittab 163 -> 169
  etc/pam.d/common-auth 985 -> 987
  etc/rc.local 132 -> 194
  etc/shadow 153 -> 153
核对通过：改动集合、权限、属主、条目数都与预期一致。
补丁镜像大小：38453248 字节（旧参数的历史构建；当前大小以重新构建为准）
```

证据保留在 `deploy/lx06/work/verify-core/`（`check-build.txt`、`check-files.txt`）。
`03-ota.patch` 在 `etc/init.d/wireless` 上有 5 行偏移，上下文完全匹配，属正常。

## 4. 修掉的问题：`02-login.patch` 打不上

第一次跑时 `etc/pam.d/common-auth` 的 hunk 失败：

```text
Hunk #1 FAILED at 8.
1 out of 1 hunk FAILED
❌ 补丁打不上
```

原因：该 hunk 的头部行数与正文不一致（写的是 `@@ -8,9 +8,9 @@`），导致 `patch`
需要 **fuzz 1** 才肯应用；而项目规则禁止 `--fuzz`。文件内容本身与 1.94.13 完全一致
（逐字节 `cmp` 无差异）。

处理：不手改 hunk，直接用官方 1.94.13 的 `etc/shadow`、`etc/inittab`、
`etc/pam.d/common-auth` 和期望结果重新 `diff -u`，保持语义不变（注释掉两条
`libmico-pam.so`、放开 `pam_unix.so`、`ttyS0` 改 `ash --login`、替换 root 密码串）。
新补丁在干净副本上 `patch -p1 --fuzz=0` 通过，原文件备份在 `/tmp/02-login.orig.patch`。

## 5. 未验证 / 阻塞项

- [ ] 音箱当前版本是否就是 1.94.13（米家 App → 设备 → 检查更新）
- [ ] 真实分区尺寸与 `system` 容量（`scripts/discover-parts.sh`）
- [ ] 六分区只读备份、刷写读回校验
- [ ] 刷后 SSH、原生小爱状态、P0 四条 gate
- [ ] 编译产物在真机上的启动与音频行为
