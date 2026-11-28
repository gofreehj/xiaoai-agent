//! Async `aplay` stdin PCM streaming player + zero-flash synthetic session cues.
//!
//! 1:1 Rust port of `xiaoai-doubao/xiaoai-plus/src/audio/player_aplay.cpp` and
//! `session_cue.hpp::MakeSessionCue`:
//! - Lazy spawns `aplay -q -D <device> -t raw -f S16_LE -r <rate> -c <ch>` on first PCM chunk
//! - Generation token fencing discards stale frames immediately
//! - Bounded internal queue (`<= 512 KiB`, LX06 RAM guardrail)
//! - Saturating `i16` gain scaling when `playback_gain < 1.0`
//! - Exact `finish_and_drain()` via closing stdin and awaiting child exit
//! - Immediate `interrupt()` via generation bump + queue drop + child kill/reap

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// LX06 memory guardrail: maximum queued raw PCM bytes awaiting `aplay` stdin write.
pub const MAX_QUEUE_BYTES: usize = 512 * 1024;
const QUEUE_CHANNEL_CAP: usize = 64;
const INTERRUPT_REAP_TIMEOUT: Duration = Duration::from_secs(2);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueKind {
    None,
    Wake,
    Exit,
    Listen,
}

/// Synthesize a 160 ms 24 kHz mono S16_LE tone sweep with 12 ms linear fade-in/out.
///
/// 1:1 port of `xiaoai_plus::audio::MakeSessionCue` (zero `/data` flash usage).
pub fn make_session_cue_pcm(kind: CueKind, gain: f32) -> Bytes {
    if kind == CueKind::None {
        return Bytes::new();
    }
    const SAMPLES: usize = 3840; // 160 ms @ 24 kHz
    const EDGE: usize = 288; // 12 ms fade-in / fade-out
    let mut pcm = vec![0u8; SAMPLES * 2];
    let mut phase = 0.0_f64;
    let gain_f64 = gain.max(0.0) as f64;

    for i in 0..SAMPLES {
        let t = (i as f64) / (SAMPLES as f64);
        let hz = if kind == CueKind::Exit {
            660.0 - 220.0 * t
        } else {
            660.0 + 220.0 * t
        };
        let fade_in = (i as f64) / (EDGE as f64);
        let fade_out = ((SAMPLES - 1 - i) as f64) / (EDGE as f64);
        let envelope = 1.0_f64.min(fade_in.min(fade_out));
        let raw_val = phase.sin() * 32767.0 * 0.12 * gain_f64 * envelope;
        let sample = raw_val.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        phase += 2.0 * std::f64::consts::PI * hz / 24_000.0;
        let bytes = sample.to_le_bytes();
        pcm[2 * i] = bytes[0];
        pcm[2 * i + 1] = bytes[1];
    }

    Bytes::from(pcm)
}

