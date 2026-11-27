use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::info;

use crate::agent::AgentRuntime;
use crate::audio::{AudioCoordinator, FinishMode, VolumeAction};
use crate::config::{AppConfig, ConfigHandle, ServerConfig};
use crate::frontend::native_monitor::NativeWakeMonitor;
use crate::music::MusicService;
use crate::runtime::config_admin;
use crate::runtime::doctor;
use crate::session::ControlCommand;
use crate::speech::{SocketOwnership, SpeechLiveness};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AgentPhase {
    Idle = 0,
    Listening = 1,
    UserSpeaking = 2,
    Thinking = 3,
    Speaking = 4,
}

impl AgentPhase {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Listening,
            2 => Self::UserSpeaking,
            3 => Self::Thinking,
            4 => Self::Speaking,
            _ => Self::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::UserSpeaking => "user_speaking",
            Self::Thinking => "thinking",
            Self::Speaking => "speaking",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TurnLatencySnapshot {
    pub turn_id: u64,
    pub asr_ms: u64,
    pub llm_ttft_ms: Option<u64>,
    pub llm_total_ms: u64,
    pub tts_ms: u64,
    pub e2e_first_audio_ms: Option<u64>,
}

/// 进程级运行态与最近一轮各阶段耗时快照，供 `/status` 与控制台读取。
#[derive(Clone, Default)]
pub struct RuntimeTelemetry {
    phase: Arc<AtomicU8>,
    last_turn: Arc<Mutex<Option<TurnLatencySnapshot>>>,
}

impl RuntimeTelemetry {
    pub fn set_phase(&self, phase: AgentPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn phase(&self) -> AgentPhase {
        AgentPhase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn record_turn(&self, snapshot: TurnLatencySnapshot) {
        if let Ok(mut guard) = self.last_turn.lock() {
            *guard = Some(snapshot);
        }
    }

    pub fn last_turn(&self) -> Option<TurnLatencySnapshot> {
        self.last_turn
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }
}

/// 控制面能看到的运行时事实。
///
/// 刻意只放「客观可核对」的东西（socket 归属、收包时间、重启次数、进程年龄），
/// 不放「我认为我在正常工作」这类自评——`/health` 的价值就在于它比
/// 「进程还在吗」细一级。
pub struct ControlState {
    pub token: String,
    pub mode: String,
    pub socket: SocketOwnership,
    pub liveness: SpeechLiveness,
    pub native_monitor: Option<NativeWakeMonitor>,
    pub started_at: Instant,
    pub config_path: Option<PathBuf>,
    pub log_path: Option<PathBuf>,
    pub telemetry: RuntimeTelemetry,
}

/// 控制面可注入的运行时控制句柄。
///
/// 全部字段为 `Option`，未接线时对应端点降级返回 `503`（`/stop` 除外，仍按幂等成功返回）。
#[derive(Clone, Default)]
pub struct ControlHooks {
    pub command_tx: Option<mpsc::Sender<ControlCommand>>,
    pub audio: Option<AudioCoordinator>,
    pub music: Option<Arc<MusicService>>,
    pub agent: Option<Arc<AgentRuntime>>,
    pub config: Option<ConfigHandle>,
    pub reload_tx: Option<mpsc::Sender<()>>,
}

#[derive(Clone)]
struct RouterState {
    control: Arc<ControlState>,
    hooks: ControlHooks,
}

impl RouterState {
    fn music(&self) -> Option<&Arc<MusicService>> {
        self.hooks
            .music
            .as_ref()
            .or_else(|| self.hooks.audio.as_ref().map(|a| &a.music))
    }
}

/// 启动控制面。**关闭时返回 `Ok(None)`，不 bind 任何 socket**——issue #9 的
/// 验收条件之一就是「关闭时零监听」，所以这里不能先 bind 再判断。
#[allow(dead_code)] // 仅测试直接调用；运行时走 spawn_with_hooks / speak_stream_with_mode
pub async fn spawn(
    config: &ServerConfig,
    state: Arc<ControlState>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    spawn_with_hooks(config, state, ControlHooks::default()).await
}

/// 启动控制面并注入控制句柄（`/stop`、`/speak`、`/chat`、`/reload`、`/volume`、`/music/*`、`/history/clear`）。
#[allow(dead_code)]
pub async fn spawn_with_hooks(
    config: &ServerConfig,
    state: Arc<ControlState>,
    hooks: ControlHooks,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    if !config.enabled {
        info!("本地控制面未启用（server.enabled=false），不监听任何端口");
        return Ok(None);
    }

    // 决定不启动，就别先把端口占了：检查放在 bind 之前。
    if config.token.trim().is_empty() && !is_loopback(&config.host) {
        return Err(anyhow::anyhow!(
            "控制面绑在非回环地址（{}）却没有配置 token：拒绝启动。请设 server.token，或把 host 改回 127.0.0.1",
            config.host
        ));
    }

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow::anyhow!("绑定控制面 {addr} 失败: {err}"))?;
    let local = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or(addr);

    info!(
        listen = %local,
        auth = !state.token.trim().is_empty(),
        "本地控制面已启动"
    );

    let app = build_router_with_hooks(state, hooks);

    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "控制面退出");
        }
    });
    Ok(Some(handle))
}

#[cfg(test)]
fn build_router(state: Arc<ControlState>) -> Router {
    build_router_with_hooks(state, ControlHooks::default())
}

fn build_router_with_hooks(state: Arc<ControlState>, hooks: ControlHooks) -> Router {
    let router_state = RouterState {
        control: state,
        hooks,
    };

    // 鉴权写成 layer，而不是让每个 handler 自己记得调 authorize：
    // 所有非 `/` 路由默认受保护，且状态变更端点一律只允许 POST（防 CSRF）。
    let api = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/music/status", get(music_status))
        .route("/history", get(get_history))
        .route("/config", get(get_config))
        .route("/doctor", get(get_doctor))
        .route("/logs", get(get_logs))
        .route("/stop", post(stop_output))
        .route("/speak", post(speak))
        .route("/chat", post(chat))
        .route("/reload", post(reload))
        .route("/volume", post(volume))
        .route("/music/pause", post(music_pause))
        .route("/music/resume", post(music_resume))
        .route("/music/next", post(music_next))
        .route("/music/previous", post(music_previous))
        .route("/music/stop", post(music_stop))
        .route("/history/clear", post(clear_history))
        .route("/history/delete", post(delete_history))
        .route("/config/form", post(save_config_form))
        .route("/config/yaml", post(save_config_yaml))
        .route("/config/rollback", post(rollback_config))
        .route_layer(axum::middleware::from_fn_with_state(
            router_state.clone(),
            require_token,
        ));

    Router::new()
        .route("/", get(panel))
        .merge(api)
        .with_state(router_state)
}

