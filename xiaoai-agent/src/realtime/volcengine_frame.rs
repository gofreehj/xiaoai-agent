//! Volcengine Doubao real-time voice binary frame codec.
//!
//! 1:1 Rust port of `xiaoai-doubao/xiaoai-plus/src/realtime/frame_codec.cpp`:
//! - 4-byte protocol header (`0x11`, `(msg_type << 4) | flags`, `(ser << 4) | comp`, `0x00`)
//! - Big-endian event ID, optional `connect_id` / `session_id`, and payload length
//! - Hard 1 MiB guardrail on both raw payload length and streaming Gzip decompression
//!   (protects LX06 RAM against malformed frames or zip bombs)

use std::io::Read;

use anyhow::{bail, ensure, Context};
use bytes::Bytes;
use flate2::read::GzDecoder;

/// LX06 memory guardrail: maximum raw payload bytes accepted in a single frame.
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 1024 * 1024;
/// LX06 memory guardrail: maximum decompressed Gzip bytes accepted in a single frame.
pub const MAX_GZIP_DECOMPRESSED_BYTES: usize = 1024 * 1024;

pub const PROTOCOL_VERSION: u8 = 0x1;
pub const HEADER_SIZE_WORDS: u8 = 0x1; // 1 * 4 = 4 bytes
pub const FLAG_NONE: u8 = 0x0;
pub const FLAG_WITH_EVENT: u8 = 0x4;

/// Volcengine O2/v3 protocol event IDs.
#[allow(dead_code)]
pub mod event {
    // Connection-level events
    pub const START_CONNECTION: u32 = 1;
    pub const FINISH_CONNECTION: u32 = 2;
    pub const CONNECTION_STARTED: u32 = 50;
    pub const CONNECTION_FAILED: u32 = 51;
    pub const CONNECTION_FINISHED: u32 = 52;

    // Session-level client events
    pub const START_SESSION: u32 = 100;
    pub const FINISH_SESSION: u32 = 102;
    pub const TASK_REQUEST: u32 = 200;
    pub const SAY_HELLO: u32 = 300;
    pub const END_ASR: u32 = 400;
    pub const CHAT_TEXT_QUERY: u32 = 501;

    // Session-level server events
    pub const SESSION_STARTED: u32 = 150;
    pub const SESSION_FINISHED: u32 = 152;
    pub const SESSION_FAILED: u32 = 153;
    pub const USAGE_RESPONSE: u32 = 154;
    pub const TTS_SENTENCE_START: u32 = 350;
    pub const TTS_SENTENCE_END: u32 = 351;
    pub const TTS_RESPONSE: u32 = 352;
    pub const TTS_ENDED: u32 = 359;
    pub const ASR_INFO: u32 = 450;
    pub const ASR_RESPONSE: u32 = 451;
    pub const ASR_ENDED: u32 = 459;
    pub const CHAT_RESPONSE: u32 = 550;
    pub const CHAT_ENDED: u32 = 559;
    pub const DIALOG_COMMON_ERROR: u32 = 599;