/// Apply saturating `i16` gain scaling when `gain < 0.999`.
pub fn scale_s16_pcm(pcm: &[u8], gain: f32) -> Vec<u8> {
    if gain >= 0.999 {
        return pcm.to_vec();
    }
    let clamped_gain = gain.max(0.0) as f64;
    let mut out = Vec::with_capacity(pcm.len());
    for chunk in pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        let scaled = ((sample as f64) * clamped_gain).round() as i32;
        let sat = scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        out.extend_from_slice(&sat.to_le_bytes());
    }
    if pcm.len() % 2 == 1 {
        out.push(*pcm.last().unwrap());
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamAudioSpec {
    pub aplay_path: String,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub playback_gain: f32,
    /// Optional explicit command + args override (used in cross-platform tests).
    pub command_override: Option<(String, Vec<String>)>,
}

impl Default for StreamAudioSpec {
    fn default() -> Self {
        Self {
            aplay_path: "aplay".to_string(),
            device: "default".to_string(),
            sample_rate: 24_000,
            channels: 1,
            playback_gain: 1.0,
            command_override: None,
        }
    }
}

impl StreamAudioSpec {
    fn build_command(&self) -> Command {
        if let Some((prog, args)) = &self.command_override {
            let mut cmd = Command::new(prog);
            cmd.args(args);
            return cmd;
        }
        let mut cmd = Command::new(&self.aplay_path);
        cmd.args([
            "-q",
            "-D",
            &self.device,
            "-t",
            "raw",
            "-f",
            "S16_LE",
            "-r",
            &self.sample_rate.to_string(),
            "-c",
            &self.channels.to_string(),
        ]);
        cmd
    }
}

struct ActiveTurn {
    generation: u64,
    tx: Option<mpsc::Sender<Bytes>>,
    cancel_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<anyhow::Result<()>>,
}

struct PlayerInner {
    default_spec: StreamAudioSpec,
    active: Option<ActiveTurn>,
}

/// Shared async streaming PCM player backed by an `aplay` child process.
pub struct AplayStreamPlayer {
    generation: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    queued_bytes: Arc<AtomicUsize>,
    inner: Mutex<PlayerInner>,
}

#[allow(dead_code)]
impl AplayStreamPlayer {
    pub fn new(default_spec: StreamAudioSpec) -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(1)),
            playing: Arc::new(AtomicBool::new(false)),
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            inner: Mutex::new(PlayerInner {
                default_spec,
                active: None,
            }),
        }
    }

    /// Current active generation token.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether an `aplay` child process is currently active and playing audio.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Current queued PCM bytes waiting to be written to `aplay`.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }

    /// Snapshot of the default playback specification.
    pub async fn default_spec(&self) -> StreamAudioSpec {
        self.inner.lock().await.default_spec.clone()
    }

    /// Begin a new playback turn (cancelling any prior turn) and return its generation token.
    pub async fn start_turn(&self, spec_override: Option<StreamAudioSpec>) -> u64 {
        // Bump generation immediately so any in-flight push_pcm sees stale token.
        let next_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;

        let prev = {
            let mut inner = self.inner.lock().await;
            inner.active.take()
        };
        if let Some(prev_turn) = prev {
            Self::stop_turn(prev_turn, &self.playing, &self.queued_bytes).await;
        }

        let mut inner = self.inner.lock().await;
        let spec = spec_override.unwrap_or_else(|| inner.default_spec.clone());
        let (tx, rx) = mpsc::channel::<Bytes>(QUEUE_CHANNEL_CAP);
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();

        let gen_arc = self.generation.clone();
        let playing_arc = self.playing.clone();
        let queued_arc = self.queued_bytes.clone();

        let handle = tokio::spawn(async move {
            run_writer_task(next_gen, spec, rx, cancel_rx, gen_arc, playing_arc, queued_arc).await
        });

        inner.active = Some(ActiveTurn {
            generation: next_gen,
            tx: Some(tx),
            cancel_tx: Some(cancel_tx),
            handle,
        });
        next_gen
    }

    /// Push a raw `S16_LE` PCM chunk for `generation`.
    ///
    /// Returns `Ok(false)` if `generation` is stale or the 512 KiB queue cap would be exceeded.
    pub async fn push_pcm(&self, generation: u64, pcm: Bytes) -> anyhow::Result<bool> {
        if pcm.is_empty() {
            return Ok(true);
        }
        if self.generation.load(Ordering::Acquire) != generation {
            return Ok(false);
        }

        let chunk_len = pcm.len();
        // Reserve queue bytes atomically under the 512 KiB LX06 cap.
        loop {
            let cur = self.queued_bytes.load(Ordering::Acquire);
            if cur.saturating_add(chunk_len) > MAX_QUEUE_BYTES {
                warn!(
                    queued = cur,
                    incoming = chunk_len,
                    cap = MAX_QUEUE_BYTES,
                    "aplay stream queue reached 512 KiB cap; dropping chunk"
                );
                return Ok(false);
            }
            if self
                .queued_bytes
                .compare_exchange(
                    cur,
                    cur + chunk_len,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                break;
            }
        }

        let tx_opt = {
            let inner = self.inner.lock().await;
            inner.active.as_ref().and_then(|t| {
                if t.generation == generation {
                    t.tx.clone()
                } else {
                    None
                }
            })
        };

        let Some(tx) = tx_opt else {
            self.queued_bytes.fetch_sub(chunk_len, Ordering::AcqRel);
            return Ok(false);
        };

        if tx.send(pcm).await.is_err() {
            self.queued_bytes.fetch_sub(chunk_len, Ordering::AcqRel);
            if self.generation.load(Ordering::Acquire) != generation {
                return Ok(false);
            }
            anyhow::bail!("aplay stream writer closed unexpectedly (broken pipe)");
        }

        Ok(true)
    }

    /// Close `aplay` stdin for `generation` and wait up to `timeout` for playback to drain.
    ///
    /// Returns `Ok(true)` if drained cleanly, `Ok(false)` if interrupted/stale/timed out.
    pub async fn finish_and_drain(
        &self,
        generation: u64,
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        let turn = {
            let mut inner = self.inner.lock().await;
            match inner.active.as_ref() {
                Some(t) if t.generation == generation => inner.active.take(),
                _ => return Ok(false),
            }
        };
        let Some(mut turn) = turn else {
            return Ok(false);
        };

        // Drop sender so the writer task sees EOF on rx, closes aplay stdin, and waits for exit.
        turn.tx.take();

        match tokio::time::timeout(timeout, &mut turn.handle).await {
            Ok(Ok(Ok(()))) => Ok(self.generation.load(Ordering::Acquire) == generation),
            Ok(Ok(Err(err))) => {
                if self.generation.load(Ordering::Acquire) != generation {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
            Ok(Err(join_err)) => {
                warn!(error = %join_err, "aplay writer task panicked/cancelled");
                self.playing.store(false, Ordering::Release);
                Ok(false)
            }
            Err(_) => {
                warn!(timeout_ms = timeout.as_millis() as u64, "aplay drain timed out; killing child");
                if let Some(cancel) = turn.cancel_tx.take() {
                    let _ = cancel.send(());
                }
                let _ = tokio::time::timeout(INTERRUPT_REAP_TIMEOUT, &mut turn.handle).await;
                turn.handle.abort();
                self.playing.store(false, Ordering::Release);
                self.queued_bytes.store(0, Ordering::Release);
                Ok(false)
            }
        }
    }

    /// Immediately interrupt active playback: bump generation, drop queued PCM, kill & reap child.
    pub async fn interrupt(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let prev = {
            let mut inner = self.inner.lock().await;
            inner.active.take()
        };
        if let Some(prev_turn) = prev {
            Self::stop_turn(prev_turn, &self.playing, &self.queued_bytes).await;
        } else {
            self.playing.store(false, Ordering::Release);
            self.queued_bytes.store(0, Ordering::Release);
        }
    }

    async fn stop_turn(
        mut turn: ActiveTurn,
        playing: &AtomicBool,
        queued_bytes: &AtomicUsize,
    ) {
        turn.tx.take();
        if let Some(cancel) = turn.cancel_tx.take() {
            let _ = cancel.send(());
        }
        if tokio::time::timeout(INTERRUPT_REAP_TIMEOUT, &mut turn.handle)
            .await
            .is_err()
        {
            warn!("aplay interrupt reap timed out; aborting writer task");
            turn.handle.abort();
        }
        playing.store(false, Ordering::Release);
        queued_bytes.store(0, Ordering::Release);
    }
}

async fn run_writer_task(
    my_gen: u64,
    spec: StreamAudioSpec,
    mut rx: mpsc::Receiver<Bytes>,
    mut cancel_rx: oneshot::Receiver<()>,
    generation: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    queued_bytes: Arc<AtomicUsize>,
) -> anyhow::Result<()> {
    // Lazy-spawn: wait for the first non-empty chunk before opening ALSA / spawning aplay.
    let first_chunk = loop {
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                drain_queued_bytes(&mut rx, &queued_bytes);
                return Ok(());
            }
            maybe = rx.recv() => {
                match maybe {
                    Some(chunk) => {
                        sub_queued(&queued_bytes, chunk.len());
                        if !chunk.is_empty() {
                            break chunk;
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    };

    if generation.load(Ordering::Acquire) != my_gen {
        drain_queued_bytes(&mut rx, &queued_bytes);
        return Ok(());
    }

    let mut cmd = spec.build_command();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn aplay ({})", spec.aplay_path))?;
    let mut stdin = child
        .stdin
        .take()
        .context("aplay child missing piped stdin")?;

    playing.store(true, Ordering::Release);
    debug!(gen = my_gen, rate = spec.sample_rate, ch = spec.channels, "aplay stream spawned");

    let mut current_chunk = Some(first_chunk);
    let mut write_err: Option<anyhow::Error> = None;
    let mut canceled = false;

    loop {
        let chunk = match current_chunk.take() {
            Some(c) => c,
            None => {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        canceled = true;
                        break;
                    }
                    maybe = rx.recv() => {
                        match maybe {
                            Some(c) => {
                                sub_queued(&queued_bytes, c.len());
                                if c.is_empty() {
                                    continue;
                                }
                                c
                            }
                            None => break,
                        }
                    }
                }
            }
        };

        if generation.load(Ordering::Acquire) != my_gen {
            canceled = true;
            break;
        }

        let payload = scale_s16_pcm(&chunk, spec.playback_gain);
        tokio::select! {
            biased;
            _ = &mut cancel_rx => {
                canceled = true;
                break;
            }
            res = stdin.write_all(&payload) => {
                if let Err(err) = res {
                    write_err = Some(anyhow::Error::new(err).context("aplay stdin write failed"));
                    break;
                }
            }
        }
    }

    // Close stdin so aplay either drains cleanly or releases pipe.
    drop(stdin);

    if canceled || write_err.is_some() || generation.load(Ordering::Acquire) != my_gen {
        drain_queued_bytes(&mut rx, &queued_bytes);
        let _ = child.kill().await;
        let _ = child.wait().await;
        playing.store(false, Ordering::Release);
        return match write_err {
            Some(err) if !canceled && generation.load(Ordering::Acquire) == my_gen => Err(err),
            _ => Ok(()),
        };
    }

    // Normal drain: wait for aplay to finish playing buffered audio, still interruptible.
    let wait_res = tokio::select! {
        biased;
        _ = &mut cancel_rx => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            playing.store(false, Ordering::Release);
            return Ok(());
        }
        status = child.wait() => status,
    };

    playing.store(false, Ordering::Release);
    match wait_res {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => anyhow::bail!("aplay exited with non-zero status: {status}"),
        Err(err) => Err(anyhow::Error::new(err).context("failed to wait for aplay")),
    }
}

fn sub_queued(queued: &AtomicUsize, n: usize) {
    loop {
        let cur = queued.load(Ordering::Acquire);
        let next = cur.saturating_sub(n);
        if queued
            .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break;
        }
    }
}

