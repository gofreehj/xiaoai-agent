#!/usr/bin/env python3
"""
Build, package, and deploy the Sherpa-ONNX 3.3M UDP XPCM KWS server to a Redmi AX5 router.

This router service is one reference implementation of docs/external-kws-protocol.md.
Speaker-side capture & UDP forwarding now live directly inside `xiaoai-agent`.
"""
from __future__ import annotations

import argparse
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
import time


REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
AX5_DIR = os.path.dirname(os.path.abspath(__file__))

REQUIRED_GLIBC_FILES = [
    "ld-linux-armhf.so.3",
    "libc.so.6",
    "libm.so.6",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
]


def make_askpass(password: str, tmpdir: str, name: str) -> str:
    path = os.path.join(tmpdir, f"askpass_{name}.sh")
    with open(path, "w", encoding="utf-8") as f:
        f.write(f"#!/bin/sh\nprintf '%s\\n' {password!r}\n")
    os.chmod(path, stat.S_IRWXU)
    return path


def ssh_env(askpass_path: str | None) -> dict[str, str]:
    env = os.environ.copy()
    if askpass_path:
        env["SSH_ASKPASS"] = askpass_path
        env["SSH_ASKPASS_REQUIRE"] = "force"
        env["DISPLAY"] = "1"
    return env


def run_ssh(host: str, cmd: str, env: dict[str, str], stdin_bytes: bytes | None = None):
    full_cmd = [
        "ssh",
        "-o", "ConnectTimeout=8",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "HostKeyAlgorithms=+ssh-rsa",
        "-o", "PubkeyAcceptedAlgorithms=+ssh-rsa",
        f"root@{host}",
        cmd,
    ]
    return subprocess.run(full_cmd, input=stdin_bytes, capture_output=True, env=env)


def compile_ax5_server(zig_bin: str, sherpa_include: str, out_dir: str) -> str:
    os.makedirs(out_dir, exist_ok=True)
    ax5_bin = os.path.join(out_dir, "ax5-kws-server")

    print("Compiling ax5-kws-server (arm-linux-gnueabihf.2.36)...")
    subprocess.run(
        [
            zig_bin, "cc",
            "-target", "arm-linux-gnueabihf.2.36",
            "-O2", "-s", "-fPIE", "-pie",
            f"-I{sherpa_include}",
            os.path.join(AX5_DIR, "src", "ax5_kws_server.c"),
            "-ldl", "-lm",
            "-o", ax5_bin,
        ],
        check=True,
    )
    return ax5_bin


def build_ax5_bundle(
    ax5_bin: str,
    model_dir: str,
    sherpa_libs_dir: str,
    glibc_dir: str,
    bundle_path: str,
):
    with tempfile.TemporaryDirectory() as stage:
        for sub in ("bin", "lib", "models"):
            os.makedirs(os.path.join(stage, sub), exist_ok=True)

        shutil.copy2(ax5_bin, os.path.join(stage, "bin", "ax5-kws-server"))

        for fname in REQUIRED_GLIBC_FILES:
            src = os.path.join(glibc_dir, fname)
            if not os.path.exists(src):
                raise FileNotFoundError(f"Missing glibc runtime file: {src}")
            shutil.copy2(src, os.path.join(stage, "lib", fname))

        for fname in (
            "libsherpa-onnx-c-api.so",
            "libonnxruntime.so",
            "libgcc_s.so.1",
            "libstdc++.so.6.0.30",
        ):
            src = os.path.join(sherpa_libs_dir, fname)
            if not os.path.exists(src):
                raise FileNotFoundError(f"Missing sherpa/cpp runtime file: {src}")
            shutil.copy2(src, os.path.join(stage, "lib", fname))

        os.symlink(
            "libstdc++.so.6.0.30",
            os.path.join(stage, "lib", "libstdc++.so.6"),
        )

        model_files = (
            "encoder-epoch-13-avg-2-chunk-8-left-64.int8.onnx",
            "decoder-epoch-13-avg-2-chunk-8-left-64.onnx",
            "joiner-epoch-13-avg-2-chunk-8-left-64.int8.onnx",
            "tokens.txt",
        )
        if os.path.isdir(model_dir):
            for fname in model_files:
                src = os.path.join(model_dir, fname)
                if not os.path.exists(src):
                    raise FileNotFoundError(f"Missing model file: {src}")
                shutil.copy2(src, os.path.join(stage, "models", fname))
        else:
            tar_path = model_dir.rstrip("/") + ".tar.bz2"
            if not os.path.isfile(tar_path):
                raise FileNotFoundError(f"Missing model dir {model_dir} and archive {tar_path}")
            print(f"Extracting model files from {tar_path}...")
            with tarfile.open(tar_path, "r:bz2") as tf:
                wanted = {fname: False for fname in model_files}
                for m in tf.getmembers():
                    base = os.path.basename(m.name)
                    if base in wanted and m.isfile():
                        with tf.extractfile(m) as src_f, open(os.path.join(stage, "models", base), "wb") as dst_f:
                            shutil.copyfileobj(src_f, dst_f)
                        wanted[base] = True
                missing = [k for k, v in wanted.items() if not v]
                if missing:
                    raise FileNotFoundError(f"Missing files in {tar_path}: {missing}")

        subprocess.run(
            ["tar", "-czf", bundle_path, "-C", stage, "bin", "lib", "models"],
            check=True,
        )
    size_mb = os.path.getsize(bundle_path) / (1024 * 1024)
    print(f"Built {bundle_path} ({size_mb:.2f} MiB)")


