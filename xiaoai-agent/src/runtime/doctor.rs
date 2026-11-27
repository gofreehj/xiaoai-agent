//! Shared runtime diagnostics (`doctor`) used by both CLI (`xiaoai-agent doctor`)
//! and the Web Admin Console (`GET /doctor`).

use std::path::Path;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde::Serialize;

use crate::config::{collect_startup_warnings, AppConfig, AsrProvider};
use crate::frontend::native_monitor::{NativeInterlockStatus, NativeWakeMonitor};

#[derive(Debug, Clone, Serialize)]
pub struct UpstreamProbeResult {
    pub label: String,
    pub url: String,
    pub reachable: bool,
    pub status_code: Option<u16>,
    pub rtt_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DualDoctorInfo {
    pub kws_endpoint: String,
    pub capture_summary: String,
    pub syslog_path: Option<String>,
    pub syslog_exists: bool,
    pub instruction_log_path: Option<String>,
    pub instruction_log_exists: bool,
    pub event_command: String,
    pub interlock: Option<NativeInterlockStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub mode: String,
    pub config_path: Option<String>,
    pub pid_file: Option<String>,
    pub tmp_free_kib: Option<u64>,
    pub data_path: String,
    pub data_free_kib: Option<u64>,
    pub warnings: Vec<String>,
    pub speech_socket: Option<String>,
    pub speech_socket_exists: Option<bool>,
    pub dual: Option<DualDoctorInfo>,
    pub upstreams: Vec<UpstreamProbeResult>,
}

/// Free space of `path` in KiB via `df -k`; best-effort (`None` when `df` is missing or fails).
pub fn path_free_kib(path: &str) -> Option<u64> {
    let out = std::process::Command::new("df")
        .args(["-k", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    line.split_whitespace().nth(3)?.parse().ok()
}

async fn probe_http_upstream(
    client: reqwest::Client,
    label: String,
    url: String,
) -> UpstreamProbeResult {
    let started = Instant::now();
    match client.head(&url).send().await {
        Ok(resp) => UpstreamProbeResult {
            label,
            url,
            reachable: true,
            status_code: Some(resp.status().as_u16()),
            rtt_ms: started.elapsed().as_millis() as u64,
            error: None,
        },
        Err(err) => UpstreamProbeResult {
            label,
            url,
            reachable: false,
            status_code: None,
            rtt_ms: started.elapsed().as_millis() as u64,
            error: Some(err.to_string()),
        },
    }
}

pub async fn collect_doctor_report(
    config: &AppConfig,
    config_path: Option<&Path>,
    native_monitor: Option<&NativeWakeMonitor>,
) -> DoctorReport {
    let warnings = collect_startup_warnings(config);
    let pid_file = std::fs::read_to_string("/tmp/xiaoai-agent/xiaoai-agent.pid")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let tmp_free_kib = path_free_kib("/tmp");
    let data_probe_path = if Path::new("/data").exists() {
        "/data".to_string()
    } else {
        config_path
            .and_then(|p| p.parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string())
    };
    let data_free_kib = path_free_kib(&data_probe_path);

    let (speech_socket, speech_socket_exists, dual) = if config.runtime.mode == "dual" {
        let ni = &config.dual.native_interlock;
        let syslog_exists = ni
            .syslog_path
            .as_ref()
            .is_some_and(|p| p.exists() || (p == Path::new("/tmp/log/messages") && Path::new("/var/log/messages").exists()));
        let inst_exists = ni.instruction_log_path.as_ref().is_some_and(|p| p.exists());
        (
            None,
            None,
            Some(DualDoctorInfo {
                kws_endpoint: config.dual.kws.endpoint.trim().to_string(),
                capture_summary: format!(
                    "device={} rate={}Hz ch={} format={:?} mic_ch={} ref_ch={:?}",
                    config.dual.capture.device,
                    config.dual.capture.sample_rate,
                    config.dual.capture.channels,
                    config.dual.capture.format,
                    config.dual.capture.mic_channel,
                    config.dual.capture.ref_channel
                ),
                syslog_path: ni.syslog_path.as_ref().map(|p| p.display().to_string()),
                syslog_exists,
                instruction_log_path: ni.instruction_log_path.as_ref().map(|p| p.display().to_string()),
                instruction_log_exists: inst_exists,
                event_command: ni.event_command.trim().to_string(),
                interlock: native_monitor.map(NativeWakeMonitor::status),
            }),
        )
    } else {
        let sock = config.runtime.speech_socket.clone();
        let exists = Path::new(&sock).exists();
        (Some(sock), Some(exists), None)
    };

    let mut targets: Vec<(String, String)> = Vec::new();
    let asr_url = match config.asr.provider {
        AsrProvider::OpenAi => config.asr.open_ai.base_url.clone(),
        AsrProvider::OpenAiRealtime => config
            .asr
            .openai_realtime
            .base_url
            .replace("wss://", "https://")
            .replace("ws://", "http://"),
    };
    targets.push((
        "asr".to_string(),
        asr_url.trim_end_matches('/').to_string(),
    ));
    targets.push((
        "llm".to_string(),
        config.llm.base_url.trim_end_matches('/').to_string(),
    ));
    if !config.agent.weather.qweather_url.trim().is_empty() {
        targets.push((
            "weather".to_string(),
            config.agent.weather.qweather_url.trim().to_string(),
        ));
    }
    for srv in config
        .mcp
        .servers
        .iter()
        .filter(|s| s.enabled && !s.url.trim().is_empty())
    {
        targets.push((format!("mcp:{}", srv.name), srv.url.trim().to_string()));
    }
    let ha = &config.mcp.home_assistant;
    if ha.enabled && !ha.url.trim().is_empty() {
        targets.push(("mcp:home_assistant".to_string(), ha.url.trim().to_string()));
    }

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(2_500))
        .timeout(Duration::from_millis(2_500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let http_futs = targets
        .into_iter()
        .map(|(label, url)| probe_http_upstream(client.clone(), label, url));

    let mut upstreams = if config.runtime.mode == "dual" && !config.dual.kws.endpoint.trim().is_empty()
    {
        let ep = config.dual.kws.endpoint.trim().to_string();
        let (http_res, kws_res) = tokio::join!(
            join_all(http_futs),
            crate::frontend::external_kws::probe_external_kws(&ep, Duration::from_secs(2))
        );
        let mut all = http_res;
        all.push(match kws_res {
            Ok(rtt) => UpstreamProbeResult {
                label: "external_kws".to_string(),
                url: ep,
                reachable: true,
                status_code: None,
                rtt_ms: rtt.as_millis() as u64,
                error: None,
            },
            Err(err) => UpstreamProbeResult {
                label: "external_kws".to_string(),
                url: ep,
                reachable: false,
                status_code: None,
                rtt_ms: 0,
                error: Some(err.to_string()),
            },
        });
        all
    } else {
        join_all(http_futs).await
    };

    // Keep ordering predictable (same as CLI doctor order)
    upstreams.retain(|u| !u.url.is_empty());

    DoctorReport {
        mode: config.runtime.mode.clone(),
        config_path: config_path.map(|p| p.display().to_string()),
        pid_file,
        tmp_free_kib,
        data_path: data_probe_path,
        data_free_kib,
        warnings,
        speech_socket,
        speech_socket_exists,
        dual,
        upstreams,
    }
}
