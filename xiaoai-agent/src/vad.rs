use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use tracing::{debug, info};

use crate::config::CaptureConfig;

pub const BYTES_PER_SAMPLE: usize = 2;
const NOISE_FLOOR_WINDOW: usize = 40;
const MIN_NOISE_FLOOR_SAMPLES: usize = 5;
const CONTINUE_HYSTERESIS: f64 = 0.75;
const SHORT_UTTERANCE_VOICED_MS: u64 = 1_200;
const LONG_UTTERANCE_VOICED_MS: u64 = 3_000;

pub enum SpeechEvent {
    SpeechStart(Bytes),
    SpeechChunk(Bytes),
    SpeechRejected,
    Utterance(Bytes),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UtteranceStats {
    pub voiced_ms: u64,
    pub silence_tail_ms: u64,
    pub duration_ms: u64,
}

pub struct SpeechCollector {
    sample_rate: u32,
    block_bytes: usize,
    threshold: f64,
    adaptive_noise_floor: bool,
    noise_floor_multiplier: f64,
    mic_gain: f64,
    pre_roll_blocks: usize,
    silence_ms: u64,
    dynamic_silence: bool,
    short_utterance_silence_ms: u64,
    long_utterance_silence_ms: u64,
    min_speech_ms: u64,
    max_utterance_ms: u64,
    cooldown: Duration,
    print_levels: bool,
    pending: BytesMut,
    pre_roll: VecDeque<Bytes>,
    utterance: BytesMut,
    in_speech: bool,
    voiced_ms: u64,
    silence_run_ms: u64,
    utterance_ms: u64,
    noise_levels: VecDeque<f64>,
    last_completed: Option<Instant>,
    last_level_log: Instant,
    last_stats: Option<UtteranceStats>,
}

impl SpeechCollector {
    pub fn new(config: &CaptureConfig) -> Self {
        let block_ms = config.block_ms.max(10);
        let block_bytes =
            ((config.sample_rate as u64 * block_ms / 1000) as usize).max(1) * BYTES_PER_SAMPLE;
        let pre_roll_blocks = config.pre_roll_ms.max(block_ms).div_ceil(block_ms) as usize;
        Self {
            sample_rate: config.sample_rate,
            block_bytes,
            threshold: config.threshold,
            adaptive_noise_floor: config.adaptive_noise_floor,
            noise_floor_multiplier: config.noise_floor_multiplier.max(0.0),
            mic_gain: config.mic_gain,
            pre_roll_blocks: pre_roll_blocks.max(1),
            silence_ms: config.silence_ms,
            dynamic_silence: config.dynamic_silence,
            short_utterance_silence_ms: config.short_utterance_silence_ms,
            long_utterance_silence_ms: config.long_utterance_silence_ms,
            min_speech_ms: config.min_speech_ms,
            max_utterance_ms: (config.max_utterance_s.max(1.0) * 1000.0) as u64,
            cooldown: Duration::from_secs_f64(config.cooldown_s.max(0.0)),
            print_levels: config.print_levels,
            pending: BytesMut::new(),
            pre_roll: VecDeque::new(),
            utterance: BytesMut::new(),
            in_speech: false,
            voiced_ms: 0,
            silence_run_ms: 0,
            utterance_ms: 0,
            noise_levels: VecDeque::with_capacity(NOISE_FLOOR_WINDOW),
            last_completed: None,
            last_level_log: Instant::now(),
            last_stats: None,
        }
    }

    pub fn take_last_stats(&mut self) -> Option<UtteranceStats> {
        self.last_stats.take()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SpeechEvent> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(bytes);
        while self.pending.len() >= self.block_bytes {
            // split_to 只切边界、不复制数据；push_block 里再 freeze 成 Bytes，
            // 之后 pre_roll / utterance 共享同一块内存（每秒约 31 个包，原来
            // 每块都要 drain 复制一次 + pre_roll clone 一次）。
            let block = self.pending.split_to(self.block_bytes);
            events.extend(self.push_block(block));
        }
        events
    }

    pub fn flush(&mut self) -> Option<Bytes> {
        self.pending.clear();
        if self.in_speech && self.voiced_ms >= self.min_speech_ms {
            let tail = self.utterance.split().freeze();
            let duration_s =
                tail.len() as f64 / (self.sample_rate as f64 * BYTES_PER_SAMPLE as f64);
            let peak = peak_norm(&tail);
            let voiced_ms = self.voiced_ms;
            self.last_stats = Some(UtteranceStats {
                voiced_ms,
                silence_tail_ms: self.silence_run_ms,
                duration_ms: (duration_s * 1000.0).round() as u64,
            });
            self.reset_after_utterance();
            info!(
                "CAPTURE_UTTERANCE duration={duration_s:.2}s peak={peak:.5} voiced_ms={voiced_ms} end=stream_end"
            );
            Some(tail)
        } else {
            self.utterance.clear();
            self.reset_after_utterance();
            None
        }
    }

