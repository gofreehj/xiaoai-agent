//! Native `mipns` (`speech.usock`) frontend adapter.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::device::Device;
use crate::frontend::{FrontendEvent, FrontendTurn};
use crate::speech::{
    SocketOwnership, SpeechHandle, SpeechLiveness, SpeechService, SpeechServiceEvent, SpeechTurn,
};

const PNSHELPER_FOLLOWUP_SCRIPT: &str =
    r#"ubus -t 1 -S call pnshelper event_notify '{"src":1,"event":0}'"#;

/// Command handle for the native `mipns` frontend.
#[allow(dead_code)]
#[derive(Clone)]
pub struct MipnsHandle {
    inner: SpeechHandle,
}

#[allow(dead_code)]
impl MipnsHandle {
    pub fn new(inner: SpeechHandle) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &SpeechHandle {
        &self.inner
    }

    pub async fn continue_dialog(&self, turn_id: u64) -> anyhow::Result<()> {
        self.inner.continue_dialog(turn_id).await
    }

    pub async fn finish_dialog(&self, turn_id: u64) -> anyhow::Result<()> {
        self.inner.finish_dialog(turn_id).await
    }

    pub async fn request_followup_capture(&self, device: &Device) -> anyhow::Result<()> {
        request_followup_capture(device).await
    }
}

/// Adapter wrapping [`SpeechService`] behind the unified [`FrontendEvent`] contract.
#[allow(dead_code)]
pub struct MipnsFrontend {
    service: SpeechService,
}

#[allow(dead_code)]
impl MipnsFrontend {
    pub async fn bind(socket_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let service = SpeechService::bind(socket_path).await?;
        Ok(Self { service })
    }

    pub fn ownership(&self) -> SocketOwnership {
        self.service.ownership()
    }

    pub fn liveness(&self) -> SpeechLiveness {
        self.service.liveness()
    }

    pub fn handle(&self) -> MipnsHandle {
        MipnsHandle::new(self.service.handle())
    }

    pub fn speech_handle(&self) -> SpeechHandle {
        self.service.handle()
    }

    pub async fn recv_event(&mut self) -> Option<FrontendEvent> {
        self.service.recv().await.map(map_service_event)
    }
}

#[allow(dead_code)]
pub fn map_service_event(event: SpeechServiceEvent) -> FrontendEvent {
    match event {
        SpeechServiceEvent::Registered { vendor, codec } => {
            FrontendEvent::Registered { vendor, codec }
        }
        SpeechServiceEvent::Turn(turn) => {
            let is_wakeup = turn.activation().is_wakeup();
            let wrapped = FrontendTurn::from_mipns(turn);
            if is_wakeup {
                FrontendEvent::Wake(wrapped)
            } else {
                FrontendEvent::FollowUp(wrapped)
            }
        }
        SpeechServiceEvent::Fatal(msg) => FrontendEvent::Fatal(msg),
    }
}

