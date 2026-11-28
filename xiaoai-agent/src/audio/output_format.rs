//! Downlink audio format probe (`S16_LE` vs `F32_LE`) and `OggS` safety guard.
//!
//! 1:1 port of `xiaoai-doubao/xiaoai-plus/src/audio/output_format.hpp` plus
//! hard rejection of `OggS` container pages so compressed audio is never fed
//! directly into raw `S16_LE` `aplay`.

use anyhow::{bail, ensure};
use bytes::Bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormatKind {
    #[default]
    Unknown,
    S16,
    F32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputFormatProbeResult {
    pub bytes: usize,
    pub divisible_by_four: bool,
    pub s16_rms: f64,
    pub s16_clipped_ratio: f64,
    pub f32_in_range_ratio: f64,
    pub f32_rms: f64,
    pub selected: OutputFormatKind,
}

#[derive(Debug, Clone, Default)]
pub struct OutputFormatProbe {
    kind: OutputFormatKind,
}

#[allow(dead_code)]
impl OutputFormatProbe {
    pub fn new() -> Self {
        Self {
            kind: OutputFormatKind::Unknown,
        }
    }

    pub fn kind(&self) -> OutputFormatKind {
        self.kind
    }

    pub fn reset(&mut self) {
        self.kind = OutputFormatKind::Unknown;
    }

    /// Reject `OggS` page headers and inspect raw bytes to lock onto `S16` or `F32`.
    pub fn inspect(&mut self, raw: &[u8]) -> anyhow::Result<OutputFormatProbeResult> {
        if raw.starts_with(b"OggS") {
            bail!("rejected OggS compressed audio stream in raw PCM output path");
        }

        let mut probe = OutputFormatProbeResult {
            bytes: raw.len(),
            divisible_by_four: raw.len() % 4 == 0,
            s16_rms: 0.0,
            s16_clipped_ratio: 0.0,
            f32_in_range_ratio: 0.0,
            f32_rms: 0.0,
            selected: self.kind,
        };

        let mut s16_sum = 0.0_f64;
        let mut clipped = 0usize;
        let s16_count = raw.len() / 2;
        for chunk in raw.chunks_exact(2) {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            let normalized = (sample as f64) / 32768.0;
            s16_sum += normalized * normalized;
            if sample <= -32760 || sample >= 32760 {
                clipped += 1;
            }
        }
        if s16_count > 0 {
            probe.s16_rms = (s16_sum / (s16_count as f64)).sqrt();
            probe.s16_clipped_ratio = (clipped as f64) / (s16_count as f64);
        }

        let mut in_range = 0usize;
        let mut meaningful = 0usize;
        let mut f32_sum = 0.0_f64;
        let f32_count = raw.len() / 4;
        for chunk in raw.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if sample.is_finite() && (-1.0..=1.0).contains(&sample) {
                in_range += 1;
                f32_sum += (sample as f64) * (sample as f64);
                if sample.abs() >= 0.0001 {
                    meaningful += 1;
                }
            }
        }
        if f32_count > 0 {
            probe.f32_in_range_ratio = (in_range as f64) / (f32_count as f64);
            probe.f32_rms = (f32_sum / (f32_count as f64)).sqrt();
        }

        if self.kind == OutputFormatKind::Unknown && raw.len() >= 64 {
            if probe.divisible_by_four
                && probe.f32_in_range_ratio >= 0.98
                && meaningful >= f32_count / 10
                && probe.f32_rms >= 0.0001
            {
                self.kind = OutputFormatKind::F32;
            } else if !probe.divisible_by_four
                || (probe.f32_in_range_ratio < 0.95 && probe.s16_rms >= 0.001)
            {
                self.kind = OutputFormatKind::S16;
            }
        }
        probe.selected = self.kind;
        Ok(probe)
    }

    /// Convert `raw` to `S16_LE` bytes according to the currently detected format.
    pub fn to_s16(&self, raw: &[u8]) -> anyhow::Result<Bytes> {
        if raw.starts_with(b"OggS") {
            bail!("rejected OggS compressed audio stream in raw PCM output path");
        }
        if self.kind != OutputFormatKind::F32 {
            return Ok(Bytes::copy_from_slice(raw));
        }
        ensure!(raw.len() % 4 == 0, "f32 output is not sample aligned");
        let mut out = Vec::with_capacity(raw.len() / 2);
        for chunk in raw.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            ensure!(sample.is_finite(), "non-finite f32 output sample");
            let clamped = (sample as f64).clamp(-1.0, 1.0);
            let scaled = (clamped * 32767.0).round() as i32;
            let value = scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            out.extend_from_slice(&value.to_le_bytes());
        }
        Ok(Bytes::from(out))
    }

    /// Inspect `raw` (updating format detection if still unknown) and convert to `S16_LE`.
    pub fn inspect_and_convert(&mut self, raw: &[u8]) -> anyhow::Result<Bytes> {
        self.inspect(raw)?;
        self.to_s16(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_s16_sine(samples: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        for i in 0..samples {
            let v = ((i as f64 * 0.15).sin() * 12000.0) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    fn make_f32_sine(samples: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 4);
        for i in 0..samples {
            let v = ((i as f64 * 0.15).sin() * 0.5) as f32;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    #[test]
    fn detects_s16_le_and_passes_through_bytes() {
        let mut probe = OutputFormatProbe::new();
        let raw = make_s16_sine(128);
        let res = probe.inspect(&raw).unwrap();
        assert_eq!(res.selected, OutputFormatKind::S16);
        assert_eq!(probe.kind(), OutputFormatKind::S16);
        let converted = probe.to_s16(&raw).unwrap();
        assert_eq!(converted.as_ref(), raw.as_slice());
    }

    #[test]
    fn detects_f32_le_and_converts_to_s16_le() {
        let mut probe = OutputFormatProbe::new();
        let raw = make_f32_sine(128);
        let converted = probe.inspect_and_convert(&raw).unwrap();
        assert_eq!(probe.kind(), OutputFormatKind::F32);
        assert_eq!(converted.len(), 128 * 2);

        // Verify peak sample matches ~0.5 * 32767 = ~16384
        let peak = converted
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).abs())
            .max()
            .unwrap_or(0);
        assert!((15000..=17000).contains(&peak), "unexpected peak {peak}");
    }

    #[test]
    fn rejects_oggs_compressed_stream_header() {
        let mut probe = OutputFormatProbe::new();
        let mut ogg = b"OggS".to_vec();
        ogg.extend_from_slice(&[0u8; 128]);
        let err = probe.inspect(&ogg).unwrap_err();
        assert!(err.to_string().contains("OggS"));
        assert!(probe.to_s16(&ogg).is_err());
    }

    #[test]
    fn rejects_non_finite_f32_samples() {
        let mut probe = OutputFormatProbe::new();
        let valid = make_f32_sine(64);
        probe.inspect(&valid).unwrap();
        assert_eq!(probe.kind(), OutputFormatKind::F32);

        let bad = f32::NAN.to_le_bytes();
        assert!(probe.to_s16(&bad).is_err());
    }
}