    pub fn log_noise_floor(&self, reason: &'static str) {
        self.log_noise_floor_with_trigger(reason, None);
    }

    fn noise_floor_p50(&self) -> f64 {
        if self.noise_levels.is_empty() {
            return 0.0;
        }
        let mut levels: Vec<f64> = self.noise_levels.iter().copied().collect();
        levels.sort_by(f64::total_cmp);
        percentile(&levels, 0.50)
    }

    fn onset_threshold(&self) -> f64 {
        if self.adaptive_noise_floor && self.noise_levels.len() >= MIN_NOISE_FLOOR_SAMPLES {
            let noise_p50 = self.noise_floor_p50();
            self.threshold.max(noise_p50 * self.noise_floor_multiplier)
        } else {
            self.threshold
        }
    }

    fn continue_threshold(&self) -> f64 {
        if self.adaptive_noise_floor && self.noise_levels.len() >= MIN_NOISE_FLOOR_SAMPLES {
            self.onset_threshold() * CONTINUE_HYSTERESIS
        } else {
            self.threshold
        }
    }

    fn effective_silence_ms(&self) -> u64 {
        if !self.dynamic_silence {
            return self.silence_ms;
        }
        if self.voiced_ms < SHORT_UTTERANCE_VOICED_MS {
            self.silence_ms.min(self.short_utterance_silence_ms)
        } else if self.voiced_ms >= LONG_UTTERANCE_VOICED_MS {
            self.silence_ms.max(self.long_utterance_silence_ms)
        } else {
            self.silence_ms
        }
    }

    fn record_noise_sample(&mut self, level: f64) {
        if self.noise_levels.len() >= NOISE_FLOOR_WINDOW {
            self.noise_levels.pop_front();
        }
        self.noise_levels.push_back(level);
    }

    fn push_block(&mut self, mut block: BytesMut) -> Vec<SpeechEvent> {
        if let Some(last_completed) = self.last_completed {
            if !self.in_speech && last_completed.elapsed() < self.cooldown {
                return Vec::new();
            }
        }
        apply_gain(&mut block, self.mic_gain);
        let level = rms_norm(&block);
        if self.print_levels && self.last_level_log.elapsed() >= Duration::from_secs(1) {
            info!("CAPTURE_LEVEL level={level:.5}");
            self.last_level_log = Instant::now();
        }

        let block_ms =
            (block.len() as u64 * 1000) / (self.sample_rate as u64 * BYTES_PER_SAMPLE as u64);

        // 增益与电平算完就冻结：后面的 pre_roll / utterance 只需要只读共享，
        // Bytes::clone 是引用计数，不再复制音频数据。
        let block = block.freeze();

        if !self.in_speech {
            let is_voice = level >= self.onset_threshold();
            if !is_voice {
                self.record_noise_sample(level);
            }
            self.pre_roll.push_back(block.clone());
            while self.pre_roll.len() > self.pre_roll_blocks {
                self.pre_roll.pop_front();
            }
            if is_voice {
                self.in_speech = true;
                self.utterance.clear();
                for item in &self.pre_roll {
                    self.utterance.extend_from_slice(item);
                }
                self.voiced_ms = block_ms;
                self.silence_run_ms = 0;
                self.utterance_ms = block_ms * self.pre_roll.len() as u64;
                self.log_noise_floor_with_trigger("speech_start", Some(level));
                debug!("CAPTURE_SPEECH_START level={level:.5}");
                // split() 把已累积的 pre-roll 整段取走（O(1)，不复制音频）。注意它会把
                // 容量一起带走：utterance 变成容量为 0 的空缓冲，下一轮第一块会重新
                // 分配一次。省掉的是「每块一次分配」，不是「一轮只分配一次」。
                let prefix = self.utterance.split().freeze();
                return vec![SpeechEvent::SpeechStart(prefix)];
            }
            return Vec::new();
        }

        let is_voice = level >= self.continue_threshold();
        self.utterance.extend_from_slice(&block);
        self.utterance_ms += block_ms;
        if is_voice {
            self.voiced_ms += block_ms;
            self.silence_run_ms = 0;
        } else {
            self.silence_run_ms += block_ms;
        }

        let silence_limit_ms = self.effective_silence_ms();
        if self.silence_run_ms < silence_limit_ms && self.utterance_ms < self.max_utterance_ms {
            return vec![SpeechEvent::SpeechChunk(block)];
        }

        let final_chunk = block;
        let utterance = self.utterance.split().freeze();
        let duration_s =
            utterance.len() as f64 / (self.sample_rate as f64 * BYTES_PER_SAMPLE as f64);
        let peak = peak_norm(&utterance);
        let voiced_ms = self.voiced_ms;
        let silence_tail_ms = self.silence_run_ms;
        let end_reason = if self.silence_run_ms >= silence_limit_ms {
            "silence"
        } else {
            "max_duration"
        };
        self.reset_after_utterance();

        if voiced_ms < self.min_speech_ms {
            debug!("CAPTURE_SKIP_SHORT duration={duration_s:.2}s peak={peak:.5}");
            return vec![
                SpeechEvent::SpeechChunk(final_chunk),
                SpeechEvent::SpeechRejected,
            ];
        }

        self.last_stats = Some(UtteranceStats {
            voiced_ms,
            silence_tail_ms,
            duration_ms: (duration_s * 1000.0).round() as u64,
        });

        info!(
            "CAPTURE_UTTERANCE duration={duration_s:.2}s peak={peak:.5} voiced_ms={voiced_ms} end={end_reason}"
        );
        vec![
            SpeechEvent::SpeechChunk(final_chunk),
            SpeechEvent::Utterance(utterance),
        ]
    }

