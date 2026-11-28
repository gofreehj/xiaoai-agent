//! Shared runtime diagnostics (`doctor`) used by both CLI (`xiaoai-agent doctor`)
//! and the Web Admin Console (`GET /doctor`).

use std::path::Path;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde::Serialize;

use crate::config::{
    collect_startup_warnings, AppConfig, AsrProvider, DialogueEngine, RealtimeProvider,
};
use crate::frontend::native_monitor::{NativeInterlockStatus, NativeWakeMonitor};

const DATA_LOW_SPACE_KIB: u64 = 2048;

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
    pub dialogue_engine: String,
    pub realtime_provider: String,
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

fn dialogue_engine_label(engine: DialogueEngine) -> &'static str {
    match engine {
        DialogueEngine::Cascade => "cascade",
        DialogueEngine::Realtime => "realtime",
    }
}

fn realtime_provider_label(provider: RealtimeProvider) -> &'static str {
    match provider {
        RealtimeProvider::VolcengineO2 => "volcengine_o2",
        RealtimeProvider::OpenAiRealtime => "openai_realtime",
    }
}

fn check_data_space_warning(
    warnings: &mut Vec<String>,
    data_probe_path: &str,
    data_free_kib: Option<u64>,
) {
    if let Some(kib) = data_free_kib {
        if kib < DATA_LOW_SPACE_KIB {
            warnings.push(format!(
                "{data_probe_path} 剩余空间不足 2 MiB（当前 {kib} KiB），LX06 更新/运行可能受限"
            ));
        }
    }
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

pub async fn probe_ws_upstream(label: &str, ws_url: &str) -> UpstreamProbeResult {
    let trimmed = ws_url.trim();
    let started = Instant::now();
    let parsed = match url::Url::parse(trimmed) {
        Ok(u) => u,
        Err(err) => {
            return UpstreamProbeResult {
                label: label.to_string(),
                url: trimmed.to_string(),
                reachable: false,
                status_code: None,
                rtt_ms: 0,
                error: Some(format!("invalid WebSocket URL: {err}")),
            };
        }
    };
    let Some(host) = parsed.host_str() else {
        return UpstreamProbeResult {
            label: label.to_string(),
            url: trimmed.to_string(),
            reachable: false,
            status_code: None,
            rtt_ms: 0,
            error: Some("missing WebSocket host".to_string()),
        };
    };
    let Some(port) = parsed.port_or_known_default() else {
        return UpstreamProbeResult {
            label: label.to_string(),
            url: trimmed.to_string(),
            reachable: false,
            status_code: None,
            rtt_ms: 0,
            error: Some("unknown WebSocket port".to_string()),
        };
    };
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(
        Duration::from_millis(2_500),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_stream)) => UpstreamProbeResult {
            label: label.to_string(),
            url: trimmed.to_string(),
            reachable: true,
            status_code: None,
            rtt_ms: started.elapsed().as_millis() as u64,
            error: None,
        },
        Ok(Err(err)) => UpstreamProbeResult {
            label: label.to_string(),
            url: trimmed.to_string(),
            reachable: false,
            status_code: None,
            rtt_ms: started.elapsed().as_millis() as u64,
            error: Some(err.to_string()),
        },
        Err(_) => UpstreamProbeResult {
            label: label.to_string(),
            url: trimmed.to_string(),
            reachable: false,
            status_code: None,
            rtt_ms: started.elapsed().as_millis() as u64,
            error: Some("TCP connect timeout".to_string()),
        },
    }
}

pub async fn collect_doctor_report(
    config: &AppConfig,
    config_path: Option<&Path>,
    native_monitor: Option<&NativeWakeMonitor>,
) -> DoctorReport {
    let mut warnings = collect_startup_warnings(config);
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
    check_data_space_warning(&mut warnings, &data_probe_path, data_free_kib);

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

    let realtime_ws_url = if config.dialogue.engine == DialogueEngine::Realtime {
        match config.realtime.provider {
            RealtimeProvider::VolcengineO2 => Some(config.realtime.volcengine_o2.ws_url.trim()),
            RealtimeProvider::OpenAiRealtime => Some(config.realtime.openai_realtime.ws_url.trim()),
        }
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
    } else {
        None
    };

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

    if let Some(ws_url) = realtime_ws_url {
        upstreams.push(probe_ws_upstream("realtime_ws", &ws_url).await);
    }

    // Keep ordering predictable (same as CLI doctor order)
    upstreams.retain(|u| !u.url.is_empty());

    DoctorReport {
        mode: config.runtime.mode.clone(),
        dialogue_engine: dialogue_engine_label(config.dialogue.engine).to_string(),
        realtime_provider: realtime_provider_label(config.realtime.provider).to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn doctor_probes_realtime_ws_and_flags_low_data_space() {
        let mut warnings = Vec::new();
        check_data_space_warning(&mut warnings, "/data", Some(1500));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("2 MiB"));

        let mut warnings_ok = Vec::new();
        check_data_space_warning(&mut warnings_ok, "/data", Some(4096));
        assert!(warnings_ok.is_empty());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let ok_probe = probe_ws_upstream("realtime_ws", &format!("ws://{addr}/v3")).await;
        assert!(ok_probe.reachable);
        assert_eq!(ok_probe.label, "realtime_ws");
        accept_task.await.unwrap();

        let bad_probe = probe_ws_upstream("realtime_ws", "not-a-ws-url").await;
        assert!(!bad_probe.reachable);
        assert!(bad_probe.error.is_some());
    }
}
