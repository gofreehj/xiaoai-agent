//! Volcengine Doubao O2/v3 real-time voice dialogue driver.
//!
//! 1:1 Rust port of `xiaoai-doubao/xiaoai-plus/src/realtime/o2_protocol.cpp` with:
//! - Official dual auth (`X-Api-Key` or `X-Api-App-Id` + `X-Api-Access-Key` +
//!   `X-Api-Resource-Id` + `X-Api-App-Key` + UUID `X-Api-Connect-Id`)
//! - Built-in `enable_volc_websearch` parameters inside `StartSession (100)`
//! - Local `canceled_question_id` cancellation fence to drop stale downlink audio/text
//!   after barge-in until a fresh `question_id` arrives

use std::collections::VecDeque;

use anyhow::{bail, ensure, Context};
use futures::{SinkExt, StreamExt};
use rand::RngCore;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use crate::config::VolcengineO2Config;
use crate::realtime::volcengine_frame::{
    build_audio_frame, build_json_frame, decode_frame, event, MessageType, VolcFrame,
};
use crate::realtime::{BoxFuture, RealtimeDriver, RealtimeEvent, RealtimeUsage};

pub const VOLC_OUTPUT_SAMPLE_RATE: u32 = 24_000;
/// 20 ms of 16 kHz mono S16_LE PCM.
pub const VOLC_INPUT_CHUNK_BYTES: usize = 640;

/// Generate an RFC 4122 UUID v4 string without requiring an external `uuid` crate.
pub fn generate_uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3],
        b[4], b[5],
        b[6], b[7],
        b[8], b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Build official Volcengine Doubao O2/v3 WebSocket handshake headers.
///
/// Auth precedence:
/// 1. `api_key` non-empty -> `X-Api-Key`
/// 2. `app_id` + `access_key` non-empty -> `X-Api-App-Id` + `X-Api-Access-Key`
/// Always includes `X-Api-Resource-Id`, `X-Api-App-Key`, and `X-Api-Connect-Id`.
pub fn build_volcengine_headers(
    cfg: &VolcengineO2Config,
    connect_id: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    ensure!(
        !connect_id.trim().is_empty(),
        "X-Api-Connect-Id must not be empty"
    );

    let mut headers = Vec::with_capacity(6);
    let api_key = cfg.api_key.trim();
    let app_id = cfg.app_id.trim();
    let access_key = cfg.access_key.trim();

    if !api_key.is_empty() {
        headers.push(("X-Api-Key".to_string(), api_key.to_string()));
    } else if !app_id.is_empty() && !access_key.is_empty() {
        headers.push(("X-Api-App-Id".to_string(), app_id.to_string()));
        headers.push(("X-Api-Access-Key".to_string(), access_key.to_string()));
    } else {
        bail!(
            "Volcengine O2 credentials missing: configure realtime.volcengine_o2.api_key or both app_id and access_key"
        );
    }

    let resource_id = if cfg.resource_id.trim().is_empty() {
        "volc.speech.dialog"
    } else {
        cfg.resource_id.trim()
    };
    let app_key = if cfg.app_key.trim().is_empty() {
        "PlgvMymc7f3tQnJ6"
    } else {
        cfg.app_key.trim()
    };

    headers.push(("X-Api-Resource-Id".to_string(), resource_id.to_string()));
    headers.push(("X-Api-App-Key".to_string(), app_key.to_string()));
    headers.push(("X-Api-Connect-Id".to_string(), connect_id.trim().to_string()));
    Ok(headers)
}

