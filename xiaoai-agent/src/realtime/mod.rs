//! Unified real-time voice dialogue driver and full-duplex AEC abstractions.
//!
//! Decouples the shared session runner from specific cloud providers (Volcengine O2/v3,
//! OpenAI Realtime) and optional echo cancellation backends.

pub mod daily_usage;
pub mod runner;
pub mod smart_turn;
pub mod volcengine_frame;
pub mod volcengine_o2;

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;

/// Token usage counters reported at the end of a real-time response turn.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RealtimeUsage {
    pub input_text_tokens: u64,
    pub input_audio_tokens: u64,
    pub output_text_tokens: u64,
    pub output_audio_tokens: u64,
    pub cached_text_tokens: u64,
    pub cached_audio_tokens: u64,
}

#[allow(dead_code)]
impl RealtimeUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_text_tokens
            .saturating_add(self.input_audio_tokens)
            .saturating_add(self.output_text_tokens)
            .saturating_add(self.output_audio_tokens)
            .saturating_add(self.cached_text_tokens)
            .saturating_add(self.cached_audio_tokens)
    }
}

/// Terminal reason why a real-time dialogue session ended.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeExit {
    UserExit,
    IdleTimeout,
    NativeYield,
    TransportClosed,
    Error(String),
}

/// Provider-agnostic events emitted by a [`RealtimeDriver`].
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    SessionReady {
        session_id: String,
    },
    UserSpeechStarted,
    UserTranscriptDelta(String),
    UserTranscriptCompleted(String),
    AssistantTextDelta(String),
    AssistantAudioDelta(Bytes),
    AssistantAudioDone,
    ToolCallRequested {
        call_id: String,
        name: String,
        arguments: String,
    },
    ResponseDone {
        usage: RealtimeUsage,
    },
    Error(String),
    Closed,
}

/// Boxed `Send` future alias used by [`RealtimeDriver`] to avoid external proc-macro crates.
#[allow(dead_code)]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Cloud-provider-agnostic full/half-duplex real-time voice transport contract.
#[allow(dead_code)]
pub trait RealtimeDriver: Send {
    /// Expected downlink PCM output sample rate in Hz (e.g. `24_000`).
    fn output_sample_rate(&self) -> u32;

    /// Preferred uplink 16 kHz mono S16_LE PCM chunk size in bytes (e.g. `640` = 20 ms).
    fn input_chunk_bytes(&self) -> usize;

    /// Send one uplink 16 kHz mono S16_LE PCM frame.
    fn send_audio<'a>(&'a mut self, pcm: &'a [u8]) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Signal end-of-utterance / commit user turn (used by Smart Turn or manual end-of-ASR).
    fn commit_user_turn<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Cancel any in-flight assistant response (and fence stale downlink audio).
    fn cancel_response<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Return a tool execution result to the provider (used by tool-capable drivers).
    fn submit_tool_output<'a>(
        &'a mut self,
        call_id: &'a str,
        output: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Receive the next normalized [`RealtimeEvent`], or `Ok(None)` on clean EOF.
    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<Option<RealtimeEvent>>>;

    /// Gracefully finish the remote session and close the underlying transport.
    fn close<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Reserved full-duplex echo canceller hook (for future `subband_aec` integration).
#[allow(dead_code)]
pub trait EchoCanceller: Send {
    fn sample_rate(&self) -> u32;
    fn feed_render(&mut self, render_pcm: &[i16]);
    fn process_capture(&mut self, capture_pcm: &mut [i16]);
    fn reset(&mut self);
}