def deploy_ax5(ax5_ip: str, ax5_env: dict[str, str], bundle_path: str):
    print(f"\n=== Deploying KWS server to Redmi AX5 ({ax5_ip}) ===")
    res = run_ssh(ax5_ip, "uname -a; df -m /data /tmp", ax5_env)
    if res.returncode != 0:
        raise RuntimeError(f"SSH to AX5 failed: {res.stderr.decode()}")
    print(res.stdout.decode().strip())

    run_ssh(
        ax5_ip,
        "/data/kws/start-ax5-kws.sh stop 2>/dev/null || true; "
        "for p in /proc/[0-9]*; do "
        "  [ -r \"$p/cmdline\" ] || continue; "
        "  case \"$(tr '\\0' ' ' <\"$p/cmdline\" 2>/dev/null)\" in "
        "    *ax5-kws-server*) kill \"${p#/proc/}\" 2>/dev/null || true ;; "
        "  esac; "
        "done; "
        "killall sherpa-onnx-keyword-spotter 2>/dev/null || true; "
        "rm -rf /tmp/kws_test /tmp/kws /tmp/ax5-kws.log /tmp/ax5_kws_test.log; "
        "mkdir -p /data/kws",
        ax5_env,
    )

    print("Uploading kws_runtime.tar.gz to /data/kws/...")
    with open(bundle_path, "rb") as f:
        res = run_ssh(ax5_ip, "cat > /data/kws/kws_runtime.tar.gz", ax5_env, stdin_bytes=f.read())
        if res.returncode != 0:
            raise RuntimeError(f"Upload bundle failed: {res.stderr.decode()}")

    with open(os.path.join(AX5_DIR, "keywords.txt"), "rb") as f:
        res = run_ssh(ax5_ip, "cat > /data/kws/keywords.txt", ax5_env, stdin_bytes=f.read())
        if res.returncode != 0:
            raise RuntimeError(f"Upload keywords.txt failed: {res.stderr.decode()}")

    with open(os.path.join(AX5_DIR, "start-ax5-kws.sh"), "rb") as f:
        res = run_ssh(
            ax5_ip,
            "cat > /data/kws/start-ax5-kws.sh && chmod +x /data/kws/start-ax5-kws.sh",
            ax5_env,
            stdin_bytes=f.read(),
        )
        if res.returncode != 0:
            raise RuntimeError(f"Upload start-ax5-kws.sh failed: {res.stderr.decode()}")

    autostart_cmd = (
        "if [ -f /etc/basic_settings/my_script.sh ] && "
        "! grep -q '/data/kws/start-ax5-kws.sh' /etc/basic_settings/my_script.sh; then "
        "printf '\\n# Auto-start Sherpa-ONNX KWS service\\n/data/kws/start-ax5-kws.sh start\\n' "
        ">> /etc/basic_settings/my_script.sh; "
        "fi"
    )
    res = run_ssh(ax5_ip, autostart_cmd, ax5_env)
    if res.returncode != 0:
        raise RuntimeError(f"Autostart setup failed: {res.stderr.decode()}")

    print("Starting ax5-kws-server on AX5...")
    res = run_ssh(ax5_ip, "/data/kws/start-ax5-kws.sh restart", ax5_env)
    print(res.stdout.decode().strip())
    if res.returncode != 0:
        raise RuntimeError(f"start-ax5-kws.sh failed: {res.stderr.decode()}")

    for _ in range(25):
        chk = run_ssh(
            ax5_ip,
            "if grep -q 'ready!' /tmp/ax5-kws.log 2>/dev/null; then echo READY; "
            "elif ! /data/kws/start-ax5-kws.sh status >/dev/null 2>&1; then echo DEAD; fi",
            ax5_env,
        )
        if b"READY" in chk.stdout or b"DEAD" in chk.stdout:
            break
        time.sleep(1)

    status = run_ssh(
        ax5_ip,
        "/data/kws/start-ax5-kws.sh status; rc=$?; tail -n 15 /tmp/ax5-kws.log; df -h /data /tmp; exit $rc",
        ax5_env,
    )
    print(status.stdout.decode().strip())
    if status.returncode != 0:
        raise RuntimeError("ax5-kws-server is not running after startup")


def main():
    parser = argparse.ArgumentParser(description="Build & deploy AX5 Sherpa-ONNX UDP XPCM KWS server")
    parser.add_argument("--ax5-ip", default=os.environ.get("AX5_IP", "192.168.31.1"))
    parser.add_argument("--ax5-pass", default=os.environ.get("AX5_PASS", "admin"))
    parser.add_argument("--zig", required=True, help="Path to zig binary")
    parser.add_argument("--sherpa-include", required=True, help="Path to sherpa-onnx include dir")
    parser.add_argument("--sherpa-libs", required=True, help="Path to stripped sherpa/onnxruntime/libstdc++ libs")
    parser.add_argument("--glibc-dir", required=True, help="Path to glibc 2.36 armhf runtime libs")
    parser.add_argument("--model-dir", default=os.path.join(REPO_ROOT, "sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20"))
    parser.add_argument("--out-dir", default=os.path.join(AX5_DIR, "dist"))
    parser.add_argument("--build-only", action="store_true")
    args = parser.parse_args()

    ax5_bin = compile_ax5_server(args.zig, args.sherpa_include, args.out_dir)
    bundle_path = os.path.join(args.out_dir, "kws_runtime.tar.gz")
    build_ax5_bundle(ax5_bin, args.model_dir, args.sherpa_libs, args.glibc_dir, bundle_path)

    if args.build_only:
        print("Build-only complete.")
        return

    with tempfile.TemporaryDirectory() as tmpdir:
        ax5_ask = make_askpass(args.ax5_pass, tmpdir, "ax5") if args.ax5_pass else None
        deploy_ax5(args.ax5_ip, ssh_env(ax5_ask), bundle_path)


if __name__ == "__main__":
    main()