/// 统一的 Bearer 校验。**所有非公开路由都过这里**。
///
/// 这套设计成立的前提只有一条：跨站请求带不上自定义头。所以任何会改变状态的
/// 端点都必须要求 `Authorization`，且不该用 GET（`<img>`/`<a>`/`<form>` 都能触发
/// GET，但都带不上 Bearer——GET + 不鉴权 = CSRF 跳板）。
async fn require_token(
    State(state): State<RouterState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match authorize(&state.control.token, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(status) => (status, Json(json!({ "error": "unauthorized" }))).into_response(),
    }
}

/// 控制台页面。**故意不鉴权**：它只是一张静态 HTML，本身不含任何设备信息，
/// token 由页面里输入一次、存 localStorage，之后的请求才带 Authorization。
/// 这样运维不用记 curl；同时也不会把 token 写进 URL（历史、日志都会留痕）。
async fn panel() -> Response {
    // 面板只有内联样式与脚本：
    // - `default-src 'none'` + `connect-src 'self'`：页面永远不加载外部资源（和
    //   「无 CDN」的目标一致），请求只能打到本机；
    // - `frame-ancestors 'none'`：不能被嵌进 iframe —— 加了「停止播放」
    //   这类按钮后，点击劫持就成立了。
    let csp = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    (
        [
            (axum::http::header::CONTENT_SECURITY_POLICY, csp),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(PANEL_HTML),
    )
        .into_response()
}

/// 单文件控制台：无外部依赖（设备上没有 CDN 也要能开）。
/// 放在独立的 `.html` 里而不是 raw string：编辑器有高亮、diff 只显示 HTML、
/// 也不会因为 JS 里出现 `"#` 而静默截断。
const PANEL_HTML: &str = include_str!("panel.html");

async fn health(State(state): State<RouterState>) -> Response {
    let (status, body) = health_response(&state.control);
    (status, no_store(), Json(body)).into_response()
}

async fn status(State(state): State<RouterState>) -> Response {
    (StatusCode::OK, no_store(), Json(status_body(&state.control))).into_response()
}

#[derive(Deserialize)]
struct TextPayload {
    text: String,
}

#[derive(Deserialize)]
struct VolumePayload {
    action: String,
    value: Option<i32>,
}

async fn stop_output(State(state): State<RouterState>) -> Response {
    if let Some(audio) = &state.hooks.audio {
        audio.stop_all_output().await;
    }
    if let Some(tx) = &state.hooks.command_tx {
        let _ = tx.send(ControlCommand::Stop).await;
    }
    (
        StatusCode::OK,
        no_store(),
        Json(json!({ "ok": true, "action": "stop" })),
    )
        .into_response()
}

async fn speak(State(state): State<RouterState>, Json(payload): Json<TextPayload>) -> Response {
    let text = payload.text.trim();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            no_store(),
            Json(json!({ "error": "empty_text" })),
        )
            .into_response();
    }

    if let (Some(audio), Some(config)) = (&state.hooks.audio, &state.hooks.config) {
        let cfg = config.get().await;
        match audio.speak_once(&cfg.tts, text, FinishMode::WaitForEnd).await {
            Ok(()) => {
                return (StatusCode::OK, no_store(), Json(json!({ "ok": true }))).into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    no_store(),
                    Json(json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        }
    }

    if let Some(tx) = &state.hooks.command_tx {
        if tx.send(ControlCommand::Speak(text.to_string())).await.is_ok() {
            return (StatusCode::OK, no_store(), Json(json!({ "ok": true }))).into_response();
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        no_store(),
        Json(json!({ "error": "speak_unavailable" })),
    )
        .into_response()
}

async fn chat(State(state): State<RouterState>, Json(payload): Json<TextPayload>) -> Response {
    let text = payload.text.trim();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            no_store(),
            Json(json!({ "error": "empty_text" })),
        )
            .into_response();
    }

    if let Some(tx) = &state.hooks.command_tx {
        if tx.send(ControlCommand::Chat(text.to_string())).await.is_ok() {
            return (
                StatusCode::OK,
                no_store(),
                Json(json!({ "ok": true, "queued": true })),
            )
                .into_response();
        }
    }

    if let Some(agent) = &state.hooks.agent {
        let (delta_tx, _delta_rx) = mpsc::unbounded_channel();
        match agent.run_turn_stream(text, delta_tx).await {
            Ok(result) => {
                if !result.text.trim().is_empty() {
                    if let (Some(audio), Some(config)) = (&state.hooks.audio, &state.hooks.config) {
                        let cfg = config.get().await;
                        let _ = audio
                            .speak_once(&cfg.tts, &result.text, FinishMode::WaitForEnd)
                            .await;
                    }
                }
                return (
                    StatusCode::OK,
                    no_store(),
                    Json(json!({ "ok": true, "reply": result.text })),
                )
                    .into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    no_store(),
                    Json(json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        no_store(),
        Json(json!({ "error": "chat_unavailable" })),
    )
        .into_response()
}

async fn reload(State(state): State<RouterState>) -> Response {
    if let Some(tx) = &state.hooks.reload_tx {
        if tx.send(()).await.is_ok() {
            return (
                StatusCode::OK,
                no_store(),
                Json(json!({ "ok": true, "action": "reload" })),
            )
                .into_response();
        }
    }
    if let Some(tx) = &state.hooks.command_tx {
        if tx.send(ControlCommand::Reload).await.is_ok() {
            return (
                StatusCode::OK,
                no_store(),
                Json(json!({ "ok": true, "action": "reload" })),
            )
                .into_response();
        }
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        no_store(),
        Json(json!({ "error": "reload_unavailable" })),
    )
        .into_response()
}

async fn volume(State(state): State<RouterState>, Json(payload): Json<VolumePayload>) -> Response {
    let action = match payload.action.trim().to_ascii_lowercase().as_str() {
        "up" => VolumeAction::Up,
        "down" => VolumeAction::Down,
        "mute" => VolumeAction::Mute,
        "set" => match payload.value {
            Some(v) if (0..=100).contains(&v) => VolumeAction::Set(v as u8),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    no_store(),
                    Json(json!({ "error": "invalid_volume_value" })),
                )
                    .into_response();
            }
        },
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                no_store(),
                Json(json!({ "error": "invalid_volume_action" })),
            )
                .into_response();
        }
    };

    if let Some(audio) = &state.hooks.audio {
        match audio.execute_volume(action).await {
            Ok(()) => {
                return (StatusCode::OK, no_store(), Json(json!({ "ok": true }))).into_response();
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    no_store(),
                    Json(json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        }
    }

    if let Some(tx) = &state.hooks.command_tx {
        if tx.send(ControlCommand::Volume(action)).await.is_ok() {
            return (StatusCode::OK, no_store(), Json(json!({ "ok": true }))).into_response();
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        no_store(),
        Json(json!({ "error": "volume_unavailable" })),
    )
        .into_response()
}

async fn music_status(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.status().await)).into_response()
}