/// Build the `StartSession (100)` JSON payload including optional `enable_volc_websearch`.
pub fn build_start_session_payload(cfg: &VolcengineO2Config, instructions: &str) -> Value {
    let mut dialog_extra = serde_json::Map::new();
    let model = if cfg.model.trim().is_empty() {
        "1.2.1.1"
    } else {
        cfg.model.trim()
    };
    dialog_extra.insert("model".to_string(), json!(model));
    dialog_extra.insert("input_mod".to_string(), json!("keep_alive"));
    dialog_extra.insert(
        "enable_volc_websearch".to_string(),
        json!(cfg.enable_volc_websearch),
    );
    if cfg.enable_volc_websearch {
        let ws_type = if cfg.volc_websearch_type.trim().is_empty() {
            "web_summary"
        } else {
            cfg.volc_websearch_type.trim()
        };
        dialog_extra.insert("volc_websearch_type".to_string(), json!(ws_type));
        let count = cfg.volc_websearch_result_count.clamp(1, 10);
        dialog_extra.insert("volc_websearch_result_count".to_string(), json!(count));
        if !cfg.volc_websearch_api_key.trim().is_empty() {
            dialog_extra.insert(
                "volc_websearch_api_key".to_string(),
                json!(cfg.volc_websearch_api_key.trim()),
            );
        }
        if !cfg.volc_websearch_no_result_message.trim().is_empty() {
            dialog_extra.insert(
                "volc_websearch_no_result_message".to_string(),
                json!(cfg.volc_websearch_no_result_message.trim()),
            );
        }
    }

    let mut dialog = serde_json::Map::new();
    if !cfg.bot_name.trim().is_empty() {
        dialog.insert("bot_name".to_string(), json!(cfg.bot_name.trim()));
    }
    if !instructions.trim().is_empty() {
        dialog.insert("system_role".to_string(), json!(instructions.trim()));
    }
    if !cfg.speaking_style.trim().is_empty() {
        dialog.insert(
            "speaking_style".to_string(),
            json!(cfg.speaking_style.trim()),
        );
    }
    dialog.insert("extra".to_string(), Value::Object(dialog_extra));

    let speaker = if cfg.voice.trim().is_empty() {
        "zh_female_vv_jupiter_bigtts"
    } else {
        cfg.voice.trim()
    };

    json!({
        "asr": {
            "extra": {
                "end_smooth_window_ms": cfg.end_smooth_window_ms
            }
        },
        "tts": {
            "speaker": speaker,
            "audio_config": {
                "channel": 1,
                "format": "pcm_s16le",
                "sample_rate": VOLC_OUTPUT_SAMPLE_RATE
            }
        },
        "dialog": Value::Object(dialog)
    })
}

/// Pure O2/v3 protocol state machine (unit-testable without network I/O).
#[derive(Debug, Clone)]
pub struct O2ProtocolState {
    pub session_id: String,
    pub current_question_id: Option<String>,
    pub canceled_question_id: Option<String>,
    pub canceled: bool,
    pub output_allowed: bool,
    pub tts_active: bool,
    pub pending_asr_text: String,
    pub saw_connection_started: bool,
    pub saw_session_started: bool,
}