    pub fn is_connection_event(event_id: u32) -> bool {
        matches!(
            event_id,
            START_CONNECTION
                | FINISH_CONNECTION
                | CONNECTION_STARTED
                | CONNECTION_FAILED
                | CONNECTION_FINISHED
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    FullClientRequest = 0x1,
    AudioOnlyClientRequest = 0x2,
    FullServerResponse = 0x9,
    AudioOnlyServerResponse = 0xB,
    ErrorInfo = 0xF,
}

impl MessageType {
    pub fn from_nibble(v: u8) -> anyhow::Result<Self> {
        match v {
            0x1 => Ok(Self::FullClientRequest),
            0x2 => Ok(Self::AudioOnlyClientRequest),
            0x9 => Ok(Self::FullServerResponse),
            0xB => Ok(Self::AudioOnlyServerResponse),
            0xF => Ok(Self::ErrorInfo),
            other => bail!("unknown Volcengine message type 0x{other:x}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Serialization {
    Raw = 0x0,
    Json = 0x1,
}

impl Serialization {
    pub fn from_nibble(v: u8) -> anyhow::Result<Self> {
        match v {
            0x0 => Ok(Self::Raw),
            0x1 => Ok(Self::Json),
            other => bail!("unknown Volcengine serialization 0x{other:x}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Compression {
    None = 0x0,
    Gzip = 0x1,
}

impl Compression {
    pub fn from_nibble(v: u8) -> anyhow::Result<Self> {
        match v {
            0x0 => Ok(Self::None),
            0x1 => Ok(Self::Gzip),
            other => bail!("unknown Volcengine compression 0x{other:x}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolcFrame {
    pub message_type: MessageType,
    pub flags: u8,
    pub serialization: Serialization,
    pub compression: Compression,
    pub event: Option<u32>,
    pub error_code: Option<u32>,
    pub connect_id: Option<String>,
    pub session_id: Option<String>,
    pub payload: Bytes,
}

fn write_header(
    out: &mut Vec<u8>,
    message_type: MessageType,
    flags: u8,
    serialization: Serialization,
    compression: Compression,
) {
    out.push((PROTOCOL_VERSION << 4) | HEADER_SIZE_WORDS);
    out.push(((message_type as u8) << 4) | (flags & 0x0F));
    out.push(((serialization as u8) << 4) | ((compression as u8) & 0x0F));
    out.push(0x00);
}

/// Build a JSON client request frame (connection-level when `session_id` is `None`,
/// session-level when `session_id` is `Some`).
pub fn build_json_frame(
    event_id: u32,
    session_id: Option<&str>,
    payload: &serde_json::Value,
) -> anyhow::Result<Vec<u8>> {
    let payload_bytes =
        serde_json::to_vec(payload).context("failed to serialize Volcengine JSON payload")?;
    ensure!(
        payload_bytes.len() <= MAX_FRAME_PAYLOAD_BYTES,
        "Volcengine JSON payload exceeds 1 MiB limit ({} bytes)",
        payload_bytes.len()
    );

    let is_conn = event::is_connection_event(event_id);
    if !is_conn {
        ensure!(
            session_id.is_some_and(|s| !s.is_empty()),
            "session-level Volcengine event {event_id} requires non-empty session_id"
        );
    }

    let extra = if is_conn {
        0
    } else {
        4 + session_id.unwrap_or_default().len()
    };
    let mut out = Vec::with_capacity(4 + 4 + extra + 4 + payload_bytes.len());
    write_header(
        &mut out,
        MessageType::FullClientRequest,
        FLAG_WITH_EVENT,
        Serialization::Json,
        Compression::None,
    );
    out.extend_from_slice(&event_id.to_be_bytes());
    if !is_conn {
        let sid = session_id.unwrap_or_default().as_bytes();
        out.extend_from_slice(&(sid.len() as u32).to_be_bytes());
        out.extend_from_slice(sid);
    }
    out.extend_from_slice(&(payload_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload_bytes);
    Ok(out)
}

/// Build a raw PCM audio client request frame (typically `TaskRequest = 200`).
pub fn build_audio_frame(event_id: u32, session_id: &str, pcm: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(
        !session_id.is_empty(),
        "Volcengine audio frame requires non-empty session_id"
    );
    ensure!(
        pcm.len() <= MAX_FRAME_PAYLOAD_BYTES,
        "Volcengine audio payload exceeds 1 MiB limit ({} bytes)",
        pcm.len()
    );

    let sid = session_id.as_bytes();
    let mut out = Vec::with_capacity(4 + 4 + 4 + sid.len() + 4 + pcm.len());
    write_header(
        &mut out,
        MessageType::AudioOnlyClientRequest,
        FLAG_WITH_EVENT,
        Serialization::Raw,
        Compression::None,
    );
    out.extend_from_slice(&event_id.to_be_bytes());
    out.extend_from_slice(&(sid.len() as u32).to_be_bytes());
    out.extend_from_slice(sid);
    out.extend_from_slice(&(pcm.len() as u32).to_be_bytes());
    out.extend_from_slice(pcm);
    Ok(out)
}

fn read_u32_be(buf: &[u8], offset: &mut usize, field: &str) -> anyhow::Result<u32> {
    ensure!(
        *offset + 4 <= buf.len(),
        "truncated Volcengine frame while reading {field}"
    );
    let val = u32::from_be_bytes([
        buf[*offset],
        buf[*offset + 1],
        buf[*offset + 2],
        buf[*offset + 3],
    ]);
    *offset += 4;
    Ok(val)
}

fn read_len_prefixed_slice<'a>(
    buf: &'a [u8],
    offset: &mut usize,
    max_len: usize,
    field: &str,
) -> anyhow::Result<&'a [u8]> {
    let len = read_u32_be(buf, offset, field)? as usize;
    ensure!(
        len <= max_len,
        "Volcengine frame {field} length {len} exceeds limit {max_len}"
    );
    ensure!(
        *offset + len <= buf.len(),
        "truncated Volcengine frame while reading {field} ({len} bytes)"
    );
    let slice = &buf[*offset..*offset + len];
    *offset += len;
    Ok(slice)
}

fn decompress_gzip_bounded(compressed: &[u8], max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    if compressed.is_empty() {
        return Ok(Vec::new());
    }
    let decoder = GzDecoder::new(compressed);
    let mut limited = decoder.take((max_bytes + 1) as u64);
    let mut out = Vec::with_capacity(compressed.len().min(max_bytes));
    limited
        .read_to_end(&mut out)
        .context("failed to decompress Gzip Volcengine payload")?;
    ensure!(
        out.len() <= max_bytes,
        "Volcengine Gzip decompressed payload exceeds 1 MiB limit"
    );
    Ok(out)
}

fn decode_payload(raw: &[u8], compression: Compression) -> anyhow::Result<Bytes> {
    match compression {
        Compression::None => Ok(Bytes::copy_from_slice(raw)),
        Compression::Gzip => {
            let decompressed = decompress_gzip_bounded(raw, MAX_GZIP_DECOMPRESSED_BYTES)?;
            Ok(Bytes::from(decompressed))
        }
    }
}

/// Decode a raw Volcengine binary WebSocket frame into a [`VolcFrame`].
pub fn decode_frame(raw: &[u8]) -> anyhow::Result<VolcFrame> {
    ensure!(raw.len() >= 4, "Volcengine frame shorter than 4-byte header");

    let version = raw[0] >> 4;
    let header_words = (raw[0] & 0x0F) as usize;
    ensure!(
        version == PROTOCOL_VERSION,
        "unsupported Volcengine protocol version {version}"
    );
    let header_len = header_words * 4;
    ensure!(
        header_len >= 4 && header_len <= raw.len(),
        "invalid Volcengine header size {header_len}"
    );

    let message_type = MessageType::from_nibble(raw[1] >> 4)?;
    let flags = raw[1] & 0x0F;
    let serialization = Serialization::from_nibble(raw[2] >> 4)?;
    let compression = Compression::from_nibble(raw[2] & 0x0F)?;

    let mut offset = header_len;

    if message_type == MessageType::ErrorInfo {
        let error_code = read_u32_be(raw, &mut offset, "error_code")?;
        let payload_raw =
            read_len_prefixed_slice(raw, &mut offset, MAX_FRAME_PAYLOAD_BYTES, "error_payload")?;
        let payload = decode_payload(payload_raw, compression)?;
        return Ok(VolcFrame {
            message_type,
            flags,
            serialization,
            compression,
            event: None,
            error_code: Some(error_code),
            connect_id: None,
            session_id: None,
            payload,
        });
    }

    if matches!(flags & 0x03, 0x01 | 0x03) {
        let _sequence = read_u32_be(raw, &mut offset, "sequence")?;
    }

    let event_id = if (flags & FLAG_WITH_EVENT) != 0 {
        Some(read_u32_be(raw, &mut offset, "event_id")?)
    } else {
        None
    };

    let mut connect_id = None;
    let mut session_id = None;

    if let Some(ev) = event_id {
        if event::is_connection_event(ev) {
            // Server connection events (50/51/52) may prepend a length-prefixed `connect_id`
            // before the length-prefixed payload. Disambiguate cleanly by exact remaining length.
            let remaining = raw.len().saturating_sub(offset);
            if remaining >= 8 {
                let first_len = u32::from_be_bytes([
                    raw[offset],
                    raw[offset + 1],
                    raw[offset + 2],
                    raw[offset + 3],
                ]) as usize;
                let second_len_off = offset.saturating_add(4).saturating_add(first_len);
                if first_len <= 1024 && second_len_off + 4 <= raw.len() {
                    let second_len = u32::from_be_bytes([
                        raw[second_len_off],
                        raw[second_len_off + 1],
                        raw[second_len_off + 2],
                        raw[second_len_off + 3],
                    ]) as usize;
                    if second_len_off + 4 + second_len == raw.len() {
                        let cid_bytes = read_len_prefixed_slice(raw, &mut offset, 1024, "connect_id")?;
                        connect_id = Some(
                            std::str::from_utf8(cid_bytes)
                                .context("Volcengine connect_id is not valid UTF-8")?
                                .to_string(),
                        );
                    }
                }
            }
        } else {
            let sid_bytes = read_len_prefixed_slice(raw, &mut offset, 4096, "session_id")?;
            session_id = Some(
                std::str::from_utf8(sid_bytes)
                    .context("Volcengine session_id is not valid UTF-8")?
                    .to_string(),
            );
        }
    }

    let payload_raw = if offset < raw.len() {
        read_len_prefixed_slice(raw, &mut offset, MAX_FRAME_PAYLOAD_BYTES, "payload")?
    } else {
        &[]
    };
    let payload = decode_payload(payload_raw, compression)?;

    Ok(VolcFrame {
        message_type,
        flags,
        serialization,
        compression,
        event: event_id,
        error_code: None,
        connect_id,
        session_id,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression as FlateCompression;
    use std::io::Write;

    fn gzip_bytes(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), FlateCompression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn roundtrip_session_json_frame() {
        let payload = serde_json::json!({"dialog": {"bot_name": "豆包"}});
        let raw = build_json_frame(event::START_SESSION, Some("sess-123"), &payload).unwrap();
        let decoded = decode_frame(&raw).unwrap();

        assert_eq!(decoded.message_type, MessageType::FullClientRequest);
        assert_eq!(decoded.flags, FLAG_WITH_EVENT);
        assert_eq!(decoded.serialization, Serialization::Json);
        assert_eq!(decoded.compression, Compression::None);
        assert_eq!(decoded.event, Some(event::START_SESSION));
        assert_eq!(decoded.session_id.as_deref(), Some("sess-123"));
        assert_eq!(decoded.connect_id, None);
        let parsed: serde_json::Value = serde_json::from_slice(&decoded.payload).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn roundtrip_audio_frame() {
        let pcm = vec![0x12u8, 0x34, 0x56, 0x78];
        let raw = build_audio_frame(event::TASK_REQUEST, "sess-audio", &pcm).unwrap();
        let decoded = decode_frame(&raw).unwrap();

        assert_eq!(decoded.message_type, MessageType::AudioOnlyClientRequest);
        assert_eq!(decoded.serialization, Serialization::Raw);
        assert_eq!(decoded.event, Some(event::TASK_REQUEST));
        assert_eq!(decoded.session_id.as_deref(), Some("sess-audio"));
        assert_eq!(decoded.payload.as_ref(), pcm.as_slice());
    }

    #[test]
    fn decodes_connection_frame_with_and_without_connect_id() {
        // Without connect_id
        let payload = serde_json::json!({});
        let raw_no_cid = build_json_frame(event::START_CONNECTION, None, &payload).unwrap();
        let dec1 = decode_frame(&raw_no_cid).unwrap();
        assert_eq!(dec1.event, Some(event::START_CONNECTION));
        assert_eq!(dec1.connect_id, None);
        assert_eq!(dec1.payload.as_ref(), b"{}");

        // Server ConnectionStarted (50) with connect_id
        let mut raw_with_cid = Vec::new();
        write_header(
            &mut raw_with_cid,
            MessageType::FullServerResponse,
            FLAG_WITH_EVENT,
            Serialization::Json,
            Compression::None,
        );
        raw_with_cid.extend_from_slice(&event::CONNECTION_STARTED.to_be_bytes());
        let cid = b"conn-uuid-42";
        raw_with_cid.extend_from_slice(&(cid.len() as u32).to_be_bytes());
        raw_with_cid.extend_from_slice(cid);
        let body = br#"{"status":"ok"}"#;
        raw_with_cid.extend_from_slice(&(body.len() as u32).to_be_bytes());
        raw_with_cid.extend_from_slice(body);

        let dec2 = decode_frame(&raw_with_cid).unwrap();
        assert_eq!(dec2.event, Some(event::CONNECTION_STARTED));
        assert_eq!(dec2.connect_id.as_deref(), Some("conn-uuid-42"));
        assert_eq!(dec2.payload.as_ref(), body);
    }

    #[test]
    fn decodes_gzip_server_response_and_error_info() {
        let body = r#"{"content":"你好"}"#.as_bytes();
        let gz = gzip_bytes(body);

        let mut raw = Vec::new();
        write_header(
            &mut raw,
            MessageType::FullServerResponse,
            FLAG_WITH_EVENT,
            Serialization::Json,
            Compression::Gzip,
        );
        raw.extend_from_slice(&event::CHAT_RESPONSE.to_be_bytes());
        let sid = b"s1";
        raw.extend_from_slice(&(sid.len() as u32).to_be_bytes());
        raw.extend_from_slice(sid);
        raw.extend_from_slice(&(gz.len() as u32).to_be_bytes());
        raw.extend_from_slice(&gz);

        let dec = decode_frame(&raw).unwrap();
        assert_eq!(dec.event, Some(event::CHAT_RESPONSE));
        assert_eq!(dec.session_id.as_deref(), Some("s1"));
        assert_eq!(dec.payload.as_ref(), body);

        // ErrorInfo frame
        let mut err_raw = Vec::new();
        write_header(
            &mut err_raw,
            MessageType::ErrorInfo,
            FLAG_NONE,
            Serialization::Json,
            Compression::None,
        );
        err_raw.extend_from_slice(&45000010u32.to_be_bytes());
        let err_msg = br#"{"error":"invalid token"}"#;
        err_raw.extend_from_slice(&(err_msg.len() as u32).to_be_bytes());
        err_raw.extend_from_slice(err_msg);

        let dec_err = decode_frame(&err_raw).unwrap();
        assert_eq!(dec_err.message_type, MessageType::ErrorInfo);
        assert_eq!(dec_err.error_code, Some(45000010));
        assert_eq!(dec_err.payload.as_ref(), err_msg);
    }

    #[test]
    fn rejects_payload_and_gzip_bomb_over_one_mib() {
        // 1) Raw payload length header claiming > 1 MiB
        let mut raw_oversize = Vec::new();
        write_header(
            &mut raw_oversize,
            MessageType::AudioOnlyServerResponse,
            FLAG_WITH_EVENT,
            Serialization::Raw,
            Compression::None,
        );
        raw_oversize.extend_from_slice(&event::TTS_RESPONSE.to_be_bytes());
        raw_oversize.extend_from_slice(&2u32.to_be_bytes());
        raw_oversize.extend_from_slice(b"s1");
        raw_oversize.extend_from_slice(&((MAX_FRAME_PAYLOAD_BYTES + 1) as u32).to_be_bytes());
        assert!(decode_frame(&raw_oversize).is_err());

        // 2) Small compressed Gzip bomb that expands to > 1 MiB
        let big = vec![b'A'; MAX_GZIP_DECOMPRESSED_BYTES + 1024];
        let gz = gzip_bytes(&big);
        assert!(gz.len() < 16 * 1024, "compressed bomb should be small");

        let mut raw_bomb = Vec::new();
        write_header(
            &mut raw_bomb,
            MessageType::FullServerResponse,
            FLAG_WITH_EVENT,
            Serialization::Json,
            Compression::Gzip,
        );
        raw_bomb.extend_from_slice(&event::CHAT_RESPONSE.to_be_bytes());
        raw_bomb.extend_from_slice(&2u32.to_be_bytes());
        raw_bomb.extend_from_slice(b"s1");
        raw_bomb.extend_from_slice(&(gz.len() as u32).to_be_bytes());
        raw_bomb.extend_from_slice(&gz);

        let err = decode_frame(&raw_bomb).unwrap_err();
        assert!(
            err.to_string().contains("1 MiB"),
            "unexpected error: {err}"
        );
    }
}
