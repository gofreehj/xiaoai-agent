#!/bin/sh
# Redmi AX5 Sherpa-ONNX 3.3M KWS service manager
set -u

KWS_DATA_DIR=${AX5_KWS_DATA_DIR:-/data/kws}
KWS_RUN_DIR=${AX5_KWS_RUN_DIR:-/tmp/kws}
KWS_BUNDLE=${AX5_KWS_BUNDLE:-$KWS_DATA_DIR/kws_runtime.tar.gz}
KWS_KEYWORDS=${AX5_KWS_KEYWORDS:-$KWS_DATA_DIR/keywords.txt}
KWS_LOG=${AX5_KWS_LOG:-/tmp/ax5-kws.log}
KWS_PID_FILE=${AX5_KWS_PID_FILE:-/tmp/ax5-kws.pid}

NUM_THREADS=${AX5_KWS_THREADS:-2}
KEYWORDS_THRESHOLD=${AX5_KWS_THRESHOLD:-0.05}
KEYWORDS_SCORE=${AX5_KWS_SCORE:-1.5}
NUM_TRAILING_BLANKS=${AX5_KWS_NUM_TRAILING_BLANKS:-0}
MAX_ACTIVE_PATHS=${AX5_KWS_MAX_PATHS:-12}
LISTEN_PORT=${AX5_KWS_LISTEN_PORT:-5005}
WAKE_PORT=${AX5_KWS_WAKE_PORT:-0}
COOLDOWN_MS=${AX5_KWS_COOLDOWN_MS:-1200}
VAD_RMS=${AX5_KWS_VAD_RMS:-0}
HANGOVER_MS=${AX5_KWS_HANGOVER_MS:-2500}
STATS_SEC=${AX5_KWS_STATS_SEC:-0}

log() {
    printf '%s\n' "ax5-kws: $*"
}

is_running() {
    if [ -f "$KWS_PID_FILE" ]; then
        pid=$(cat "$KWS_PID_FILE" 2>/dev/null)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
    fi
    return 1
}

prepare_runtime() {
    need_extract=0
    bundle_sig=""
    if [ -f "$KWS_BUNDLE" ]; then
        bundle_sig=$(md5sum "$KWS_BUNDLE" 2>/dev/null | awk '{print $1}')
    fi
    old_sig=$(cat "$KWS_RUN_DIR/.bundle_sig" 2>/dev/null || true)

    if [ ! -x "$KWS_RUN_DIR/bin/ax5-kws-server" ] || [ ! -x "$KWS_RUN_DIR/lib/ld-linux-armhf.so.3" ] || [ ! -f "$KWS_RUN_DIR/models/tokens.txt" ]; then
        need_extract=1
    elif [ -n "$bundle_sig" ] && [ "$bundle_sig" != "$old_sig" ]; then
        need_extract=1
    fi

    if [ "$need_extract" -eq 1 ]; then
        if [ ! -f "$KWS_BUNDLE" ]; then
            log "error: runtime bundle not found at $KWS_BUNDLE"
            return 1
        fi
        log "extracting runtime bundle to $KWS_RUN_DIR..."
        rm -rf "$KWS_RUN_DIR"
        mkdir -p "$KWS_RUN_DIR"
        tar -xzf "$KWS_BUNDLE" -C "$KWS_RUN_DIR" || return 1
        chmod +x "$KWS_RUN_DIR/bin/ax5-kws-server" "$KWS_RUN_DIR/lib/ld-linux-armhf.so.3" 2>/dev/null || true
        [ -n "$bundle_sig" ] && printf '%s\n' "$bundle_sig" >"$KWS_RUN_DIR/.bundle_sig" 2>/dev/null || true
    fi

    if [ ! -f "$KWS_KEYWORDS" ]; then
        log "error: keywords file not found at $KWS_KEYWORDS"
        return 1
    fi
    cp "$KWS_KEYWORDS" "$KWS_RUN_DIR/keywords.txt"
}