impl O2ProtocolState {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            current_question_id: None,
            canceled_question_id: None,
            canceled: false,
            output_allowed: true,
            tts_active: false,
            pending_asr_text: String::new(),
            saw_connection_started: false,
            saw_session_started: false,
        }
    }

    /// Locally fence any in-flight assistant response until a fresh `question_id`
    /// from ASR is paired with its matching reply header (`350`/`550`/`559`/`359`).
    pub fn cancel_response(&mut self) {
        self.canceled = true;
        self.canceled_question_id = self.current_question_id.clone();
        self.output_allowed = false;
        self.tts_active = false;
    }

    fn check_output_fence(&mut self, json: Option<&Value>) -> bool {
        let q = json.and_then(extract_question_id);
        if self.canceled {
            match q {
                Some(qid) => {
                    let allowed = self.current_question_id.as_deref() == Some(qid)
                        && self.canceled_question_id.as_deref() != Some(qid);
                    self.output_allowed = allowed;
                    return !allowed;
                }
                None => {
                    return !self.output_allowed;
                }
            }
        }
        if !self.output_allowed {
            return true;
        }
        if let (Some(canceled), Some(qid)) = (self.canceled_question_id.as_deref(), q) {
            if canceled == qid {
                return true;
            }
        }
        false
    }

    /// Translate one decoded [`VolcFrame`] into zero or more normalized [`RealtimeEvent`]s.
    pub fn handle_frame(&mut self, frame: VolcFrame) -> Vec<RealtimeEvent> {
        if frame.message_type == MessageType::ErrorInfo {
            let code = frame.error_code.unwrap_or(0);
            let msg = parse_error_message(&frame.payload);
            return vec![RealtimeEvent::Error(format!("volcengine error {code}: {msg}"))];
        }

        let Some(ev) = frame.event else {
            return Vec::new();
        };

        match ev {
            event::CONNECTION_STARTED => {
                self.saw_connection_started = true;
                Vec::new()
            }
            event::CONNECTION_FAILED => {
                let msg = parse_error_message(&frame.payload);
                vec![RealtimeEvent::Error(format!("connection failed: {msg}"))]
            }
            event::CONNECTION_FINISHED | event::SESSION_FINISHED => {
                vec![RealtimeEvent::Closed]
            }
            event::SESSION_STARTED => {
                self.saw_session_started = true;
                let sid = frame
                    .session_id
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| self.session_id.clone());
                self.session_id = sid.clone();
                vec![RealtimeEvent::SessionReady { session_id: sid }]
            }
            event::SESSION_FAILED | event::DIALOG_COMMON_ERROR => {
                let msg = parse_error_message(&frame.payload);
                vec![RealtimeEvent::Error(format!("session error ({ev}): {msg}"))]
            }
            event::ASR_INFO => {
                if let Some(json) = parse_json(&frame.payload) {
                    if let Some(qid) = extract_question_id(&json) {
                        self.current_question_id = Some(qid.to_string());
                    }
                }
                self.pending_asr_text.clear();
                vec![RealtimeEvent::UserSpeechStarted]
            }
            event::ASR_RESPONSE => {
                let Some(json) = parse_json(&frame.payload) else {
                    return Vec::new();
                };
                if let Some(qid) = extract_question_id(&json) {
                    self.current_question_id = Some(qid.to_string());
                }
                let (text, _is_interim) = extract_asr_text(&json);
                if text.is_empty() {
                    return Vec::new();
                }
                self.pending_asr_text = text.clone();
                vec![RealtimeEvent::UserTranscriptDelta(text)]
            }
            event::ASR_ENDED => {
                let mut text = std::mem::take(&mut self.pending_asr_text);
                if text.is_empty() {
                    if let Some(json) = parse_json(&frame.payload) {
                        let (fallback_text, _) = extract_asr_text(&json);
                        text = fallback_text;
                    }
                }
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![RealtimeEvent::UserTranscriptCompleted(text)]
                }
            }
            event::TTS_SENTENCE_START => {
                let json = parse_json(&frame.payload);
                if self.check_output_fence(json.as_ref()) {
                    debug!("fenced stale TTS_SENTENCE_START");
                    return Vec::new();
                }
                self.tts_active = true;
                Vec::new()
            }
            event::TTS_SENTENCE_END => Vec::new(),
            event::CHAT_RESPONSE => {
                let Some(json) = parse_json(&frame.payload) else {
                    return Vec::new();
                };
                if self.check_output_fence(Some(&json)) {
                    debug!("fenced stale CHAT_RESPONSE");
                    return Vec::new();
                }
                let content = json
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| json.get("text").and_then(Value::as_str))
                    .unwrap_or_default();
                if content.is_empty() {
                    Vec::new()
                } else {
                    vec![RealtimeEvent::AssistantTextDelta(content.to_string())]
                }
            }
            event::TTS_RESPONSE => {
                if !self.output_allowed {
                    debug!(
                        bytes = frame.payload.len(),
                        "fenced stale TTS_RESPONSE audio chunk"
                    );
                    return Vec::new();
                }
                if frame.payload.is_empty() {
                    Vec::new()
                } else {
                    self.tts_active = true;
                    vec![RealtimeEvent::AssistantAudioDelta(frame.payload)]
                }
            }
            event::TTS_ENDED => {
                let json = parse_json(&frame.payload);
                if self.check_output_fence(json.as_ref()) {
                    debug!("fenced stale TTS_ENDED");
                    return Vec::new();
                }
                self.tts_active = false;
                vec![RealtimeEvent::AssistantAudioDone]
            }
            event::CHAT_ENDED => {
                let json = parse_json(&frame.payload);
                let _ = self.check_output_fence(json.as_ref());
                Vec::new()
            }
            event::USAGE_RESPONSE => {
                let usage = parse_json(&frame.payload)
                    .map(|j| parse_usage(&j))
                    .unwrap_or_default();
                vec![RealtimeEvent::ResponseDone { usage }]
            }
            other => {
                debug!(event = other, "ignoring unhandled Volcengine O2 event");
                Vec::new()
            }
        }
    }
}