/// Ask `pnshelper` via `ubus` to open the microphone for a follow-up turn.
///
/// Sleeps 100 ms first so `mipns` can finish processing `DialogFinish` and return
/// to idle before the follow-up capture event is injected.
#[allow(dead_code)]
pub async fn request_followup_capture(device: &Device) -> anyhow::Result<()> {
    sleep(Duration::from_millis(100)).await;
    let result = device
        .run_raw(PNSHELPER_FOLLOWUP_SCRIPT, Duration::from_secs(10))
        .await
        .map_err(|err| anyhow::anyhow!("failed to request follow-up capture: {err}"))?;
    if result.exit_code != 0 {
        anyhow::bail!(
            "pnshelper follow-up request failed exit={} stderr={}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    let response: serde_json::Value = serde_json::from_str(&result.stdout)
        .context("pnshelper returned an invalid follow-up response")?;
    if response.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        anyhow::bail!(
            "pnshelper rejected follow-up capture: {}",
            result.stdout.trim()
        );
    }
    info!("requested follow-up capture through pnshelper");
    Ok(())
}

/// Reject any stale follow-up turns already buffered before a new `pnshelper` request.
#[allow(dead_code)]
pub async fn drain_stale_followup_turns(
    followup_rx: &mut mpsc::Receiver<SpeechTurn>,
    speech: &SpeechHandle,
) {
    while let Ok(stale_turn) = followup_rx.try_recv() {
        warn!(
            stale_turn_id = stale_turn.id(),
            "draining stale follow-up turn queued before pnshelper request"
        );
        reject_orphaned_turn(stale_turn, speech).await;
    }
}

/// Cleanly stop capture and finish the native dialog for an orphaned `mipns` turn.
#[allow(dead_code)]
pub async fn reject_orphaned_turn(mut turn: SpeechTurn, speech: &SpeechHandle) {
    warn!(
        turn_id = turn.id(),
        activation = ?turn.activation(),
        "rejecting speech turn without an active session"
    );
    let turn_id = turn.id();
    let _ = turn.stop_capture().await;
    let _ = speech.finish_dialog(turn_id).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::DeviceConfig;
    use crate::shell::RecordingShell;
    use crate::speech::TestCommand;

    #[tokio::test]
    async fn request_followup_capture_succeeds_on_zero_code_json() {
        let shell = Arc::new(RecordingShell::answering(r#"{"code":0,"info":"ok"}"#));
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        request_followup_capture(&device)
            .await
            .expect("code=0 JSON should succeed");

        let scripts = shell.scripts();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0], PNSHELPER_FOLLOWUP_SCRIPT);
    }

    #[tokio::test]
    async fn request_followup_capture_fails_on_nonzero_exit_code() {
        let shell = Arc::new(RecordingShell::exiting_with(4, "ubus object not found"));
        let device = Device::with_runner(DeviceConfig::default(), shell);

        let err = request_followup_capture(&device)
            .await
            .expect_err("non-zero exit must fail")
            .to_string();
        assert!(err.contains("exit=4"), "unexpected error: {err}");
        assert!(
            err.contains("ubus object not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn request_followup_capture_fails_on_invalid_json() {
        let shell = Arc::new(RecordingShell::answering("not-json"));
        let device = Device::with_runner(DeviceConfig::default(), shell);

        let err = request_followup_capture(&device)
            .await
            .expect_err("malformed JSON must fail")
            .to_string();
        assert!(
            err.contains("invalid follow-up response"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn request_followup_capture_fails_on_nonzero_pnshelper_code() {
        let shell = Arc::new(RecordingShell::answering(r#"{"code":1,"info":"busy"}"#));
        let device = Device::with_runner(DeviceConfig::default(), shell);

        let err = request_followup_capture(&device)
            .await
            .expect_err("code != 0 must fail")
            .to_string();
        assert!(
            err.contains("rejected follow-up capture"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn reject_orphaned_turn_stops_capture_then_finishes_dialog() {
        let (turn, _input_tx, commands, handle) = SpeechTurn::for_test_with_handle(11);

        reject_orphaned_turn(turn, &handle).await;

        assert_eq!(
            *commands.lock().await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ]
        );
    }

    #[tokio::test]
    async fn drain_stale_followup_turns_rejects_all_queued_turns_in_order() {
        let (turn1, _in1, cmds1, handle1) = SpeechTurn::for_test_with_handle(21);
        let (turn2, _in2, cmds2, _handle2) = SpeechTurn::for_test_with_handle(22);

        let (tx, mut rx) = mpsc::channel(4);
        tx.send(turn1).await.unwrap();
        tx.send(turn2).await.unwrap();

        drain_stale_followup_turns(&mut rx, &handle1).await;

        assert!(
            rx.try_recv().is_err(),
            "all stale turns must be drained from followup_rx"
        );
        // turn1's stop_capture + finish_dialog(21) + finish_dialog(22) all go through handle1;
        // turn2's own stop_capture goes through its internal handle (cmds2).
        assert_eq!(
            *cmds1.lock().await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                },
                TestCommand::Finish {
                    continue_dialog: false
                },
            ]
        );
        assert_eq!(*cmds2.lock().await, vec![TestCommand::StopCapture]);
    }

    #[tokio::test]
    async fn map_service_event_translates_registered_wake_and_fatal() {
        let reg = map_service_event(SpeechServiceEvent::Registered {
            vendor: "mipns".into(),
            codec: Some("pcm".into()),
        });
        assert!(matches!(
            reg,
            FrontendEvent::Registered { ref vendor, ref codec }
                if vendor == "mipns" && codec.as_deref() == Some("pcm")
        ));

        let (turn, _in, _cmds) = SpeechTurn::for_test(99);
        let wake = map_service_event(SpeechServiceEvent::Turn(turn));
        assert!(matches!(wake, FrontendEvent::Wake(ref t) if t.id() == 99));

        let fatal = map_service_event(SpeechServiceEvent::Fatal("socket died".into()));
        assert!(matches!(fatal, FrontendEvent::Fatal(ref s) if s == "socket died"));
    }
}