async fn music_pause(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.pause().await)).into_response()
}

async fn music_resume(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.resume().await)).into_response()
}

async fn music_next(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.next().await)).into_response()
}

async fn music_previous(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.previous().await)).into_response()
}

async fn music_stop(State(state): State<RouterState>) -> Response {
    let Some(music) = state.music() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "music_unavailable" })),
        )
            .into_response();
    };
    (StatusCode::OK, no_store(), Json(music.stop().await)).into_response()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HistoryQuery {
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ClearHistoryPayload {
    clear_log: bool,
}

#[derive(Deserialize)]
struct DeleteHistoryPayload {
    id: u64,
}

#[derive(Deserialize)]
struct YamlConfigPayload {
    yaml: String,
    #[serde(default)]
    validate_only: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsQuery {
    lines: Option<usize>,
    level: Option<String>,
    q: Option<String>,
}

async fn get_history(State(state): State<RouterState>, Query(query): Query<HistoryQuery>) -> Response {
    let Some(agent) = &state.hooks.agent else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "history_unavailable" })),
        )
            .into_response();
    };
    let (entries, total, context_turns) = agent.conversation_snapshot(query.limit.unwrap_or(50)).await;
    let (max_turns, ttl_s) = AgentRuntime::history_limits();
    (
        StatusCode::OK,
        no_store(),
        Json(json!({
            "entries": entries,
            "total": total,
            "context_turns": context_turns,
            "context_max_turns": max_turns,
            "context_ttl_s": ttl_s,
        })),
    )
        .into_response()
}

async fn clear_history(State(state): State<RouterState>, body: Bytes) -> Response {
    let clear_log = if body.iter().all(u8::is_ascii_whitespace) {
        false
    } else {
        match serde_json::from_slice::<ClearHistoryPayload>(&body) {
            Ok(p) => p.clear_log,
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    no_store(),
                    Json(json!({ "error": format!("invalid_payload: {err}") })),
                )
                    .into_response();
            }
        }
    };

    let mut handled = false;
    if let Some(agent) = &state.hooks.agent {
        agent.reset_session("clear history").await;
        if clear_log {
            agent.clear_conversation_log().await;
        }
        handled = true;
    }
    if let Some(tx) = &state.hooks.command_tx {
        if tx.send(ControlCommand::ClearHistory).await.is_ok() {
            handled = true;
        }
    }
    if handled {
        (
            StatusCode::OK,
            no_store(),
            Json(json!({ "ok": true, "cleared_log": clear_log })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "history_unavailable" })),
        )
            .into_response()
    }
}

async fn delete_history(
    State(state): State<RouterState>,
    Json(payload): Json<DeleteHistoryPayload>,
) -> Response {
    let Some(agent) = &state.hooks.agent else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "history_unavailable" })),
        )
            .into_response();
    };
    let deleted = agent.delete_conversation_entry(payload.id).await;
    (
        StatusCode::OK,
        no_store(),
        Json(json!({ "ok": true, "deleted": deleted })),
    )
        .into_response()
}

async fn trigger_reload_after_save(state: &RouterState, path: &Path) -> bool {
    if let Some(tx) = &state.hooks.reload_tx {
        if tx.send(()).await.is_ok() {
            return true;
        }
    }
    if let (Some(cfg), Some(agent)) = (&state.hooks.config, &state.hooks.agent) {
        crate::runtime::reload::apply_config_reload(cfg, path, agent).await;
        return true;
    }
    false
}

async fn get_config(State(state): State<RouterState>) -> Response {
    let (Some(path), Some(cfg_handle)) = (&state.control.config_path, &state.hooks.config) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "config_unavailable" })),
        )
            .into_response();
    };
    let effective = cfg_handle.get().await;
    match config_admin::read_config_view(path, &effective) {
        Ok(view) => (StatusCode::OK, no_store(), Json(view)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            no_store(),
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn save_config_form(State(state): State<RouterState>, Json(patch): Json<Value>) -> Response {
    let (Some(path), Some(cfg_handle)) = (&state.control.config_path, &state.hooks.config) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "config_unavailable" })),
        )
            .into_response();
    };
    let effective = cfg_handle.get().await;
    match config_admin::validate_and_save_form(path, &patch, &effective) {
        Ok(outcome) => {
            let reloaded = if outcome.should_reload {
                trigger_reload_after_save(&state, path).await
            } else {
                false
            };
            (
                StatusCode::OK,
                no_store(),
                Json(json!({
                    "ok": true,
                    "reloaded": reloaded,
                    "outcome": outcome,
                })),
            )
                .into_response()
        }
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            no_store(),
            Json(json!({ "error": msg })),
        )
            .into_response(),
    }
}

async fn save_config_yaml(
    State(state): State<RouterState>,
    Json(payload): Json<YamlConfigPayload>,
) -> Response {
    let (Some(path), Some(cfg_handle)) = (&state.control.config_path, &state.hooks.config) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "config_unavailable" })),
        )
            .into_response();
    };
    let effective = cfg_handle.get().await;
    match config_admin::validate_and_save_yaml(path, &payload.yaml, &effective, payload.validate_only) {
        Ok(outcome) => {
            let reloaded = if outcome.should_reload {
                trigger_reload_after_save(&state, path).await
            } else {
                false
            };
            (
                StatusCode::OK,
                no_store(),
                Json(json!({
                    "ok": true,
                    "validate_only": payload.validate_only,
                    "reloaded": reloaded,
                    "outcome": outcome,
                })),
            )
                .into_response()
        }
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            no_store(),
            Json(json!({ "error": msg })),
        )
            .into_response(),
    }
}

