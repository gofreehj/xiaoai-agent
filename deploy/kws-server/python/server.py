#!/usr/bin/env python3
"""
Generic Sherpa-ONNX UDP XPCM KWS Server for XiaoAI Agent (dual mode).

Implements docs/external-kws-protocol.md for NAS, Raspberry Pi, Linux, macOS,
or Docker hosts. Receives 16 kHz mono S16_LE PCM frames wrapped in 8-byte XPCM
UDP headers and replies XPONG / WAKE:<seq>:<keyword> directly to the sender's
source UDP address.
"""
from __future__ import annotations

import argparse
import glob
import os
import socket
import struct
import time

import numpy as np
import sherpa_onnx

XPCM_MAGIC = b"XPCM"
XPCM_HDR_LEN = 8
XPCM_FLAG_PCM = 1
XPCM_FLAG_RESET = 2
XPCM_FLAG_PING = 3
XPCM_FLAG_SYS_ACTIVE = 0x0100
SAMPLE_RATE = 16000


def find_model_file(model_dir: str, pattern: str) -> str:
    matches = sorted(glob.glob(os.path.join(model_dir, pattern)))
    if not matches:
        raise FileNotFoundError(f"Missing model file matching {pattern!r} in {model_dir}")
    # Prefer int8 quantized file when both fp32 and int8 exist
    for path in matches:
        if "int8" in os.path.basename(path):
            return path
    return matches[0]


def create_spotter(args: argparse.Namespace) -> sherpa_onnx.KeywordSpotter:
    encoder = args.encoder or find_model_file(args.model_dir, "*encoder*.onnx")
    decoder = args.decoder or find_model_file(args.model_dir, "*decoder*.onnx")
    joiner = args.joiner or find_model_file(args.model_dir, "*joiner*.onnx")
    tokens = args.tokens or find_model_file(args.model_dir, "tokens.txt")

    print(
        f"[KWS] Loading Sherpa-ONNX model:\n"
        f"  encoder = {encoder}\n"
        f"  decoder = {decoder}\n"
        f"  joiner  = {joiner}\n"
        f"  tokens  = {tokens}\n"
        f"  keywords= {args.keywords_file}",
        flush=True,
    )
    return sherpa_onnx.KeywordSpotter(
        tokens=tokens,
        encoder=encoder,
        decoder=decoder,
        joiner=joiner,
        num_threads=args.num_threads,
        keywords_file=args.keywords_file,
        keywords_score=args.keywords_score,
        keywords_threshold=args.keywords_threshold,
        max_active_paths=args.max_active_paths,
    )


class ClientSession:
    def __init__(self, kws: sherpa_onnx.KeywordSpotter) -> None:
        self.stream = kws.create_stream()
        self.last_wake_at = 0.0
        self.last_sys_active_at = 0.0
        self.wake_seq = 0


def main() -> None:
    parser = argparse.ArgumentParser(description="Generic Sherpa-ONNX UDP XPCM KWS server")
    parser.add_argument("--model-dir", required=True, help="Directory containing Sherpa-ONNX KWS ONNX & tokens.txt")
    parser.add_argument("--keywords-file", required=True, help="Path to keywords.txt (with tone phonemes)")
    parser.add_argument("--encoder", default="", help="Optional override encoder ONNX path")
    parser.add_argument("--decoder", default="", help="Optional override decoder ONNX path")
    parser.add_argument("--joiner", default="", help="Optional override joiner ONNX path")
    parser.add_argument("--tokens", default="", help="Optional override tokens.txt path")
    parser.add_argument("--host", default="0.0.0.0", help="UDP listen host (default: 0.0.0.0)")
    parser.add_argument("--port", type=int, default=5005, help="UDP listen port (default: 5005)")
    parser.add_argument("--num-threads", type=int, default=2, help="ONNX runtime threads (default: 2)")
    parser.add_argument("--max-active-paths", type=int, default=12)
    parser.add_argument("--keywords-score", type=float, default=2.0)
    parser.add_argument("--keywords-threshold", type=float, default=0.15)
    parser.add_argument("--cooldown-ms", type=int, default=1500)
    args = parser.parse_args()

    kws = create_spotter(args)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.settimeout(0.05)
    sock.bind((args.host, args.port))
    print(f"[KWS] Listening on udp://{args.host}:{args.port} (ready!)", flush=True)

    sessions: dict[tuple[str, int], ClientSession] = {}
    pending_retry: tuple[tuple[str, int], bytes, float] | None = None

    try:
        while True:
            now = time.monotonic()
            if pending_retry is not None and now >= pending_retry[2]:
                sock.sendto(pending_retry[1], pending_retry[0])
                pending_retry = None

            try:
                pkt, src = sock.recvfrom(4096)
            except TimeoutError:
                continue

            if len(pkt) >= XPCM_HDR_LEN and pkt[:4] == XPCM_MAGIC:
                _, flags = struct.unpack_from("<HH", pkt, 4)
                pcm = pkt[XPCM_HDR_LEN:]
            else:
                flags = XPCM_FLAG_PCM
                pcm = pkt

            base_flags = flags & 0x00FF
            sys_active = (flags & XPCM_FLAG_SYS_ACTIVE) != 0

            if base_flags == XPCM_FLAG_PING:
                sock.sendto(b"XPONG\n", src)
                continue

            session = sessions.get(src)
            if session is None:
                session = ClientSession(kws)
                sessions[src] = session
                print(f"[KWS] Client connected: {src[0]}:{src[1]}", flush=True)

            if sys_active:
                session.last_sys_active_at = now
            in_sys_window = sys_active or (
                session.last_sys_active_at > 0 and (now - session.last_sys_active_at) <= 0.8
            )

            if base_flags == XPCM_FLAG_RESET:
                kws.reset_stream(session.stream)
                continue

            if base_flags != XPCM_FLAG_PCM or not pcm or (len(pcm) % 2) != 0:
                continue

            samples = np.frombuffer(pcm, dtype=np.int16).astype(np.float32) / 32768.0
            session.stream.accept_waveform(SAMPLE_RATE, samples)

            while kws.is_ready(session.stream):
                kws.decode_stream(session.stream)
                kw = kws.get_result(session.stream)
                if not kw:
                    continue
                if in_sys_window and kw == "__CMD_EXIT__":
                    kws.reset_stream(session.stream)
                    continue
                if (now - session.last_wake_at) * 1000.0 >= args.cooldown_ms:
                    session.last_wake_at = now
                    session.wake_seq += 1
                    payload = f"WAKE:{session.wake_seq}:{kw}\n".encode("utf-8")
                    print(
                        f"[KWS] TRIGGERED peer={src[0]}:{src[1]} seq={session.wake_seq} kw={kw!r}",
                        flush=True,
                    )
                    sock.sendto(payload, src)
                    pending_retry = (src, payload, now + 0.02)
                kws.reset_stream(session.stream)
    except KeyboardInterrupt:
        print("\n[KWS] Shutting down...", flush=True)
    finally:
        sock.close()


if __name__ == "__main__":
    main()