fn parse_json(payload: &[u8]) -> Option<Value> {
    if payload.is_empty() {
        return None;
    }
    serde_json::from_slice(payload).ok()
}

fn parse_error_message(payload: &[u8]) -> String {
    if let Some(json) = parse_json(payload) {
        if let Some(msg) = json
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| json.get("message").and_then(Value::as_str))
            .or_else(|| json.get("msg").and_then(Value::as_str))
        {
            return msg.chars().take(200).collect();
        }
    }
    let raw = String::from_utf8_lossy(payload);
    raw.trim().chars().take(200).collect()
}

fn extract_question_id(json: &Value) -> Option<&str> {
    json.get("question_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn extract_asr_text(json: &Value) -> (String, bool) {
    if let Some(first) = json
        .get("results")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    {
        let text = first
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let is_interim = first
            .get("is_interim")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return (text, is_interim);
    }
    let text = json
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    (text, false)
}

fn parse_usage(json: &Value) -> RealtimeUsage {
    let src = json.get("usage").unwrap_or(json);
    let get_u64 = |key: &str| src.get(key).and_then(Value::as_u64).unwrap_or(0);
    RealtimeUsage {
        input_text_tokens: get_u64("input_text_tokens"),
        input_audio_tokens: get_u64("input_audio_tokens"),
        output_text_tokens: get_u64("output_text_tokens"),
        output_audio_tokens: get_u64("output_audio_tokens"),
        cached_text_tokens: get_u64("cached_text_tokens"),
        cached_audio_tokens: get_u64("cached_audio_tokens"),
    }
}

/// Live WebSocket driver for Volcengine Doubao O2/v3 real-time voice dialogue.
#[allow(dead_code)]
pub struct VolcengineO2Driver {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    state: O2ProtocolState,
    pending_events: VecDeque<RealtimeEvent>,
    uplink_buf: Vec<u8>,
    closed: bool,
}

#[allow(dead_code)]
impl VolcengineO2Driver {
    /// Connect to Volcengine O2/v3 WebSocket and complete `StartConnection -> 50 -> StartSession -> 150`.
    pub async fn connect(cfg: &VolcengineO2Config, system_prompt: &str) -> anyhow::Result<Self> {
        let ws_url = cfg.ws_url.trim();
        ensure!(!ws_url.is_empty(), "realtime.volcengine_o2.ws_url is empty");

        let connect_id = generate_uuid_v4();
        let session_id = generate_uuid_v4();
        let auth_headers = build_volcengine_headers(cfg, &connect_id)?;

        let mut request = ws_url
            .into_client_request()
            .context("invalid Volcengine O2 WebSocket URL")?;
        for (name, value) in auth_headers {
            let hdr_name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid header name {name}"))?;
            let hdr_val = HeaderValue::from_str(&value)
                .with_context(|| format!("invalid header value for {name}"))?;
            request.headers_mut().insert(hdr_name, hdr_val);
        }

        let (ws, _resp) = connect_async(request)
            .await
            .map_err(|e| anyhow::anyhow!("Volcengine O2 WebSocket handshake failed: {e}"))?;

        let mut driver = Self {
            ws,
            state: O2ProtocolState::new(session_id.clone()),
            pending_events: VecDeque::new(),
            uplink_buf: Vec::with_capacity(VOLC_INPUT_CHUNK_BYTES * 2),
            closed: false,
        };

        // 1) StartConnection (1) -> expect ConnectionStarted (50)
        let start_conn = build_json_frame(event::START_CONNECTION, None, &json!({}))?;
        driver
            .ws
            .send(Message::Binary(start_conn.into()))
            .await
            .context("failed to send StartConnection")?;

        while !driver.state.saw_connection_started {
            let Some(ev) = driver.read_one_ws_event().await? else {
                bail!("Volcengine O2 closed before ConnectionStarted (50)");
            };
            if let RealtimeEvent::Error(msg) = ev {
                bail!("Volcengine O2 StartConnection error: {msg}");
            }
            driver.pending_events.push_back(ev);
        }

        // 2) StartSession (100) -> expect SessionStarted (150)
        let start_payload = build_start_session_payload(cfg, system_prompt);
        let start_sess = build_json_frame(
            event::START_SESSION,
            Some(&driver.state.session_id),
            &start_payload,
        )?;
        driver
            .ws
            .send(Message::Binary(start_sess.into()))
            .await
            .context("failed to send StartSession")?;

        while !driver.state.saw_session_started {
            let Some(ev) = driver.read_one_ws_event().await? else {
                bail!("Volcengine O2 closed before SessionStarted (150)");
            };
            if let RealtimeEvent::Error(msg) = ev {
                bail!("Volcengine O2 StartSession error: {msg}");
            }
            driver.pending_events.push_back(ev);
        }

        Ok(driver)
    }

    /// Send an optional `SayHello (300)` greeting prompt.
    pub async fn send_say_hello(&mut self, content: &str) -> anyhow::Result<()> {
        let frame = build_json_frame(
            event::SAY_HELLO,
            Some(&self.state.session_id),
            &json!({ "content": content }),
        )?;
        self.ws
            .send(Message::Binary(frame.into()))
            .await
            .context("failed to send SayHello")?;
        Ok(())
    }

    async fn read_one_ws_event(&mut self) -> anyhow::Result<Option<RealtimeEvent>> {
        if let Some(ev) = self.pending_events.pop_front() {
            return Ok(Some(ev));
        }
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Binary(bin))) => {
                    let frame = decode_frame(&bin)?;
                    let events = self.state.handle_frame(frame);
                    for ev in events {
                        self.pending_events.push_back(ev);
                    }
                    if let Some(ev) = self.pending_events.pop_front() {
                        return Ok(Some(ev));
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = self.ws.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Text(txt))) => {
                    debug!(len = txt.len(), "ignoring unexpected text WS message");
                }
                Some(Ok(Message::Close(_))) | None => {
                    self.closed = true;
                    return Ok(None);
                }
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(err)) => {
                    self.closed = true;
                    return Err(anyhow::anyhow!("Volcengine O2 WS read error: {err}"));
                }
            }
        }
    }
}