async fn rollback_config(State(state): State<RouterState>) -> Response {
    let (Some(path), Some(cfg_handle)) = (&state.control.config_path, &state.hooks.config) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            no_store(),
            Json(json!({ "error": "config_unavailable" })),
        )
            .into_response();
    };
    let effective = cfg_handle.get().await;
    match config_admin::rollback_from_backup(path, &effective) {
        Ok(outcome) => {
            let reloaded = if outcome.should_reload {
                trigger_reload_after_save(&state, path).await
            } else {
                false
            };
            (
                StatusCode::OK,
                no_store(),
                Json(json!({
                    "ok": true,
                    "reloaded": reloaded,
                    "outcome": outcome,
                })),
            )
                .into_response()
        }
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            no_store(),
            Json(json!({ "error": msg })),
        )
            .into_response(),
    }
}

async fn get_doctor(State(state): State<RouterState>) -> Response {
    let cfg = if let Some(handle) = &state.hooks.config {
        (*handle.get().await).clone()
    } else if let Some(path) = &state.control.config_path {
        AppConfig::load(path).unwrap_or_default()
    } else {
        AppConfig::default()
    };
    let report = doctor::collect_doctor_report(
        &cfg,
        state.control.config_path.as_deref(),
        state.control.native_monitor.as_ref(),
    )
    .await;
    (StatusCode::OK, no_store(), Json(report)).into_response()
}

fn resolve_log_path(state: &ControlState) -> PathBuf {
    if let Some(path) = &state.log_path {
        return path.clone();
    }
    if let Ok(env_path) = std::env::var("XIAOAI_AGENT_LOG") {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    let primary = PathBuf::from("/tmp/xiaoai-agent.log");
    if primary.exists() {
        return primary;
    }
    let secondary = PathBuf::from("/tmp/xiaoai-agent/agent.log");
    if secondary.exists() {
        return secondary;
    }
    primary
}

fn strip_ansi_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

async fn get_logs(State(state): State<RouterState>, Query(query): Query<LogsQuery>) -> Response {
    const MAX_TAIL_BYTES: u64 = 64 * 1024;
    let path = resolve_log_path(&state.control);
    let max_lines = query.lines.unwrap_or(150).clamp(1, 500);
    let level_filter = query
        .level
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase);
    let keyword = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    if !path.exists() {
        return (
            StatusCode::OK,
            no_store(),
            Json(json!({
                "path": path.display().to_string(),
                "exists": false,
                "file_size_bytes": 0,
                "truncated": false,
                "lines": Vec::<String>::new(),
            })),
        )
            .into_response();
    }

    let Ok(mut file) = OpenOptions::new().read(true).open(&path) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            no_store(),
            Json(json!({ "error": "cannot_open_log" })),
        )
            .into_response();
    };
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let truncated = file_size > MAX_TAIL_BYTES;
    let start = file_size.saturating_sub(MAX_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            no_store(),
            Json(json!({ "error": "cannot_seek_log" })),
        )
            .into_response();
    }

    let mut buf = Vec::with_capacity((file_size - start) as usize);
    if file.read_to_end(&mut buf).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            no_store(),
            Json(json!({ "error": "cannot_read_log" })),
        )
            .into_response();
    }

    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        if let Some(pos) = text.find('\n') {
            text = text[pos + 1..].to_string();
        }
    }

    let mut matched: Vec<String> = text
        .lines()
        .map(strip_ansi_escapes)
        .filter(|line| {
            if let Some(ref lvl) = level_filter {
                if !line.to_ascii_uppercase().contains(lvl) {
                    return false;
                }
            }
            if let Some(ref kw) = keyword {
                if !line.to_ascii_lowercase().contains(kw) {
                    return false;
                }
            }
            true
        })
        .collect();

    if matched.len() > max_lines {
        let drop_n = matched.len() - max_lines;
        matched.drain(..drop_n);
    }

    (
        StatusCode::OK,
        no_store(),
        Json(json!({
            "path": path.display().to_string(),
            "exists": true,
            "file_size_bytes": file_size,
            "truncated": truncated,
            "lines": matched,
        })),
    )
        .into_response()
}