fn drain_queued_bytes(rx: &mut mpsc::Receiver<Bytes>, queued: &AtomicUsize) {
    while let Ok(chunk) = rx.try_recv() {
        sub_queued(queued, chunk.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_out_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-aplay-test-{tag}-{}-{stamp}.bin",
            std::process::id()
        ))
    }

    /// Build a cross-platform mock `aplay` command that reads stdin to `out_file`.
    fn mock_aplay_sink(out_file: &Path) -> StreamAudioSpec {
        let out_str = out_file.to_string_lossy().to_string();
        #[cfg(windows)]
        let command_override = Some((
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                format!(
                    "$in = [Console]::OpenStandardInput(); $out = [IO.File]::Create('{}'); $in.CopyTo($out); $out.Close()",
                    out_str.replace('\'', "''")
                ),
            ],
        ));
        #[cfg(not(windows))]
        let command_override = Some((
            "sh".to_string(),
            vec!["-c".to_string(), format!("cat > '{}'", out_str)],
        ));

        StreamAudioSpec {
            aplay_path: "mock-aplay".to_string(),
            device: "default".to_string(),
            sample_rate: 24_000,
            channels: 1,
            playback_gain: 1.0,
            command_override,
        }
    }

    /// Build a cross-platform mock `aplay` that blocks until killed.
    fn mock_aplay_blocking() -> StreamAudioSpec {
        #[cfg(windows)]
        let command_override = Some((
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Start-Sleep -Seconds 30".to_string(),
            ],
        ));
        #[cfg(not(windows))]
        let command_override = Some((
            "sh".to_string(),
            vec!["-c".to_string(), "sleep 30".to_string()],
        ));

        StreamAudioSpec {
            aplay_path: "mock-aplay-sleep".to_string(),
            device: "default".to_string(),
            sample_rate: 24_000,
            channels: 1,
            playback_gain: 1.0,
            command_override,
        }
    }

    #[test]
    fn make_session_cue_pcm_has_expected_length_and_soft_edges() {
        assert!(make_session_cue_pcm(CueKind::None, 1.0).is_empty());

        let wake = make_session_cue_pcm(CueKind::Wake, 1.0);
        let exit = make_session_cue_pcm(CueKind::Exit, 1.0);
        assert_eq!(wake.len(), 3840 * 2);
        assert_eq!(exit.len(), 3840 * 2);
        assert_ne!(wake, exit, "wake sweeps up and exit sweeps down");

        let first = i16::from_le_bytes([wake[0], wake[1]]);
        let last = i16::from_le_bytes([wake[wake.len() - 2], wake[wake.len() - 1]]);
        assert_eq!(first, 0, "first sample must be faded to 0");
        assert_eq!(last, 0, "last sample must be faded to 0");

        let mid_peak = wake[288 * 2..(3840 - 288) * 2]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).abs())
            .max()
            .unwrap_or(0);
        assert!(mid_peak > 3000, "mid-body tone should be audible, got {mid_peak}");
    }

    #[test]
    fn scale_s16_pcm_applies_gain_and_saturates() {
        let mut raw = Vec::new();
        for s in [10000i16, -20000i16, i16::MAX, i16::MIN] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        let scaled = scale_s16_pcm(&raw, 0.5);
        let samples: Vec<i16> = scaled
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![5000, -10000, 16384, -16384]);
    }

    #[tokio::test]
    async fn push_pcm_scales_gain_drains_cleanly_and_drops_stale_generation() {
        let out_file = temp_out_path("drain");
        let mut spec = mock_aplay_sink(&out_file);
        spec.playback_gain = 0.5;
        let player = AplayStreamPlayer::new(spec);

        let gen1 = player.start_turn(None).await;
        let stale_gen = gen1 - 1;

        // Stale generation frame must be dropped and NOT written.
        let stale_bytes = Bytes::from(vec![0x7f, 0x7f, 0x7f, 0x7f]);
        assert!(
            !player.push_pcm(stale_gen, stale_bytes).await.unwrap(),
            "stale generation push_pcm must return false"
        );

        let mut raw = Vec::new();
        for s in [1000i16, -2000i16, 4000i16, -8000i16] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        assert!(player.push_pcm(gen1, Bytes::from(raw)).await.unwrap());

        let drained = player
            .finish_and_drain(gen1, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(drained, "finish_and_drain must return true on clean exit");
        assert!(!player.is_playing());

        let written = std::fs::read(&out_file).unwrap();
        let _ = std::fs::remove_file(&out_file);
        let got: Vec<i16> = written
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(
            got,
            vec![500, -1000, 2000, -4000],
            "only current generation scaled PCM should reach aplay"
        );
    }

    #[tokio::test]
    async fn interrupt_terminates_blocking_child_promptly() {
        let player = AplayStreamPlayer::new(mock_aplay_blocking());
        let gen1 = player.start_turn(None).await;
        assert!(player
            .push_pcm(gen1, Bytes::from_static(&[1, 2, 3, 4]))
            .await
            .unwrap());

        // Wait briefly for child spawn.
        for _ in 0..40 {
            if player.is_playing() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(player.is_playing(), "blocking mock aplay should be playing");

        let started = std::time::Instant::now();
        player.interrupt().await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "interrupt must terminate child promptly"
        );
        assert!(!player.is_playing());
        assert_eq!(player.queued_bytes(), 0);
        // Old generation must now be rejected.
        assert!(!player
            .push_pcm(gen1, Bytes::from_static(&[5, 6]))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn queue_enforces_512_kib_cap() {
        let player = AplayStreamPlayer::new(mock_aplay_blocking());
        let gen1 = player.start_turn(None).await;
        let big = Bytes::from(vec![0u8; MAX_QUEUE_BYTES + 2]);
        assert!(
            !player.push_pcm(gen1, big).await.unwrap(),
            "chunk exceeding 512 KiB cap must be rejected"
        );
        player.interrupt().await;
    }
}
