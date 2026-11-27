//! Speech frontend abstractions shared across native `mipns` and dual-KWS frontends.

pub mod external_kws;
pub mod mipns;

use std::fmt;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};

use crate::speech::{ActivationMode, SpeechInput, SpeechTurn};

#[allow(dead_code)]
#[derive(Debug)]
pub enum AudioStreamChunk {
    Pcm(Bytes),
    SysActive(bool),
    End,
    Cancelled,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeInSignal {
    Wake,
    ExitCmd,
}

#[allow(dead_code)]
enum FrontendTurnInner {
    Mipns(SpeechTurn),
    Stream {
        id: u64,
        activation: ActivationMode,
        interaction_mode: u64,
        chunk_rx: mpsc::Receiver<AudioStreamChunk>,
        stop_tx: Option<oneshot::Sender<()>>,
        stop_requested: bool,
    },
}

/// Frontend-agnostic active voice turn handle consumed by the session orchestrator.
#[allow(dead_code)]
pub struct FrontendTurn {
    inner: FrontendTurnInner,
}

#[allow(dead_code)]
impl FrontendTurn {
    pub fn from_mipns(turn: SpeechTurn) -> Self {
        Self {
            inner: FrontendTurnInner::Mipns(turn),
        }
    }

    pub fn from_stream(
        id: u64,
        activation: ActivationMode,
        interaction_mode: u64,
        chunk_rx: mpsc::Receiver<AudioStreamChunk>,
        stop_tx: Option<oneshot::Sender<()>>,
    ) -> Self {
        Self {
            inner: FrontendTurnInner::Stream {
                id,
                activation,
                interaction_mode,
                chunk_rx,
                stop_tx,
                stop_requested: false,
            },
        }
    }

    pub fn id(&self) -> u64 {
        match &self.inner {
            FrontendTurnInner::Mipns(turn) => turn.id(),
            FrontendTurnInner::Stream { id, .. } => *id,
        }
    }

    pub fn activation(&self) -> ActivationMode {
        match &self.inner {
            FrontendTurnInner::Mipns(turn) => turn.activation(),
            FrontendTurnInner::Stream { activation, .. } => *activation,
        }
    }

    pub fn interaction_mode(&self) -> u64 {
        match &self.inner {
            FrontendTurnInner::Mipns(turn) => turn.interaction_mode(),
            FrontendTurnInner::Stream {
                interaction_mode, ..
            } => *interaction_mode,
        }
    }

    pub async fn recv(&mut self) -> Option<AudioStreamChunk> {
        match &mut self.inner {
            FrontendTurnInner::Mipns(turn) => turn.recv().await.map(|input| match input {
                SpeechInput::Pcm(bytes) => AudioStreamChunk::Pcm(bytes),
                SpeechInput::End => AudioStreamChunk::End,
                SpeechInput::Cancelled => AudioStreamChunk::Cancelled,
            }),
            FrontendTurnInner::Stream { chunk_rx, .. } => chunk_rx.recv().await,
        }
    }

    pub async fn stop_capture(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            FrontendTurnInner::Mipns(turn) => turn.stop_capture().await,
            FrontendTurnInner::Stream {
                stop_tx,
                stop_requested,
                ..
            } => {
                if *stop_requested {
                    return Ok(());
                }
                *stop_requested = true;
                if let Some(tx) = stop_tx.take() {
                    let _ = tx.send(());
                }
                Ok(())
            }
        }
    }

    pub fn as_speech_turn_mut(&mut self) -> Option<&mut SpeechTurn> {
        match &mut self.inner {
            FrontendTurnInner::Mipns(turn) => Some(turn),
            FrontendTurnInner::Stream { .. } => None,
        }
    }

    pub fn into_speech_turn(self) -> Option<SpeechTurn> {
        match self.inner {
            FrontendTurnInner::Mipns(turn) => Some(turn),
            FrontendTurnInner::Stream { .. } => None,
        }
    }
}

impl fmt::Debug for FrontendTurn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrontendTurn")
            .field("id", &self.id())
            .field("activation", &self.activation())
            .field("interaction_mode", &self.interaction_mode())
            .finish()
    }
}

#[allow(dead_code)]
pub enum FrontendEvent {
    Registered {
        vendor: String,
        codec: Option<String>,
    },
    Wake(FrontendTurn),
    FollowUp(FrontendTurn),
    ExitCmd,
    Fatal(String),
}

impl fmt::Debug for FrontendEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registered { vendor, codec } => f
                .debug_struct("Registered")
                .field("vendor", vendor)
                .field("codec", codec)
                .finish(),
            Self::Wake(turn) => f
                .debug_struct("Wake")
                .field("id", &turn.id())
                .field("activation", &turn.activation())
                .finish(),
            Self::FollowUp(turn) => f
                .debug_struct("FollowUp")
                .field("id", &turn.id())
                .field("activation", &turn.activation())
                .finish(),
            Self::ExitCmd => write!(f, "ExitCmd"),
            Self::Fatal(msg) => f.debug_tuple("Fatal").field(msg).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frontend_turn_from_stream_forwards_chunks_and_signals_stop_once() {
        let (chunk_tx, chunk_rx) = mpsc::channel(4);
        let (stop_tx, stop_rx) = oneshot::channel();
        let mut turn = FrontendTurn::from_stream(7, ActivationMode::FollowUp, 2, chunk_rx, Some(stop_tx));

        assert_eq!(turn.id(), 7);
        assert_eq!(turn.activation(), ActivationMode::FollowUp);
        assert_eq!(turn.interaction_mode(), 2);
        assert!(turn.as_speech_turn_mut().is_none());

        chunk_tx
            .send(AudioStreamChunk::SysActive(true))
            .await
            .unwrap();
        chunk_tx
            .send(AudioStreamChunk::Pcm(Bytes::from_static(b"\x01\x02")))
            .await
            .unwrap();
        chunk_tx.send(AudioStreamChunk::End).await.unwrap();

        assert!(matches!(
            turn.recv().await,
            Some(AudioStreamChunk::SysActive(true))
        ));
        assert!(matches!(
            turn.recv().await,
            Some(AudioStreamChunk::Pcm(ref b)) if b.as_ref() == b"\x01\x02"
        ));
        assert!(matches!(turn.recv().await, Some(AudioStreamChunk::End)));

        turn.stop_capture().await.unwrap();
        stop_rx.await.expect("stop_tx should fire on first stop_capture");
        // Calling stop_capture a second time must be an idempotent no-op.
        turn.stop_capture().await.unwrap();
        assert!(turn.into_speech_turn().is_none());
    }

    #[tokio::test]
    async fn frontend_turn_from_mipns_maps_inputs_and_stops_capture() {
        let (speech_turn, input_tx, commands) = SpeechTurn::for_test(42);
        let mut turn = FrontendTurn::from_mipns(speech_turn);

        assert_eq!(turn.id(), 42);
        assert_eq!(turn.activation(), ActivationMode::Wakeup);
        assert!(turn.as_speech_turn_mut().is_some());

        input_tx
            .send(SpeechInput::Pcm(Bytes::from_static(b"\x10\x20")))
            .await
            .unwrap();
        input_tx.send(SpeechInput::Cancelled).await.unwrap();

        assert!(matches!(
            turn.recv().await,
            Some(AudioStreamChunk::Pcm(ref b)) if b.as_ref() == b"\x10\x20"
        ));
        assert!(matches!(turn.recv().await, Some(AudioStreamChunk::Cancelled)));

        turn.stop_capture().await.unwrap();
        assert_eq!(
            *commands.lock().await,
            vec![crate::speech::TestCommand::StopCapture]
        );
    }
}