impl RealtimeDriver for VolcengineO2Driver {
    fn output_sample_rate(&self) -> u32 {
        VOLC_OUTPUT_SAMPLE_RATE
    }

    fn input_chunk_bytes(&self) -> usize {
        VOLC_INPUT_CHUNK_BYTES
    }

    fn send_audio<'a>(&'a mut self, pcm: &'a [u8]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if self.closed || pcm.is_empty() {
                return Ok(());
            }
            self.uplink_buf.extend_from_slice(pcm);
            while self.uplink_buf.len() >= VOLC_INPUT_CHUNK_BYTES {
                let chunk: Vec<u8> = self.uplink_buf.drain(..VOLC_INPUT_CHUNK_BYTES).collect();
                let frame =
                    build_audio_frame(event::TASK_REQUEST, &self.state.session_id, &chunk)?;
                self.ws
                    .send(Message::Binary(frame.into()))
                    .await
                    .context("failed to send TaskRequest audio frame")?;
            }
            Ok(())
        })
    }

    fn commit_user_turn<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Ok(());
            }
            if !self.uplink_buf.is_empty() {
                let remainder = std::mem::take(&mut self.uplink_buf);
                let frame =
                    build_audio_frame(event::TASK_REQUEST, &self.state.session_id, &remainder)?;
                self.ws.send(Message::Binary(frame.into())).await?;
            }
            let end_asr = build_json_frame(
                event::END_ASR,
                Some(&self.state.session_id),
                &json!({}),
            )?;
            self.ws
                .send(Message::Binary(end_asr.into()))
                .await
                .context("failed to send EndAsr")?;
            Ok(())
        })
    }

    fn cancel_response<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // O2/v3 has no dedicated `response.cancel` control frame; fence locally.
            self.state.cancel_response();
            self.uplink_buf.clear();
            Ok(())
        })
    }

    fn submit_tool_output<'a>(
        &'a mut self,
        _call_id: &'a str,
        _output: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn next_event<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<Option<RealtimeEvent>>> {
        Box::pin(async move { self.read_one_ws_event().await })
    }

    fn close<'a>(&'a mut self) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if self.closed {
                return Ok(());
            }
            self.closed = true;
            if let Ok(finish_sess) =
                build_json_frame(event::FINISH_SESSION, Some(&self.state.session_id), &json!({}))
            {
                if let Err(err) = self.ws.send(Message::Binary(finish_sess.into())).await {
                    warn!(error = %err, "failed to send FinishSession");
                }
            }
            if let Ok(finish_conn) =
                build_json_frame(event::FINISH_CONNECTION, None, &json!({}))
            {
                let _ = self.ws.send(Message::Binary(finish_conn.into())).await;
            }
            let _ = self.ws.close(None).await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::volcengine_frame::{Compression, Serialization, FLAG_WITH_EVENT};
    use bytes::Bytes;

    fn make_server_json(ev: u32, sid: &str, body: Value) -> VolcFrame {
        VolcFrame {
            message_type: MessageType::FullServerResponse,
            flags: FLAG_WITH_EVENT,
            serialization: Serialization::Json,
            compression: Compression::None,
            event: Some(ev),
            error_code: None,
            connect_id: None,
            session_id: Some(sid.to_string()),
            payload: Bytes::from(serde_json::to_vec(&body).unwrap()),
        }
    }

    fn make_server_audio(sid: &str, pcm: &[u8]) -> VolcFrame {
        VolcFrame {
            message_type: MessageType::AudioOnlyServerResponse,
            flags: FLAG_WITH_EVENT,
            serialization: Serialization::Raw,
            compression: Compression::None,
            event: Some(event::TTS_RESPONSE),
            error_code: None,
            connect_id: None,
            session_id: Some(sid.to_string()),
            payload: Bytes::copy_from_slice(pcm),
        }
    }

    #[test]
    fn uuid_v4_has_standard_format() {
        let u1 = generate_uuid_v4();
        let u2 = generate_uuid_v4();
        assert_eq!(u1.len(), 36);
        assert_ne!(u1, u2);
        assert_eq!(&u1[14..15], "4");
        assert!(matches!(&u1[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn dual_auth_headers_support_api_key_and_app_id_pair() {
        // 1) API key takes precedence over app_id + access_key
        let mut cfg = VolcengineO2Config::default();
        cfg.api_key = "ak-secret".to_string();
        cfg.app_id = "app-1".to_string();
        cfg.access_key = "acc-1".to_string();
        let cid = generate_uuid_v4();
        let headers = build_volcengine_headers(&cfg, &cid).unwrap();
        assert!(headers.iter().any(|(k, v)| k == "X-Api-Key" && v == "ak-secret"));
        assert!(!headers.iter().any(|(k, _)| k == "X-Api-App-Id"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Api-Resource-Id" && v == "volc.speech.dialog"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Api-App-Key" && v == "PlgvMymc7f3tQnJ6"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "X-Api-Connect-Id" && v == &cid));

        // 2) App ID + Access Key fallback
        cfg.api_key.clear();
        let headers2 = build_volcengine_headers(&cfg, &cid).unwrap();
        assert!(!headers2.iter().any(|(k, _)| k == "X-Api-Key"));
        assert!(headers2.iter().any(|(k, v)| k == "X-Api-App-Id" && v == "app-1"));
        assert!(headers2
            .iter()
            .any(|(k, v)| k == "X-Api-Access-Key" && v == "acc-1"));

        // 3) Missing credentials rejected
        cfg.access_key.clear();
        assert!(build_volcengine_headers(&cfg, &cid).is_err());
    }

    #[test]
    fn start_session_payload_assembles_websearch_fields() {
        let mut cfg = VolcengineO2Config::default();
        cfg.model = "1.2.6.1".to_string();
        cfg.enable_volc_websearch = true;
        cfg.volc_websearch_type = "web_summary".to_string();
        cfg.volc_websearch_api_key = "ws-key-123".to_string();
        cfg.volc_websearch_result_count = 6;
        cfg.volc_websearch_no_result_message = "没查到相关资料".to_string();

        let payload = build_start_session_payload(&cfg, "你是语音助手");
        let extra = &payload["dialog"]["extra"];
        assert_eq!(extra["model"], "1.2.6.1");
        assert_eq!(extra["input_mod"], "keep_alive");
        assert_eq!(extra["enable_volc_websearch"], true);
        assert_eq!(extra["volc_websearch_type"], "web_summary");
        assert_eq!(extra["volc_websearch_api_key"], "ws-key-123");
        assert_eq!(extra["volc_websearch_result_count"], 6);
        assert_eq!(extra["volc_websearch_no_result_message"], "没查到相关资料");
        assert_eq!(payload["dialog"]["system_role"], "你是语音助手");
        assert_eq!(payload["tts"]["audio_config"]["sample_rate"], 24_000);
    }

    #[test]
    fn o2_state_machine_handles_full_turn_and_usage_sum() {
        let mut st = O2ProtocolState::new("s-init".to_string());

        assert!(st
            .handle_frame(make_server_json(event::CONNECTION_STARTED, "", json!({})))
            .is_empty());
        assert!(st.saw_connection_started);

        let ready = st.handle_frame(make_server_json(event::SESSION_STARTED, "s-real", json!({})));
        assert_eq!(
            ready,
            vec![RealtimeEvent::SessionReady {
                session_id: "s-real".to_string()
            }]
        );

        let asr_start = st.handle_frame(make_server_json(
            event::ASR_INFO,
            "s-real",
            json!({"question_id": "q-1"}),
        ));
        assert_eq!(asr_start, vec![RealtimeEvent::UserSpeechStarted]);

        let delta = st.handle_frame(make_server_json(
            event::ASR_RESPONSE,
            "s-real",
            json!({"question_id": "q-1", "results": [{"text": "今天北京天气", "is_interim": false}]}),
        ));
        assert_eq!(
            delta,
            vec![RealtimeEvent::UserTranscriptDelta("今天北京天气".to_string())]
        );

        let done = st.handle_frame(make_server_json(event::ASR_ENDED, "s-real", json!({})));
        assert_eq!(
            done,
            vec![RealtimeEvent::UserTranscriptCompleted(
                "今天北京天气".to_string()
            )]
        );

        assert!(st
            .handle_frame(make_server_json(
                event::TTS_SENTENCE_START,
                "s-real",
                json!({"reply_id": "q-1"})
            ))
            .is_empty());
        let txt = st.handle_frame(make_server_json(
            event::CHAT_RESPONSE,
            "s-real",
            json!({"reply_id": "q-1", "content": "晴天"}),
        ));
        assert_eq!(txt, vec![RealtimeEvent::AssistantTextDelta("晴天".to_string())]);

        let audio = st.handle_frame(make_server_audio("s-real", &[1, 2, 3, 4]));
        assert_eq!(
            audio,
            vec![RealtimeEvent::AssistantAudioDelta(Bytes::from_static(&[1, 2, 3, 4]))]
        );

        let audio_done = st.handle_frame(make_server_json(event::TTS_ENDED, "s-real", json!({})));
        assert_eq!(audio_done, vec![RealtimeEvent::AssistantAudioDone]);

        let usage = st.handle_frame(make_server_json(
            event::USAGE_RESPONSE,
            "s-real",
            json!({
                "usage": {
                    "input_text_tokens": 10,
                    "input_audio_tokens": 20,
                    "output_text_tokens": 30,
                    "output_audio_tokens": 40,
                    "cached_text_tokens": 5,
                    "cached_audio_tokens": 15
                }
            }),
        ));
        assert_eq!(
            usage,
            vec![RealtimeEvent::ResponseDone {
                usage: RealtimeUsage {
                    input_text_tokens: 10,
                    input_audio_tokens: 20,
                    output_text_tokens: 30,
                    output_audio_tokens: 40,
                    cached_text_tokens: 5,
                    cached_audio_tokens: 15,
                }
            }]
        );
        if let RealtimeEvent::ResponseDone { usage } = &usage[0] {
            assert_eq!(usage.total_tokens(), 120);
        }
    }

    #[test]
    fn cancellation_fence_drops_stale_audio_and_reopens_on_new_question() {
        let mut st = O2ProtocolState::new("s1".to_string());

        // Turn 1 starts
        st.handle_frame(make_server_json(
            event::ASR_INFO,
            "s1",
            json!({"question_id": "q-1"}),
        ));
        let first_audio = st.handle_frame(make_server_audio("s1", &[10, 20]));
        assert_eq!(first_audio.len(), 1);

        // User barges in -> local cancellation fence activates
        st.cancel_response();
        assert_eq!(st.canceled_question_id.as_deref(), Some("q-1"));
        assert!(!st.output_allowed);

        // Stale downlink frames from q-1 MUST be dropped
        assert!(st
            .handle_frame(make_server_json(
                event::TTS_SENTENCE_START,
                "s1",
                json!({"question_id": "q-1", "reply_id": "r-1"})
            ))
            .is_empty());
        assert!(st
            .handle_frame(make_server_json(
                event::CHAT_RESPONSE,
                "s1",
                json!({"question_id": "q-1", "reply_id": "r-1", "content": "旧内容"})
            ))
            .is_empty());
        assert!(st
            .handle_frame(make_server_audio("s1", &[30, 40]))
            .is_empty());
        assert!(st
            .handle_frame(make_server_json(event::TTS_ENDED, "s1", json!({})))
            .is_empty());

        // Even a stale ASR_INFO echoing q-1 must NOT reopen the fence
        st.handle_frame(make_server_json(
            event::ASR_INFO,
            "s1",
            json!({"question_id": "q-1"}),
        ));
        assert!(st
            .handle_frame(make_server_audio("s1", &[50, 60]))
            .is_empty());

        // Fresh ASR_INFO q-2 updates question_id, but fence stays closed until q-2 reply arrives
        let q2_start = st.handle_frame(make_server_json(
            event::ASR_INFO,
            "s1",
            json!({"question_id": "q-2"}),
        ));
        assert_eq!(q2_start, vec![RealtimeEvent::UserSpeechStarted]);
        assert!(!st.output_allowed);
        assert!(st
            .handle_frame(make_server_audio("s1", &[55, 65]))
            .is_empty());

        // Stale TTS_SENTENCE_START for q-1 after q-2 ASR_INFO must still be fenced
        assert!(st
            .handle_frame(make_server_json(
                event::TTS_SENTENCE_START,
                "s1",
                json!({"question_id": "q-1", "reply_id": "r-1"})
            ))
            .is_empty());
        assert!(st
            .handle_frame(make_server_audio("s1", &[58, 68]))
            .is_empty());

        // Matching TTS_SENTENCE_START for q-2 reopens the fence
        assert!(st
            .handle_frame(make_server_json(
                event::TTS_SENTENCE_START,
                "s1",
                json!({"question_id": "q-2", "reply_id": "r-2"})
            ))
            .is_empty());
        assert!(st.output_allowed);

        let q2_audio = st.handle_frame(make_server_audio("s1", &[70, 80]));
        assert_eq!(
            q2_audio,
            vec![RealtimeEvent::AssistantAudioDelta(Bytes::from_static(&[70, 80]))]
        );

        // Late stale header from q-1 re-closes the fence
        assert!(st
            .handle_frame(make_server_json(
                event::TTS_SENTENCE_START,
                "s1",
                json!({"question_id": "q-1", "reply_id": "r-1"})
            ))
            .is_empty());
        assert!(st
            .handle_frame(make_server_audio("s1", &[90, 99]))
            .is_empty());
    }
}