/// 监控端点被缓存没有任何好处（`fetch` 默认会走 HTTP 缓存），显式禁掉。
fn no_store() -> [(axum::http::header::HeaderName, &'static str); 2] {
    [
        (axum::http::header::CACHE_CONTROL, "no-store"),
        (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ]
}

/// 探活判据：**只看一件不会撒谎的事实——speech socket 还是不是我们的**。
///
/// 让位、被原生抢回去、socket 被 unlink 重建，都会让这条从 true 变成 false，
/// 而进程本身还活得好好的。所以「进程在」不能当健康，inode 归属才能当健康。
/// 收包年龄只作为诊断字段返回，不参与判定：没人说话时长期没有音频包是正常的。
fn health_response(state: &ControlState) -> (StatusCode, Value) {
    if state.mode == "dual" {
        let body = json!({
            "status": "ok",
            "mode": "dual",
            "uptime_s": state.started_at.elapsed().as_secs(),
            "socket": {
                "path": state.socket.path().display().to_string(),
                "owned": false,
            },
            "speech": speech_json(state),
            "native_interlock": state.native_monitor.as_ref().map(NativeWakeMonitor::status),
        });
        return (StatusCode::OK, body);
    }

    let owned = state.socket.still_owned();
    let body = json!({
        "status": if owned { "ok" } else { "socket_not_owned" },
        "mode": state.mode,
        "uptime_s": state.started_at.elapsed().as_secs(),
        "socket": {
            "path": state.socket.path().display().to_string(),
            "owned": owned,
        },
        "speech": speech_json(state),
    });
    let code = if owned {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, body)
}

fn status_body(state: &ControlState) -> Value {
    json!({
        "pid": std::process::id(),
        "mode": state.mode,
        "uptime_s": state.started_at.elapsed().as_secs(),
        "config": state.config_path.as_ref().map(|p| p.display().to_string()),
        "phase": state.telemetry.phase().as_str(),
        "last_turn": state.telemetry.last_turn(),
        "socket": {
            "path": state.socket.path().display().to_string(),
            "owned": state.socket.still_owned(),
        },
        "speech": speech_json(state),
        "native_interlock": state.native_monitor.as_ref().map(NativeWakeMonitor::status),
    })
}

fn speech_json(state: &ControlState) -> Value {
    json!({
        "last_packet_age_s": state.liveness.last_packet_age().map(|age| age.as_secs()),
        "service_restarts": state.liveness.restarts(),
    })
}

/// Bearer 鉴权。空 token = 不鉴权（配置默认只绑回环，此时风险可控）。
fn authorize(token: &str, headers: &HeaderMap) -> Result<(), StatusCode> {
    if token.trim().is_empty() {
        return Ok(());
    }
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = format!("Bearer {token}");
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// 定长比较：控制面在局域网里，token 泄露的代价是「任何人都能让音箱说话」，
/// 不值得为省十行代码留一个理论上可行的时序侧信道。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 回环判定用 `IpAddr` 解析，而不是字面量匹配：`127.0.0.2`、`[::1]`
/// 这类写法都该算回环，字面量匹配会把它们误判成「对外暴露」。
fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unattributed() -> SocketOwnership {
        SocketOwnership::unattributed(temp_sock("unattributed"))
    }
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;

    fn state_with_ownership(token: &str, socket: SocketOwnership) -> Arc<ControlState> {
        let liveness = SpeechLiveness {
            last_packet_ms: Arc::new(AtomicU64::new(crate::speech::now_ms())),
            restarts: Arc::new(AtomicU32::new(0)),
        };
        Arc::new(ControlState {
            token: token.to_string(),
            mode: "exclusive".to_string(),
            socket,
            liveness,
            native_monitor: None,
            started_at: Instant::now(),
            config_path: None,
            log_path: None,
            telemetry: RuntimeTelemetry::default(),
        })
    }

    /// 临时占位文件要自己收：每次跑测试都留一个 `xiaoai-health-*` 的话，开发机会越攒越多。
    fn cleanup(state: &Arc<ControlState>) {
        let _ = std::fs::remove_file(state.socket.path());
    }

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xiaoai-health-{}-{tag}.usock", std::process::id()))
    }

    /// 路径存在、inode 就是我们绑的那个。
    fn still_owned_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let path = temp_sock(tag);
        std::fs::write(&path, b"agent").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        SocketOwnership::for_test(path, inode)
    }

    /// 路径存在，但 inode 已经换成原生的（复刻 launcher 的 `rm` + `mv`）。
    fn taken_over_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let live = temp_sock(tag);
        let native = live.with_extension("native.usock");
        std::fs::write(&live, b"agent").unwrap();
        std::fs::write(&native, b"native").unwrap();
        let mine = std::fs::metadata(&live).unwrap().ino();
        std::fs::remove_file(&live).unwrap();
        std::fs::rename(&native, &live).unwrap();
        SocketOwnership::for_test(live, mine)
    }

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    /// 关闭时绝不 bind：这是 issue #9 的验收条件（netstat 里不该出现这个端口）。
    #[tokio::test]
    async fn disabled_server_binds_nothing() {
        let config = ServerConfig {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 8765,
            token: String::new(),
        };
        let handle = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap();
        assert!(handle.is_none(), "关闭状态不该产生监听任务");
    }

    #[tokio::test]
    async fn enabled_server_listens_and_serves_health() {
        let config = ServerConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0, // 交给内核挑端口，避免测试之间抢端口
            token: "t".to_string(),
        };
        let handle = spawn(&config, state_with_ownership("t", unattributed()))
            .await
            .unwrap();
        assert!(handle.is_some(), "启用后必须真的起监听");
        handle.unwrap().abort();
    }

    /// 空 token = 不鉴权；设了 token 就必须带对 `Bearer`。
    #[test]
    fn bearer_token_is_required_when_configured() {
        assert!(authorize("", &HeaderMap::new()).is_ok(), "空 token 不鉴权");

        assert!(authorize("s3cret", &HeaderMap::new()).is_err());
        assert!(authorize("s3cret", &headers_with("Bearer wrong")).is_err());
        assert!(
            authorize("s3cret", &headers_with("s3cret")).is_err(),
            "缺 Bearer 前缀也要拒"
        );
        assert!(authorize("s3cret", &headers_with("Bearer s3cret")).is_ok());
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    /// 控制台页面：不带令牌也能打开（它只是一张静态页），但页面里的 API 仍然要鉴权。
    #[tokio::test]
    async fn panel_opens_without_a_token_but_the_api_still_requires_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = state_with_ownership("s3cret", still_owned_socket("panel"));
        let app = build_router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let page = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let body = page.text().await.unwrap();
        assert!(body.contains("XiaoAI 控制台"), "页面应当包含标题");
        assert!(
            body.contains("/health") && body.contains("Bearer"),
            "页面要真的去调 API，并且用 Bearer"
        );

        // 除了面板，**每一条**路由都必须要求鉴权。这是路由级断言：
        // 新增端点忘了挂 layer 会直接红。
        let client = reqwest::Client::new();
        for path in [
            "/health",
            "/status",
            "/music/status",
            "/history",
            "/config",
            "/doctor",
            "/logs",
        ] {
            let unauth = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(unauth.status(), 401, "GET {path} 无令牌必须 401");
        }
        for path in [
            "/stop",
            "/speak",
            "/chat",
            "/reload",
            "/volume",
            "/music/stop",
            "/music/pause",
            "/music/resume",
            "/music/next",
            "/music/previous",
            "/history/clear",
            "/history/delete",
            "/config/form",
            "/config/yaml",
            "/config/rollback",
        ] {
            let unauth = client
                .post(format!("http://127.0.0.1:{port}{path}"))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(unauth.status(), 401, "POST {path} 无令牌必须 401");
        }

        // 状态变更端点绝不允许 GET（防 CSRF），哪怕带了合法令牌也必须 405 Method Not Allowed。
        for path in [
            "/stop",
            "/speak",
            "/volume",
            "/reload",
            "/history/clear",
            "/history/delete",
            "/config/form",
            "/config/yaml",
            "/config/rollback",
        ] {
            let get_mut = client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .bearer_auth("s3cret")
                .send()
                .await
                .unwrap();
            assert_eq!(
                get_mut.status(),
                405,
                "GET {path} 必须 405 Method Not Allowed"
            );
        }

        let authed = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            authed.status(),
            200,
            "带对令牌要拿到 200（socket 仍归本进程）"
        );
        assert_eq!(
            authed
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
            "监控端点不能被缓存"
        );

        // 面板与 API 的字段契约：改名必须两边一起改，否则页面静默显示 0。
        let body: Value = authed.json().await.unwrap();
        assert!(body["uptime_s"].is_u64(), "uptime_s 必须是数字");
        assert!(
            body["socket"]["owned"].is_boolean(),
            "socket.owned 必须是布尔"
        );
        assert!(
            body["speech"]["service_restarts"].is_u64(),
            "speech.service_restarts 必须是数字"
        );
        assert!(
            body["speech"].get("last_packet_age_s").is_some(),
            "last_packet_age_s 允许是 null，但键必须在"
        );
        for field in ["uptime_s", "service_restarts", "last_packet_age_s", "owned"] {
            assert!(
                PANEL_HTML.contains(field),
                "页面读取的字段 `{field}` 在 panel.html 里找不到"
            );
        }
        cleanup(&state);
    }

    /// 判据一：路径还是我们绑的那个 inode → 200 / owned: true。
    #[test]
    fn health_is_ok_only_while_the_socket_is_still_ours() {
        let socket = still_owned_socket("healthy");
        let state = state_with_ownership("", socket);
        let (code, body) = health_response(&state);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["socket"]["owned"], true);
        cleanup(&state);
    }

    /// `/status` 除了健康字段，还要给出排查用的 pid、阶段与最近一轮耗时（与健康判据无关，单独一条）。
    #[test]
    fn status_reports_the_process_id_and_telemetry() {
        let state = state_with_ownership("t", unattributed());
        state.telemetry.set_phase(AgentPhase::Thinking);
        state.telemetry.record_turn(TurnLatencySnapshot {
            turn_id: 9,
            asr_ms: 420,
            llm_ttft_ms: Some(610),
            llm_total_ms: 1300,
            tts_ms: 950,
            e2e_first_audio_ms: Some(1150),
        });
        let body = status_body(&state);
        assert_eq!(body["pid"], std::process::id());
        assert_eq!(body["phase"], "thinking");
        assert_eq!(body["last_turn"]["turn_id"], 9);
        assert_eq!(body["last_turn"]["asr_ms"], 420);
        assert_eq!(body["last_turn"]["llm_ttft_ms"], 610);
        assert_eq!(body["last_turn"]["e2e_first_audio_ms"], 1150);
    }

    /// 判据二：inode 换成了原生的（被让位/被抢回）→ 503。
    #[test]
    fn health_is_503_when_the_socket_was_taken_over() {
        let socket = taken_over_socket("taken");
        let state = state_with_ownership("", socket);
        let (code, body) = health_response(&state);
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "socket_not_owned");
        assert_eq!(body["socket"]["owned"], false);
        cleanup(&state);
    }

    /// 判据三（复核指出的那个洞）：**路径不存在**时不能算「还是我的」。
    /// `taken_over()` 在这里返回 false，用它取反就会误报 200 —— 所以必须用
    /// `still_owned()`：不知道归属时按不健康报。
    #[test]
    fn health_is_503_when_the_socket_path_is_gone() {
        let missing = temp_sock("gone");
        let _ = std::fs::remove_file(&missing);
        let socket = SocketOwnership::unattributed(missing);
        let (code, body) = health_response(&state_with_ownership("", socket));
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["socket"]["owned"], false);
    }

    /// 非回环 + 空 token → 拒绝启动（在写端点进来之前就把边界划死）。
    #[tokio::test]
    async fn non_loopback_without_a_token_is_refused() {
        let config = ServerConfig {
            enabled: true,
            host: "0.0.0.0".to_string(),
            port: 0,
            token: String::new(),
        };
        let err = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("拒绝启动"), "{err}");
    }

    /// 回环 + 空 token 允许（默认值就是这个组合，只在本机可达）。
    #[tokio::test]
    async fn loopback_without_a_token_is_allowed() {
        let config = ServerConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0,
            token: String::new(),
        };
        let handle = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap()
            .expect("回环 + 空 token 应当允许启动");
        // JoinHandle 的 drop 不会取消任务，测试里必须自己 abort。
        handle.abort();
    }

    #[test]
    fn loopback_detection_accepts_the_usual_spellings() {
        for host in ["127.0.0.1", "127.0.0.2", "localhost", "::1", "[::1]"] {
            assert!(is_loopback(host), "{host} 应判为回环");
        }
        for host in ["0.0.0.0", "192.168.1.181", "[::]"] {
            assert!(!is_loopback(host), "{host} 不该判为回环");
        }
    }

    async fn test_audio_and_config(
        shell: Arc<crate::shell::RecordingShell>,
    ) -> (AudioCoordinator, ConfigHandle, Arc<MusicService>) {
        let mut cfg = crate::config::AppConfig::default();
        cfg.airplay.enabled = false;
        cfg.tts.provider = crate::config::TtsProvider::Native;
        let config_handle = ConfigHandle::new(Arc::new(cfg.clone()));
        let device = crate::device::Device::with_runner(cfg.device.clone(), shell);
        let playback = Arc::new(crate::tts::PlaybackGate::default());
        let tts = crate::tts::TtsRouter::new(device.clone(), playback.clone());
        let music = Arc::new(MusicService::new(config_handle.clone(), device.clone()).unwrap());
        let airplay = crate::airplay::AirPlayService::start(cfg.airplay)
            .await
            .unwrap();
        let audio = AudioCoordinator::new(
            device,
            cfg.device,
            playback,
            tts,
            music.clone(),
            airplay,
            RuntimeTelemetry::default(),
            Arc::new(crate::tts::TtsHealth::default()),
        );
        (audio, config_handle, music)
    }

    async fn spawn_test_server_with_hooks(
        token: &str,
        hooks: ControlHooks,
    ) -> (u16, Arc<ControlState>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = state_with_ownership(token, still_owned_socket(&format!("hooks-{port}")));
        let app = build_router_with_hooks(state.clone(), hooks);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, state, handle)
    }

    #[tokio::test]
    async fn post_stop_aborts_audio_and_emits_control_command() {
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let (audio, _cfg, _music) = test_audio_and_config(shell.clone()).await;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let hooks = ControlHooks {
            command_tx: Some(cmd_tx),
            audio: Some(audio.clone()),
            ..ControlHooks::default()
        };
        let (port, state, handle) = spawn_test_server_with_hooks("tok", hooks).await;

        let res = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/stop"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["action"], "stop");
        assert!(matches!(cmd_rx.try_recv(), Ok(ControlCommand::Stop)));
        assert!(shell.count_matching("killall tts_play.sh miplayer") >= 1);

        handle.abort();
        cleanup(&state);
    }

    #[tokio::test]
    async fn post_volume_validates_and_dispatches_actions() {
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let (audio, _cfg, _music) = test_audio_and_config(shell.clone()).await;
        let hooks = ControlHooks {
            audio: Some(audio),
            ..ControlHooks::default()
        };
        let (port, state, handle) = spawn_test_server_with_hooks("tok", hooks).await;
        let client = reqwest::Client::new();

        for payload in [
            json!({"action": "up"}),
            json!({"action": "down"}),
            json!({"action": "mute"}),
            json!({"action": "set", "value": 50}),
        ] {
            let res = client
                .post(format!("http://127.0.0.1:{port}/volume"))
                .bearer_auth("tok")
                .json(&payload)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200, "payload={payload}");
            let body: Value = res.json().await.unwrap();
            assert_eq!(body["ok"], true);
        }

        assert_eq!(shell.count_matching("volume_up"), 1);
        assert_eq!(shell.count_matching("volume_down"), 1);
        assert_eq!(shell.count_matching("volume_set 0"), 1);
        assert_eq!(shell.count_matching("volume_set 50"), 1);

        for bad in [
            json!({"action": "bogus"}),
            json!({"action": "set"}),
            json!({"action": "set", "value": 101}),
            json!({"action": "set", "value": -1}),
        ] {
            let res = client
                .post(format!("http://127.0.0.1:{port}/volume"))
                .bearer_auth("tok")
                .json(&bad)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 400, "bad payload={bad} must return 400");
        }

        handle.abort();
        cleanup(&state);
    }

    #[tokio::test]
    async fn post_speak_rejects_empty_and_accepts_valid_text() {
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let (audio, config, _music) = test_audio_and_config(shell.clone()).await;
        let hooks = ControlHooks {
            audio: Some(audio),
            config: Some(config),
            ..ControlHooks::default()
        };
        let (port, state, handle) = spawn_test_server_with_hooks("tok", hooks).await;
        let client = reqwest::Client::new();

        let empty = client
            .post(format!("http://127.0.0.1:{port}/speak"))
            .bearer_auth("tok")
            .json(&json!({"text": "   "}))
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), 400, "空文本必须返回 400");

        let ok = client
            .post(format!("http://127.0.0.1:{port}/speak"))
            .bearer_auth("tok")
            .json(&json!({"text": "你好小爱"}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        let body: Value = ok.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert!(shell.count_matching("你好小爱") >= 1);

        handle.abort();
        cleanup(&state);
    }

    #[tokio::test]
    async fn post_history_clear_reload_chat_and_music_endpoints_work() {
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let (audio, _config, music) = test_audio_and_config(shell.clone()).await;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let (reload_tx, mut reload_rx) = mpsc::channel(4);
        let hooks = ControlHooks {
            command_tx: Some(cmd_tx),
            audio: Some(audio),
            music: Some(music),
            reload_tx: Some(reload_tx),
            ..ControlHooks::default()
        };
        let (port, state, handle) = spawn_test_server_with_hooks("tok", hooks).await;
        let client = reqwest::Client::new();

        // /history/clear
        let res = client
            .post(format!("http://127.0.0.1:{port}/history/clear"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.json::<Value>().await.unwrap()["ok"], true);
        assert!(matches!(cmd_rx.try_recv(), Ok(ControlCommand::ClearHistory)));

        // /reload
        let res = client
            .post(format!("http://127.0.0.1:{port}/reload"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert!(reload_rx.try_recv().is_ok());

        // /chat empty -> 400, valid -> 200
        let empty_chat = client
            .post(format!("http://127.0.0.1:{port}/chat"))
            .bearer_auth("tok")
            .json(&json!({"text": ""}))
            .send()
            .await
            .unwrap();
        assert_eq!(empty_chat.status(), 400);

        let ok_chat = client
            .post(format!("http://127.0.0.1:{port}/chat"))
            .bearer_auth("tok")
            .json(&json!({"text": "今天天气怎么样"}))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_chat.status(), 200);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(ControlCommand::Chat(ref text)) if text == "今天天气怎么样"
        ));

        // GET /music/status + POST /music/stop
        let ms = client
            .get(format!("http://127.0.0.1:{port}/music/status"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap();
        assert_eq!(ms.status(), 200);
        assert_eq!(ms.json::<Value>().await.unwrap()["playing"], false);

        let mstop = client
            .post(format!("http://127.0.0.1:{port}/music/stop"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(mstop.status(), 200);
        assert_eq!(mstop.json::<Value>().await.unwrap()["stopped"], true);

        handle.abort();
        cleanup(&state);
    }

    #[test]
    fn dual_mode_health_returns_200_and_reports_native_interlock_state() {
        let mut ni_cfg = crate::config::NativeInterlockConfig::default();
        ni_cfg.syslog_path = None;
        ni_cfg.instruction_log_path = None;
        let monitor = NativeWakeMonitor::spawn(&ni_cfg);
        let state = Arc::new(ControlState {
            token: String::new(),
            mode: "dual".to_string(),
            socket: SocketOwnership::unattributed(PathBuf::from("udp://127.0.0.1:5005")),
            liveness: SpeechLiveness::default(),
            native_monitor: Some(monitor.clone()),
            started_at: Instant::now(),
            config_path: None,
            log_path: None,
            telemetry: RuntimeTelemetry::default(),
        });

        let (code, body) = health_response(&state);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["mode"], "dual");
        assert_eq!(body["native_interlock"]["state"], "idle");

        monitor.notify_native_utterance();
        let (code2, body2) = health_response(&state);
        assert_eq!(code2, StatusCode::OK);
        assert_eq!(body2["native_interlock"]["state"], "cooldown");
        assert_eq!(body2["native_interlock"]["wake_count"], 1);
    }

    #[tokio::test]
    async fn history_endpoints_list_delete_and_optionally_clear_log() {
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let (audio, cfg, music) = test_audio_and_config(shell.clone()).await;
        let device = crate::device::Device::with_runner(cfg.get().await.device.clone(), shell);
        let agent = Arc::new(AgentRuntime::new(cfg.clone(), device, music.clone()).await.unwrap());
        agent.record_partial_turn("你好", "在呢").await;
        agent.record_partial_turn("北京天气", "晴天").await;

        let hooks = ControlHooks {
            audio: Some(audio),
            music: Some(music),
            agent: Some(agent.clone()),
            config: Some(cfg),
            ..ControlHooks::default()
        };
        let (port, state, handle) = spawn_test_server_with_hooks("tok", hooks).await;
        let client = reqwest::Client::new();

        let list = client
            .get(format!("http://127.0.0.1:{port}/history?limit=10"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap();
        assert_eq!(list.status(), 200);
        let body: Value = list.json().await.unwrap();
        assert_eq!(body["total"], 2);
        let first_id = body["entries"][0]["id"].as_u64().unwrap();

        // /history/clear with empty JSON clears LLM context only
        let clear_ctx = client
            .post(format!("http://127.0.0.1:{port}/history/clear"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(clear_ctx.status(), 200);
        assert_eq!(clear_ctx.json::<Value>().await.unwrap()["cleared_log"], false);
        assert_eq!(agent.history_len_for_test().await, 0);

        // /history/delete deletes one entry
        let del = client
            .post(format!("http://127.0.0.1:{port}/history/delete"))
            .bearer_auth("tok")
            .json(&json!({ "id": first_id }))
            .send()
            .await
            .unwrap();
        assert_eq!(del.status(), 200);
        assert_eq!(del.json::<Value>().await.unwrap()["deleted"], true);

        // /history/clear with clear_log=true empties admin log too
        let clear_all = client
            .post(format!("http://127.0.0.1:{port}/history/clear"))
            .bearer_auth("tok")
            .json(&json!({ "clear_log": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(clear_all.status(), 200);
        assert_eq!(clear_all.json::<Value>().await.unwrap()["cleared_log"], true);
        let after = client
            .get(format!("http://127.0.0.1:{port}/history"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(after["total"], 0);

        handle.abort();
        cleanup(&state);
    }

    #[tokio::test]
    async fn config_endpoints_redact_validate_save_reload_and_rollback() {
        let dir = std::env::temp_dir().join(format!("xiaoai-srv-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("agent.yaml");
        std::fs::write(
            &cfg_path,
            "runtime:\n  mode: dual\nllm:\n  model: m1\n  api_key: secret-abc\nagent:\n  tools_enabled: false\nairplay:\n  enabled: false\ntts:\n  provider: native\n",
        )
        .unwrap();

        let initial = AppConfig::load(&cfg_path).unwrap();
        let cfg_handle = ConfigHandle::new(Arc::new(initial.clone()));
        let shell = Arc::new(crate::shell::RecordingShell::always_ok());
        let device = crate::device::Device::with_runner(initial.device.clone(), shell.clone());
        let music = Arc::new(MusicService::new(cfg_handle.clone(), device.clone()).unwrap());
        let agent = Arc::new(AgentRuntime::new(cfg_handle.clone(), device, music.clone()).await.unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(ControlState {
            token: "tok".to_string(),
            mode: "dual".to_string(),
            socket: SocketOwnership::unattributed(PathBuf::from("udp://127.0.0.1:5005")),
            liveness: SpeechLiveness::default(),
            native_monitor: None,
            started_at: Instant::now(),
            config_path: Some(cfg_path.clone()),
            log_path: None,
            telemetry: RuntimeTelemetry::default(),
        });
        let hooks = ControlHooks {
            agent: Some(agent),
            config: Some(cfg_handle.clone()),
            music: Some(music),
            ..ControlHooks::default()
        };
        let app = build_router_with_hooks(state.clone(), hooks);
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let get_cfg: Value = client
            .get(format!("http://127.0.0.1:{port}/config"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!get_cfg["yaml"].as_str().unwrap().contains("secret-abc"));
        assert_eq!(get_cfg["form"]["llm"]["api_key_set"], true);

        // Invalid YAML syntax or invalid config schema -> 400 and does NOT modify disk
        for bad_yaml in ["llm: [\nbroken", "tts:\n  provider: bogus_provider\n"] {
            let bad = client
                .post(format!("http://127.0.0.1:{port}/config/yaml"))
                .bearer_auth("tok")
                .json(&json!({ "yaml": bad_yaml, "validate_only": false }))
                .send()
                .await
                .unwrap();
            assert_eq!(bad.status(), 400);
            assert_eq!(AppConfig::load(&cfg_path).unwrap().llm.model, "m1");
        }

        // Save form -> updates disk & reloads effective ConfigHandle while keeping secret
        let save_form = client
            .post(format!("http://127.0.0.1:{port}/config/form"))
            .bearer_auth("tok")
            .json(&json!({ "llm": { "model": "m2", "api_key": "" } }))
            .send()
            .await
            .unwrap();
        assert_eq!(save_form.status(), 200);
        let save_body: Value = save_form.json().await.unwrap();
        assert_eq!(save_body["reloaded"], true);
        assert_eq!(cfg_handle.get().await.llm.model, "m2");
        assert_eq!(cfg_handle.get().await.llm.api_key, "secret-abc");

        // Rollback -> restores m1
        let rb = client
            .post(format!("http://127.0.0.1:{port}/config/rollback"))
            .bearer_auth("tok")
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(rb.status(), 200);
        assert_eq!(cfg_handle.get().await.llm.model, "m1");

        handle.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn logs_and_doctor_endpoints_return_bounded_tail_and_diagnostics() {
        let dir = std::env::temp_dir().join(format!("xiaoai-srv-diag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("agent.log");
        let mut log_content = String::new();
        for i in 0..300 {
            let lvl = if i % 10 == 0 { "\x1b[33mWARN\x1b[0m" } else { "\x1b[32mINFO\x1b[0m" };
            log_content.push_str(&format!("2026-11-27T15:00:00Z {lvl} line={i} needle_{}\n", i % 3));
        }
        std::fs::write(&log_path, &log_content).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(ControlState {
            token: "tok".to_string(),
            mode: "exclusive".to_string(),
            socket: still_owned_socket("diag"),
            liveness: SpeechLiveness::default(),
            native_monitor: None,
            started_at: Instant::now(),
            config_path: None,
            log_path: Some(log_path.clone()),
            telemetry: RuntimeTelemetry::default(),
        });
        let app = build_router_with_hooks(state.clone(), ControlHooks::default());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let logs: Value = client
            .get(format!("http://127.0.0.1:{port}/logs?lines=5&level=WARN&q=needle_0"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(logs["exists"], true);
        let lines = logs["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 5);
        for l in lines {
            let s = l.as_str().unwrap();
            assert!(
                s.contains("WARN") && s.contains("needle_0") && !s.contains('\x1b'),
                "unexpected line: {s}"
            );
        }

        let doc: Value = client
            .get(format!("http://127.0.0.1:{port}/doctor"))
            .bearer_auth("tok")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc["mode"], "exclusive");
        assert!(doc["warnings"].is_array());
        assert!(doc["upstreams"].is_array());

        handle.abort();
        cleanup(&state);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