stop_orphans() {
    for p in /proc/[0-9]*; do
        opid=${p#/proc/}
        [ "$opid" = "$$" ] || [ "$opid" = "$PPID" ] && continue
        [ -r "$p/cmdline" ] || continue
        cmdline=$(tr '\0' ' ' <"$p/cmdline" 2>/dev/null)
        case "$cmdline" in
            sh\ -c*|ash\ -c*|*/sh\ -c*|*/ash\ -c*|ssh\ *|sshd:*)
                continue
                ;;
            *ax5-kws-server*)
                kill "$opid" 2>/dev/null || true
                ;;
        esac
    done
}

start_kws() {
    if is_running; then
        log "already running (PID: $(cat "$KWS_PID_FILE"))"
        return 0
    fi

    prepare_runtime || return 1

    stop_orphans
    rm -f "$KWS_PID_FILE"

    # Cap log size on tmpfs
    if [ -f "$KWS_LOG" ] && [ "$(wc -c <"$KWS_LOG" 2>/dev/null || echo 0)" -gt 262144 ]; then
        tail -n 200 "$KWS_LOG" >"$KWS_LOG.tmp" 2>/dev/null && mv "$KWS_LOG.tmp" "$KWS_LOG" || true
    fi

    log "starting ax5-kws-server (UDP :$LISTEN_PORT -> :$WAKE_PORT, threads=$NUM_THREADS, vad_rms=$VAD_RMS)..."
    "$KWS_RUN_DIR/lib/ld-linux-armhf.so.3" \
        --library-path "$KWS_RUN_DIR/lib" \
        "$KWS_RUN_DIR/bin/ax5-kws-server" \
        --sherpa-lib "$KWS_RUN_DIR/lib/libsherpa-onnx-c-api.so" \
        --encoder "$KWS_RUN_DIR/models/encoder-epoch-13-avg-2-chunk-8-left-64.int8.onnx" \
        --decoder "$KWS_RUN_DIR/models/decoder-epoch-13-avg-2-chunk-8-left-64.onnx" \
        --joiner "$KWS_RUN_DIR/models/joiner-epoch-13-avg-2-chunk-8-left-64.int8.onnx" \
        --tokens "$KWS_RUN_DIR/models/tokens.txt" \
        --keywords-file "$KWS_RUN_DIR/keywords.txt" \
        --num-threads "$NUM_THREADS" \
        --num-trailing-blanks "$NUM_TRAILING_BLANKS" \
        --max-active-paths "$MAX_ACTIVE_PATHS" \
        --keywords-threshold "$KEYWORDS_THRESHOLD" \
        --keywords-score "$KEYWORDS_SCORE" \
        --listen-port "$LISTEN_PORT" \
        --wake-port "$WAKE_PORT" \
        --cooldown-ms "$COOLDOWN_MS" \
        --vad-rms "$VAD_RMS" \
        --hangover-ms "$HANGOVER_MS" \
        --stats-sec "$STATS_SEC" \
        >>"$KWS_LOG" 2>&1 </dev/null &

    pid=$!
    echo "$pid" >"$KWS_PID_FILE"
    sleep 1
    if kill -0 "$pid" 2>/dev/null; then
        log "started (PID: $pid, log: $KWS_LOG)"
        return 0
    fi
    log "error: ax5-kws-server exited immediately, see $KWS_LOG"
    rm -f "$KWS_PID_FILE"
    return 1
}

stop_kws() {
    if [ -f "$KWS_PID_FILE" ]; then
        pid=$(cat "$KWS_PID_FILE" 2>/dev/null)
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            sleep 1
            kill -9 "$pid" 2>/dev/null || true
        fi
        rm -f "$KWS_PID_FILE"
    fi
    stop_orphans
    log "stopped"
}

status_kws() {
    if is_running; then
        pid=$(cat "$KWS_PID_FILE")
        log "running (PID: $pid)"
        grep -E "VmPeak|VmRSS" "/proc/$pid/status" 2>/dev/null || true
        return 0
    fi
    log "not running"
    return 1
}

case "${1:-start}" in
    start)
        start_kws
        ;;
    stop)
        stop_kws
        ;;
    restart)
        stop_kws
        sleep 1
        start_kws
        ;;
    status)
        status_kws
        ;;
    *)
        echo "usage: $0 {start|stop|restart|status}" >&2
        exit 2
        ;;
esac