    fn reset_after_utterance(&mut self) {
        self.pre_roll.clear();
        self.utterance.clear();
        self.in_speech = false;
        self.voiced_ms = 0;
        self.silence_run_ms = 0;
        self.utterance_ms = 0;
        self.last_completed = Some(Instant::now());
    }

    fn log_noise_floor_with_trigger(&self, reason: &'static str, trigger_level: Option<f64>) {
        let mut levels: Vec<f64> = self.noise_levels.iter().copied().collect();
        levels.sort_by(f64::total_cmp);
        let p50 = percentile(&levels, 0.50);
        let p95 = percentile(&levels, 0.95);
        let trigger = trigger_level.unwrap_or(0.0);
        info!(
            "CAPTURE_NOISE_FLOOR reason={reason} samples={} rms_p50={p50:.5} rms_p95={p95:.5} threshold={:.5} effective_threshold={:.5} trigger={trigger:.5}",
            levels.len(),
            self.threshold,
            self.onset_threshold(),
        );
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

pub fn apply_gain(block: &mut [u8], gain: f64) {
    if (gain - 1.0).abs() < f64::EPSILON {
        return;
    }
    for sample in block.chunks_exact_mut(BYTES_PER_SAMPLE) {
        let raw = i16::from_le_bytes([sample[0], sample[1]]) as f64;
        let value = (raw * gain).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        sample.copy_from_slice(&value.to_le_bytes());
    }
}

fn rms_norm(block: &[u8]) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for sample in block.chunks_exact(BYTES_PER_SAMPLE) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as f64;
        sum += value * value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt() / 32768.0
    }
}

fn peak_norm(block: &[u8]) -> f64 {
    block
        .chunks_exact(BYTES_PER_SAMPLE)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() as f64)
        .fold(0.0, f64::max)
        / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CaptureConfig {
        CaptureConfig {
            sample_rate: 1_000,
            block_ms: 100,
            pre_roll_ms: 300,
            silence_ms: 200,
            min_speech_ms: 100,
            max_utterance_s: 5.0,
            cooldown_s: 0.0,
            threshold: 0.1,
            ..CaptureConfig::default()
        }
    }

    fn pcm_block(sample: i16) -> Vec<u8> {
        (0..100).flat_map(|_| sample.to_le_bytes()).collect()
    }

    #[test]
    fn streams_only_after_speech_start_with_pre_roll() {
        let mut collector = SpeechCollector::new(&test_config());
        assert!(collector.push(&pcm_block(100)).is_empty());
        assert!(collector.push(&pcm_block(200)).is_empty());

        let events = collector.push(&pcm_block(10_000));
        assert_eq!(events.len(), 1);
        let SpeechEvent::SpeechStart(prefix) = &events[0] else {
            panic!("expected speech start");
        };
        assert_eq!(prefix.len(), 3 * 100 * BYTES_PER_SAMPLE);

        let events = collector.push(&pcm_block(12_000));
        assert!(matches!(events.as_slice(), [SpeechEvent::SpeechChunk(_)]));

        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        let events = collector.push(&pcm_block(0));
        assert!(matches!(
            events.as_slice(),
            [SpeechEvent::SpeechChunk(_), SpeechEvent::Utterance(_)]
        ));
    }

    #[test]
    fn short_speech_is_streamed_then_rejected() {
        let mut config = test_config();
        config.min_speech_ms = 300;
        let mut collector = SpeechCollector::new(&config);

        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechStart(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_), SpeechEvent::SpeechRejected]
        ));
    }

    #[test]
    fn adaptive_noise_floor_blocks_steady_high_ambient_noise() {
        let mut config = test_config();
        config.threshold = 0.006;
        config.adaptive_noise_floor = true;
        config.noise_floor_multiplier = 2.5;
        let mut collector = SpeechCollector::new(&config);

        // 5 blocks of 0.005 ambient noise (< 0.006) -> p50 = 0.005, onset = 0.0125
        for _ in 0..5 {
            assert!(collector.push(&pcm_block(164)).is_empty());
        }
        // 6 blocks of 0.010 ambient noise (< 0.0125) -> p50 = 0.010, onset = 0.025
        for _ in 0..6 {
            assert!(collector.push(&pcm_block(328)).is_empty());
        }
        // 40 blocks of steady high ambient noise at rms ~0.020 (> 0.006 static threshold!)
        // must NOT trigger false speech, and ring window stays capped at NOISE_FLOOR_WINDOW.
        for _ in 0..40 {
            assert!(
                collector.push(&pcm_block(655)).is_empty(),
                "steady high ambient noise (~0.02) should not trigger false speech"
            );
        }
        assert_eq!(collector.noise_levels.len(), NOISE_FLOOR_WINDOW);

        // Real speech (rms ~0.153) still triggers SpeechStart.
        let events = collector.push(&pcm_block(5_000));
        assert!(matches!(events.as_slice(), [SpeechEvent::SpeechStart(_)]));
    }

    #[test]
    fn dynamic_silence_closes_short_utterances_faster_than_long_utterances() {
        let mut config = test_config();
        config.silence_ms = 1_000;
        config.dynamic_silence = true;
        config.short_utterance_silence_ms = 400;
        config.long_utterance_silence_ms = 1_400;
        config.max_utterance_s = 10.0;

        // Short utterance: 500ms voiced (< 1200ms) -> closes after 400ms (4 silence blocks)
        let mut short_collector = SpeechCollector::new(&config);
        for _ in 0..5 {
            short_collector.push(&pcm_block(10_000));
        }
        let mut short_silence_blocks = 0;
        for _ in 0..20 {
            short_silence_blocks += 1;
            let events = short_collector.push(&pcm_block(0));
            if events.iter().any(|e| matches!(e, SpeechEvent::Utterance(_))) {
                break;
            }
        }
        assert_eq!(short_silence_blocks, 4, "short utterance should close in 400ms");

        // Long utterance: 3200ms voiced (>= 3000ms) -> closes after 1400ms (14 silence blocks)
        let mut long_collector = SpeechCollector::new(&config);
        for _ in 0..32 {
            long_collector.push(&pcm_block(10_000));
        }
        let mut long_silence_blocks = 0;
        for _ in 0..20 {
            long_silence_blocks += 1;
            let events = long_collector.push(&pcm_block(0));
            if events.iter().any(|e| matches!(e, SpeechEvent::Utterance(_))) {
                break;
            }
        }
        assert_eq!(long_silence_blocks, 14, "long utterance should close in 1400ms");
        assert!(short_silence_blocks < long_silence_blocks);
    }

    #[test]
    fn flush_returns_voiced_tail_and_none_when_idle_or_too_short() {
        let mut config = test_config();
        config.min_speech_ms = 200;
        let mut collector = SpeechCollector::new(&config);

        // Idle flush -> None
        assert!(collector.flush().is_none());

        // Single 100ms voiced block (< min_speech_ms=200) -> flush returns None
        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechStart(_)]
        ));
        assert!(collector.flush().is_none());

        // 300ms voiced (>= min_speech_ms=200) -> flush returns Some(tail)
        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechStart(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        let tail = collector.flush().expect("voiced speech should flush");
        assert_eq!(tail.len(), 2 * 100 * BYTES_PER_SAMPLE);
        assert!(collector.flush().is_none(), "collector should reset after flush");
    }

    #[test]
    fn take_last_stats_captures_voiced_and_silence_tail_durations() {
        let mut config = test_config();
        config.min_speech_ms = 200;
        config.silence_ms = 300;
        let mut collector = SpeechCollector::new(&config);

        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechStart(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        let events = collector.push(&pcm_block(0));
        assert!(events.iter().any(|e| matches!(e, SpeechEvent::Utterance(_))));

        let stats = collector
            .take_last_stats()
            .expect("utterance should record VAD stats");
        assert_eq!(stats.voiced_ms, 200);
        assert_eq!(stats.silence_tail_ms, 300);
        assert_eq!(stats.duration_ms, 400);
        assert!(collector.take_last_stats().is_none());
    }
}
