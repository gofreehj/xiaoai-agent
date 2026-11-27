//! Configuration view, secret redaction, validation, atomic save, and `.bak` rollback
//! for the HTTP control plane admin console.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use serde_yaml::{Mapping, Value as YamlValue};

use crate::config::{
    classify_config_change, collect_startup_warnings, AppConfig, AsrProvider, DualCaptureFormat,
    LlmProtocol, LlmThinkingMode, TtsProvider,
};

pub const REDACTED_PLACEHOLDER: &str = "__REDACTED__";

const SECRET_PATHS: &[&[&str]] = &[
    &["asr", "open_ai", "api_key"],
    &["asr", "openai_realtime", "api_key"],
    &["llm", "api_key"],
    &["tts", "openai", "api_key"],
    &["agent", "web_search", "api_key"],
    &["server", "token"],
    &["airplay", "password"],
    &["music", "netease", "password"],
    &["music", "netease", "md5_password"],
    &["music", "navidrome", "password"],
    &["mcp", "home_assistant", "token"],
];

fn is_real_secret(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("EMPTY")
        && trimmed != REDACTED_PLACEHOLDER
}

fn yaml_key(name: &str) -> YamlValue {
    YamlValue::String(name.to_string())
}

fn get_yaml_path<'a>(root: &'a YamlValue, path: &[&str]) -> Option<&'a YamlValue> {
    let mut cur = root;
    for seg in path {
        cur = cur.as_mapping()?.get(yaml_key(seg))?;
    }
    Some(cur)
}

fn get_yaml_path_mut<'a>(root: &'a mut YamlValue, path: &[&str]) -> Option<&'a mut YamlValue> {
    let mut cur = root;
    for seg in path {
        cur = cur.as_mapping_mut()?.get_mut(yaml_key(seg))?;
    }
    Some(cur)
}

fn is_secret_path(path: &[String]) -> bool {
    SECRET_PATHS.iter().any(|sp| {
        sp.len() == path.len() && sp.iter().zip(path.iter()).all(|(a, b)| *a == b.as_str())
    })
}

/// Replace plaintext secret values with `REDACTED_PLACEHOLDER` before returning YAML to the Web UI.
pub fn redact_yaml_value(val: &mut YamlValue) {
    for path in SECRET_PATHS {
        if let Some(YamlValue::String(s)) = get_yaml_path_mut(val, path) {
            if is_real_secret(s) {
                *s = REDACTED_PLACEHOLDER.to_string();
            }
        }
    }

    if let Some(YamlValue::Sequence(servers)) = get_yaml_path_mut(val, &["mcp", "servers"]) {
        for srv in servers {
            if let Some(map) = srv.as_mapping_mut() {
                if let Some(YamlValue::String(tok)) = map.get_mut(yaml_key("token")) {
                    if is_real_secret(tok) {
                        *tok = REDACTED_PLACEHOLDER.to_string();
                    }
                }
            }
        }
    }
}

/// Restore any `REDACTED_PLACEHOLDER` entries in `incoming` from `existing`.
pub fn restore_redacted_secrets(incoming: &mut YamlValue, existing: &YamlValue) {
    for path in SECRET_PATHS {
        if let Some(YamlValue::String(s)) = get_yaml_path_mut(incoming, path) {
            if s == REDACTED_PLACEHOLDER {
                let replacement = get_yaml_path(existing, path)
                    .and_then(YamlValue::as_str)
                    .unwrap_or("")
                    .to_string();
                *s = replacement;
            }
        }
    }

    let existing_servers =
        get_yaml_path(existing, &["mcp", "servers"]).and_then(YamlValue::as_sequence);
    if let Some(YamlValue::Sequence(servers)) = get_yaml_path_mut(incoming, &["mcp", "servers"]) {
        for (idx, srv) in servers.iter_mut().enumerate() {
            let Some(map) = srv.as_mapping_mut() else {
                continue;
            };
            let is_redacted = map
                .get(yaml_key("token"))
                .and_then(YamlValue::as_str)
                .is_some_and(|t| t == REDACTED_PLACEHOLDER);
            if !is_redacted {
                continue;
            }

            let srv_name = map.get(yaml_key("name")).and_then(YamlValue::as_str);
            let matched = existing_servers.and_then(|list| {
                srv_name
                    .and_then(|name| {
                        list.iter().find(|item| {
                            item.as_mapping()
                                .and_then(|m| m.get(yaml_key("name")))
                                .and_then(YamlValue::as_str)
                                == Some(name)
                        })
                    })
                    .or_else(|| list.get(idx))
            });
            let restored = matched
                .and_then(YamlValue::as_mapping)
                .and_then(|m| m.get(yaml_key("token")))
                .and_then(YamlValue::as_str)
                .unwrap_or("")
                .to_string();
            map.insert(yaml_key("token"), YamlValue::String(restored));
        }
    }
}

pub fn backup_path_for(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

/// Return the `/data/open-xiaoai/disabled` style autostart guard path next to `config_path`.
pub fn autostart_disabled_marker(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("disabled")
}

/// Read whether boot autostart is currently enabled (true when the `disabled` marker is absent).
pub fn read_autostart_status(config_path: &Path) -> JsonValue {
    let marker = autostart_disabled_marker(config_path);
    json!({
        "enabled": !marker.exists(),
        "marker_path": marker.display().to_string(),
    })
}

/// Enable or disable boot autostart by removing or creating the `disabled` marker file.
pub fn set_autostart_enabled(config_path: &Path, enabled: bool) -> anyhow::Result<JsonValue> {
    let marker = autostart_disabled_marker(config_path);
    if enabled {
        if let Err(err) = fs::remove_file(&marker) {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }
    } else {
        if let Some(parent) = marker.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&marker)?;
    }
    Ok(read_autostart_status(config_path))
}

fn set_restrictive_perms(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Atomically replace `path` with `content`, preserving one `.bak` copy of the prior file,
/// keeping `0600` permissions on both files.
pub fn atomic_write_with_backup(path: &Path, content: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("agent.yaml");
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));

    let write_res = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        set_restrictive_perms(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);

        if path.exists() {
            let bak = backup_path_for(path);
            fs::copy(path, &bak)?;
            set_restrictive_perms(&bak)?;
        }

        fs::rename(&tmp_path, path)?;
        set_restrictive_perms(path)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

fn protocol_str(p: &LlmProtocol) -> &'static str {
    match p {
        LlmProtocol::OpenAiChat => "open_ai_chat",
        LlmProtocol::Anthropic => "anthropic",
    }
}

fn thinking_mode_str(m: &LlmThinkingMode) -> &'static str {
    match m {
        LlmThinkingMode::Auto => "auto",
        LlmThinkingMode::Disabled => "disabled",
        LlmThinkingMode::Enabled => "enabled",
    }
}

fn tts_provider_str(p: TtsProvider) -> &'static str {
    match p {
        TtsProvider::Native => "native",
        TtsProvider::Doubao => "doubao",
        TtsProvider::OpenAi => "openai",
    }
}

fn asr_provider_str(p: &AsrProvider) -> &'static str {
    match p {
        AsrProvider::OpenAi => "open_ai",
        AsrProvider::OpenAiRealtime => "openai_realtime",
    }
}

fn dual_format_str(f: DualCaptureFormat) -> &'static str {
    match f {
        DualCaptureFormat::S32Le => "S32_LE",
        DualCaptureFormat::S16Le => "S16_LE",
    }
}

fn raw_opt_path_str(raw: &YamlValue, path: &[&str]) -> Option<String> {
    get_yaml_path(raw, path)
        .and_then(YamlValue::as_str)
        .map(|s| s.to_string())
}

pub fn read_config_view(path: &Path, effective: &AppConfig) -> anyhow::Result<JsonValue> {
    let raw = fs::read_to_string(path)?;
    let mut yaml_val: YamlValue = serde_yaml::from_str(&raw)?;
    let disk_cfg = AppConfig::from_yaml_str(&raw, path.parent())?;
    let pending = classify_config_change(effective, &disk_cfg);
    let warnings = collect_startup_warnings(&disk_cfg);

    let servers: Vec<JsonValue> = disk_cfg
        .mcp
        .servers
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "enabled": s.enabled,
                "url": s.url,
                "timeout_s": s.timeout_s,
                "tools": s.tools,
                "token_set": is_real_secret(&s.token),
            })
        })
        .collect();

    let enter_earcon = raw_opt_path_str(&yaml_val, &["dual", "enter_earcon"])
        .or_else(|| disk_cfg.dual.enter_earcon.as_ref().map(|p| p.display().to_string()));
    let exit_earcon = raw_opt_path_str(&yaml_val, &["dual", "exit_earcon"])
        .or_else(|| disk_cfg.dual.exit_earcon.as_ref().map(|p| p.display().to_string()));
    let thinking_earcon = raw_opt_path_str(&yaml_val, &["dual", "thinking_earcon"])
        .or_else(|| disk_cfg.dual.thinking_earcon.as_ref().map(|p| p.display().to_string()));
    let cookie_file = raw_opt_path_str(&yaml_val, &["music", "netease", "cookie_file"])
        .or_else(|| disk_cfg.music.netease.cookie_file.as_ref().map(|p| p.display().to_string()));

    let form = json!({
        "runtime": {
            "mode": disk_cfg.runtime.mode,
            "speech_socket": disk_cfg.runtime.speech_socket,
            "session_idle_timeout_s": disk_cfg.runtime.session_idle_timeout_s,
        },
        "server": {
            "enabled": disk_cfg.server.enabled,
            "host": disk_cfg.server.host,
            "port": disk_cfg.server.port,
            "token_set": is_real_secret(&disk_cfg.server.token),
        },
        "device": {
            "command_timeout_s": disk_cfg.device.command_timeout_s,
            "led_enabled": disk_cfg.device.led_enabled,
            "led_listening": disk_cfg.device.led_listening,
            "led_user_speaking": disk_cfg.device.led_user_speaking,
            "led_thinking": disk_cfg.device.led_thinking,
            "led_speaking": disk_cfg.device.led_speaking,
            "tts_command": disk_cfg.device.tts_command,
            "play_url_command": disk_cfg.device.play_url_command,
            "stop_audio_command": disk_cfg.device.stop_audio_command,
            "pause_audio_command": disk_cfg.device.pause_audio_command,
            "resume_audio_command": disk_cfg.device.resume_audio_command,
            "duck_audio_command": disk_cfg.device.duck_audio_command,
            "unduck_audio_command": disk_cfg.device.unduck_audio_command,
            "abort_command": disk_cfg.device.abort_command,
            "volume_up_command": disk_cfg.device.volume_up_command,
            "volume_down_command": disk_cfg.device.volume_down_command,
            "volume_set_command": disk_cfg.device.volume_set_command,
            "volume_mute_command": disk_cfg.device.volume_mute_command,
        },
        "asr": {
            "provider": asr_provider_str(&disk_cfg.asr.provider),
            "open_ai": {
                "base_url": disk_cfg.asr.open_ai.base_url,
                "model": disk_cfg.asr.open_ai.model,
                "language": disk_cfg.asr.open_ai.language,
                "prompt": disk_cfg.asr.open_ai.prompt,
                "timeout_s": disk_cfg.asr.open_ai.timeout_s,
                "retries": disk_cfg.asr.open_ai.retries,
                "api_key_set": is_real_secret(&disk_cfg.asr.open_ai.api_key),
            },
            "openai_realtime": {
                "base_url": disk_cfg.asr.openai_realtime.base_url,
                "model": disk_cfg.asr.openai_realtime.model,
                "target_sample_rate": disk_cfg.asr.openai_realtime.target_sample_rate,
                "chunk_ms": disk_cfg.asr.openai_realtime.chunk_ms,
                "timeout_s": disk_cfg.asr.openai_realtime.timeout_s,
                "retries": disk_cfg.asr.openai_realtime.retries,
                "api_key_set": is_real_secret(&disk_cfg.asr.openai_realtime.api_key),
                "server_vad": {
                    "enabled": disk_cfg.asr.openai_realtime.server_vad.enabled,
                    "prefix_padding_ms": disk_cfg.asr.openai_realtime.server_vad.prefix_padding_ms,
                    "silence_duration_ms": disk_cfg.asr.openai_realtime.server_vad.silence_duration_ms,
                    "threshold": disk_cfg.asr.openai_realtime.server_vad.threshold,
                },
            },
        },
        "llm": {
            "protocol": protocol_str(&disk_cfg.llm.protocol),
            "base_url": disk_cfg.llm.base_url,
            "model": disk_cfg.llm.model,
            "temperature": disk_cfg.llm.temperature,
            "max_tokens": disk_cfg.llm.max_tokens,
            "timeout_s": disk_cfg.llm.timeout_s,
            "thinking_mode": thinking_mode_str(&disk_cfg.llm.thinking.mode),
            "thinking_budget_tokens": disk_cfg.llm.thinking.budget_tokens,
            "api_key_set": is_real_secret(&disk_cfg.llm.api_key),
        },
        "agent": {
            "timezone": disk_cfg.agent.timezone,
            "tools_enabled": disk_cfg.agent.tools_enabled,
            "inject_current_time": disk_cfg.agent.inject_current_time,
            "weather_default_location": disk_cfg.agent.weather.default_location,
            "weather_qweather_url": disk_cfg.agent.weather.qweather_url,
            "weather_ip_lookup_url": disk_cfg.agent.weather.ip_lookup_url,
            "weather_timeout_s": disk_cfg.agent.weather.timeout_s,
            "web_search_enabled": disk_cfg.agent.web_search.enabled,
            "web_search_tavily_url": disk_cfg.agent.web_search.tavily_url,
            "web_search_max_results": disk_cfg.agent.web_search.max_results,
            "web_search_search_depth": disk_cfg.agent.web_search.search_depth,
            "web_search_timeout_s": disk_cfg.agent.web_search.timeout_s,
            "web_search_api_key_set": is_real_secret(&disk_cfg.agent.web_search.api_key),
        },
        "tts": {
            "provider": tts_provider_str(disk_cfg.tts.provider),
            "fallback_to_native": disk_cfg.tts.fallback_to_native,
            "playback_command": disk_cfg.tts.playback_command,
            "suppress_capture_while_playing": disk_cfg.tts.suppress_capture_while_playing,
            "playback_tail_ms": disk_cfg.tts.playback_tail_ms,
            "playback_startup_grace_ms": disk_cfg.tts.playback_startup_grace_ms,
            "openai_base_url": disk_cfg.tts.openai.base_url,
            "openai_model": disk_cfg.tts.openai.model,
            "openai_voice": disk_cfg.tts.openai.voice,
            "openai_response_format": disk_cfg.tts.openai.response_format,
            "openai_speed": disk_cfg.tts.openai.speed,
            "openai_sample_rate": disk_cfg.tts.openai.sample_rate,
            "openai_instructions": disk_cfg.tts.openai.instructions,
            "openai_timeout_s": disk_cfg.tts.openai.timeout_s,
            "openai_output_dir": disk_cfg.tts.openai.output_dir.as_ref().map(|p| p.display().to_string()),
            "openai_api_key_set": is_real_secret(&disk_cfg.tts.openai.api_key),
        },
        "music": {
            "enabled": disk_cfg.music.enabled,
            "provider": disk_cfg.music.provider,
            "interruption_mode": disk_cfg.music.interruption.mode,
            "navidrome": {
                "base_url": disk_cfg.music.navidrome.base_url,
                "username": disk_cfg.music.navidrome.username,
                "api_version": disk_cfg.music.navidrome.api_version,
                "timeout_s": disk_cfg.music.navidrome.timeout_s,
                "password_set": is_real_secret(&disk_cfg.music.navidrome.password),
            },
            "netease": {
                "api_base_url": disk_cfg.music.netease.api_base_url,
                "login_mode": disk_cfg.music.netease.login_mode,
                "account": disk_cfg.music.netease.account,
                "phone": disk_cfg.music.netease.phone,
                "cookie_file": cookie_file,
                "default_level": disk_cfg.music.netease.default_level,
                "timeout_s": disk_cfg.music.netease.timeout_s,
                "login_on_start": disk_cfg.music.netease.login_on_start,
                "password_set": is_real_secret(&disk_cfg.music.netease.password),
                "md5_password_set": is_real_secret(&disk_cfg.music.netease.md5_password),
            },
        },
        "airplay": {
            "enabled": disk_cfg.airplay.enabled,
            "name": disk_cfg.airplay.name,
            "port": disk_cfg.airplay.port,
            "hwaddr": disk_cfg.airplay.hwaddr,
            "password_set": is_real_secret(&disk_cfg.airplay.password),
            "output_backend": disk_cfg.airplay.output.backend,
            "output_aplay_path": disk_cfg.airplay.output.aplay_path,
            "output_device": disk_cfg.airplay.output.device,
            "output_format": disk_cfg.airplay.output.format,
            "interruption_mode": disk_cfg.airplay.interruption.mode,
            "interruption_duck_gain": disk_cfg.airplay.interruption.duck_gain,
        },
        "dual": {
            "enter_prompt": disk_cfg.dual.enter_prompt,
            "exit_prompt": disk_cfg.dual.exit_prompt,
            "enter_earcon": enter_earcon,
            "exit_earcon": exit_earcon,
            "thinking_earcon": thinking_earcon,
            "session_timeout_s": disk_cfg.dual.session_timeout_s,
            "exit_keywords": disk_cfg.dual.exit_keywords,
            "kws": {
                "endpoint": disk_cfg.dual.kws.endpoint,
                "ping_interval_s": disk_cfg.dual.kws.ping_interval_s,
            },
            "capture": {
                "device": disk_cfg.dual.capture.device,
                "sample_rate": disk_cfg.dual.capture.sample_rate,
                "channels": disk_cfg.dual.capture.channels,
                "format": dual_format_str(disk_cfg.dual.capture.format),
                "mic_channel": disk_cfg.dual.capture.mic_channel,
                "ref_channel": disk_cfg.dual.capture.ref_channel,
                "shift_bits": disk_cfg.dual.capture.shift_bits,
            },
            "native_interlock": {
                "enabled": disk_cfg.dual.native_interlock.enabled,
                "event_command": disk_cfg.dual.native_interlock.event_command,
                "syslog_path": disk_cfg.dual.native_interlock.syslog_path.as_ref().map(|p| p.display().to_string()),
                "instruction_log_path": disk_cfg.dual.native_interlock.instruction_log_path.as_ref().map(|p| p.display().to_string()),
                "wake_patterns": disk_cfg.dual.native_interlock.wake_patterns,
                "end_patterns": disk_cfg.dual.native_interlock.end_patterns,
                "instruction_patterns": disk_cfg.dual.native_interlock.instruction_patterns,
                "max_active_s": disk_cfg.dual.native_interlock.max_active_s,
                "cooldown_s": disk_cfg.dual.native_interlock.cooldown_s,
                "poll_interval_ms": disk_cfg.dual.native_interlock.poll_interval_ms,
            },
        },
        "capture": {
            "sample_rate": disk_cfg.capture.sample_rate,
            "threshold": disk_cfg.capture.threshold,
            "mic_gain": disk_cfg.capture.mic_gain,
            "block_ms": disk_cfg.capture.block_ms,
            "pre_roll_ms": disk_cfg.capture.pre_roll_ms,
            "silence_ms": disk_cfg.capture.silence_ms,
            "min_speech_ms": disk_cfg.capture.min_speech_ms,
            "max_utterance_s": disk_cfg.capture.max_utterance_s,
            "cooldown_s": disk_cfg.capture.cooldown_s,
            "print_levels": disk_cfg.capture.print_levels,
            "adaptive_noise_floor": disk_cfg.capture.adaptive_noise_floor,
            "noise_floor_multiplier": disk_cfg.capture.noise_floor_multiplier,
            "dynamic_silence": disk_cfg.capture.dynamic_silence,
            "short_utterance_silence_ms": disk_cfg.capture.short_utterance_silence_ms,
            "long_utterance_silence_ms": disk_cfg.capture.long_utterance_silence_ms,
        },
        "mcp": {
            "home_assistant": {
                "enabled": disk_cfg.mcp.home_assistant.enabled,
                "url": disk_cfg.mcp.home_assistant.url,
                "timeout_s": disk_cfg.mcp.home_assistant.timeout_s,
                "token_set": is_real_secret(&disk_cfg.mcp.home_assistant.token),
            },
            "servers": servers,
        },
    });

    redact_yaml_value(&mut yaml_val);
    let redacted_yaml = serde_yaml::to_string(&yaml_val)?;

    Ok(json!({
        "path": path.display().to_string(),
        "has_backup": backup_path_for(path).exists(),
        "autostart": read_autostart_status(path),
        "yaml": redacted_yaml,
        "form": form,
        "warnings": warnings,
        "pending_restart": pending.needs_restart,
        "rejected": pending.rejected,
    }))
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigSaveOutcome {
    pub applied: Vec<&'static str>,
    pub needs_restart: Vec<&'static str>,
    pub rejected: Vec<&'static str>,
    pub warnings: Vec<String>,
    pub should_reload: bool,
    pub has_backup: bool,
}

fn json_to_yaml(val: &JsonValue) -> YamlValue {
    match val {
        JsonValue::Null => YamlValue::Null,
        JsonValue::Bool(b) => YamlValue::Bool(*b),
        JsonValue::Number(n) => serde_yaml::to_value(n).unwrap_or(YamlValue::Null),
        JsonValue::String(s) => YamlValue::String(s.trim().to_string()),
        JsonValue::Array(arr) => YamlValue::Sequence(arr.iter().map(json_to_yaml).collect()),
        JsonValue::Object(obj) => {
            let mut map = Mapping::new();
            for (k, v) in obj {
                map.insert(yaml_key(k), json_to_yaml(v));
            }
            YamlValue::Mapping(map)
        }
    }
}

fn merge_mcp_servers(existing_doc: &YamlValue, incoming: &[JsonValue]) -> YamlValue {
    let existing_tokens: Vec<(String, String)> =
        get_yaml_path(existing_doc, &["mcp", "servers"])
            .and_then(YamlValue::as_sequence)
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| {
                        let m = v.as_mapping()?;
                        let name = m.get(yaml_key("name")).and_then(YamlValue::as_str)?;
                        let tok = m
                            .get(yaml_key("token"))
                            .and_then(YamlValue::as_str)
                            .unwrap_or("");
                        Some((name.to_string(), tok.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

    let mut out_seq = Vec::with_capacity(incoming.len());
    for srv in incoming {
        let Some(obj) = srv.as_object() else {
            continue;
        };
        let name = obj
            .get("name")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let mut m = Mapping::new();
        m.insert(yaml_key("name"), YamlValue::String(name.to_string()));
        m.insert(
            yaml_key("enabled"),
            YamlValue::Bool(obj.get("enabled").and_then(JsonValue::as_bool).unwrap_or(false)),
        );
        m.insert(
            yaml_key("url"),
            YamlValue::String(
                obj.get("url")
                    .and_then(JsonValue::as_str)
                    .map(str::trim)
                    .unwrap_or("")
                    .to_string(),
            ),
        );
        let timeout_s = obj
            .get("timeout_s")
            .and_then(JsonValue::as_f64)
            .unwrap_or(10.0);
        m.insert(
            yaml_key("timeout_s"),
            serde_yaml::to_value(timeout_s).unwrap_or(YamlValue::Null),
        );
        let tools = obj
            .get("tools")
            .map(json_to_yaml)
            .unwrap_or_else(|| YamlValue::Sequence(Vec::new()));
        m.insert(yaml_key("tools"), tools);

        let given_tok = obj
            .get("token")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != REDACTED_PLACEHOLDER);
        let final_tok = given_tok.map(str::to_string).unwrap_or_else(|| {
            existing_tokens
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t.clone())
                .unwrap_or_default()
        });
        m.insert(yaml_key("token"), YamlValue::String(final_tok));
        out_seq.push(YamlValue::Mapping(m));
    }
    YamlValue::Sequence(out_seq)
}

fn merge_json_into_yaml(
    doc: &mut YamlValue,
    patch: &JsonValue,
    path: &mut Vec<String>,
    root_snapshot: &YamlValue,
) {
    let Some(obj) = patch.as_object() else {
        return;
    };
    if !doc.is_mapping() {
        *doc = YamlValue::Mapping(Mapping::new());
    }
    let map = doc.as_mapping_mut().expect("ensured mapping");

    for (key, val) in obj {
        path.push(key.clone());
        let is_speech_sock =
            path.len() == 2 && path[0] == "runtime" && path[1] == "speech_socket";
        if is_speech_sock {
            path.pop();
            continue;
        }

        if path.len() == 2 && path[0] == "mcp" && path[1] == "servers" {
            if let Some(arr) = val.as_array() {
                map.insert(yaml_key(key), merge_mcp_servers(root_snapshot, arr));
            }
            path.pop();
            continue;
        }

        if is_secret_path(path) {
            let keep_existing = match val {
                JsonValue::Null => true,
                JsonValue::String(s) => {
                    let t = s.trim();
                    t.is_empty() || t == REDACTED_PLACEHOLDER
                }
                _ => false,
            };
            if keep_existing {
                path.pop();
                continue;
            }
        }

        if val.is_object() {
            let yk = yaml_key(key);
            if !map.contains_key(&yk) || !map.get(&yk).is_some_and(YamlValue::is_mapping) {
                map.insert(yk.clone(), YamlValue::Mapping(Mapping::new()));
            }
            if let Some(child) = map.get_mut(&yk) {
                merge_json_into_yaml(child, val, path, root_snapshot);
            }
        } else {
            map.insert(yaml_key(key), json_to_yaml(val));
        }
        path.pop();
    }
}

/// Deep-merge a JSON form patch aligned with `agent.yaml` nesting into `doc`,
/// keeping `runtime.speech_socket`, empty secret inputs, and existing MCP server tokens intact.
pub fn apply_form_patch(doc: &mut YamlValue, patch: &JsonValue) {
    let snapshot = doc.clone();
    let mut path = Vec::new();
    merge_json_into_yaml(doc, patch, &mut path, &snapshot);
}

fn finalize_and_maybe_write(
    path: &Path,
    final_yaml_val: &YamlValue,
    effective: &AppConfig,
    validate_only: bool,
) -> Result<ConfigSaveOutcome, String> {
    let serialized =
        serde_yaml::to_string(final_yaml_val).map_err(|e| format!("序列化 YAML 失败: {e}"))?;
    let next_cfg = AppConfig::from_yaml_str(&serialized, path.parent())
        .map_err(|e| format!("配置校验失败: {e}"))?;

    let changes = classify_config_change(effective, &next_cfg);
    let warnings = collect_startup_warnings(&next_cfg);
    let should_reload = !validate_only && changes.should_apply();

    if !validate_only {
        atomic_write_with_backup(path, &serialized)
            .map_err(|e| format!("写入配置文件失败: {e}"))?;
    }

    Ok(ConfigSaveOutcome {
        applied: changes.applied,
        needs_restart: changes.needs_restart,
        rejected: changes.rejected,
        warnings,
        should_reload,
        has_backup: backup_path_for(path).exists(),
    })
}

pub fn validate_and_save_yaml(
    path: &Path,
    raw_yaml: &str,
    effective: &AppConfig,
    validate_only: bool,
) -> Result<ConfigSaveOutcome, String> {
    let mut incoming: YamlValue =
        serde_yaml::from_str(raw_yaml).map_err(|e| format!("YAML 语法解析失败: {e}"))?;
    if !incoming.is_mapping() {
        return Err("YAML 根节点必须是对象映射".to_string());
    }

    let existing_raw = fs::read_to_string(path).unwrap_or_default();
    let existing: YamlValue =
        serde_yaml::from_str(&existing_raw).unwrap_or(YamlValue::Mapping(Mapping::new()));
    restore_redacted_secrets(&mut incoming, &existing);

    finalize_and_maybe_write(path, &incoming, effective, validate_only)
}

pub fn validate_and_save_form(
    path: &Path,
    patch: &JsonValue,
    effective: &AppConfig,
) -> Result<ConfigSaveOutcome, String> {
    if !patch.is_object() {
        return Err("表单请求必须是 JSON 对象".to_string());
    }
    let existing_raw =
        fs::read_to_string(path).map_err(|e| format!("读取当前配置文件失败: {e}"))?;
    let mut doc: YamlValue =
        serde_yaml::from_str(&existing_raw).map_err(|e| format!("解析当前配置文件失败: {e}"))?;
    apply_form_patch(&mut doc, patch);

    finalize_and_maybe_write(path, &doc, effective, false)
}

pub fn rollback_from_backup(
    path: &Path,
    effective: &AppConfig,
) -> Result<ConfigSaveOutcome, String> {
    let bak = backup_path_for(path);
    if !bak.exists() {
        return Err("没有可回滚的备份文件（agent.yaml.bak 不存在）".to_string());
    }
    let bak_raw = fs::read_to_string(&bak).map_err(|e| format!("读取备份文件失败: {e}"))?;
    let next_cfg = AppConfig::from_yaml_str(&bak_raw, path.parent())
        .map_err(|e| format!("备份文件校验失败，已中止回滚: {e}"))?;

    let changes = classify_config_change(effective, &next_cfg);
    let warnings = collect_startup_warnings(&next_cfg);
    let should_reload = changes.should_apply();

    atomic_write_with_backup(path, &bak_raw).map_err(|e| format!("写回备份配置失败: {e}"))?;

    Ok(ConfigSaveOutcome {
        applied: changes.applied,
        needs_restart: changes.needs_restart,
        rejected: changes.rejected,
        warnings,
        should_reload,
        has_backup: backup_path_for(path).exists(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "xiaoai-config-admin-{tag}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_admin_redacts_all_secrets_and_preserves_them_on_roundtrip() {
        let dir = temp_dir("redact");
        let path = dir.join("agent.yaml");
        let original = r#"
runtime:
  mode: dual
asr:
  open_ai:
    api_key: asr-secret-1
  openai_realtime:
    api_key: asr-rt-secret
llm:
  api_key: llm-secret-key
  model: gpt-5.4-mini
tts:
  openai:
    api_key: tts-secret-key
agent:
  web_search:
    api_key: tavily-secret
server:
  enabled: true
  host: 0.0.0.0
  token: panel-token-xyz
airplay:
  password: airplay-pw
music:
  netease:
    password: ne-pw
    md5_password: ne-md5
  navidrome:
    password: navi-pw
mcp:
  home_assistant:
    token: ha-token-123
  servers:
    - name: hass
      enabled: true
      url: http://127.0.0.1:8123/mcp
      token: mcp-srv-token
"#;
        fs::write(&path, original).unwrap();
        let effective = AppConfig::load(&path).unwrap();

        let view = read_config_view(&path, &effective).unwrap();
        let redacted_yaml = view["yaml"].as_str().unwrap();

        for secret in [
            "asr-secret-1",
            "asr-rt-secret",
            "llm-secret-key",
            "tts-secret-key",
            "tavily-secret",
            "panel-token-xyz",
            "airplay-pw",
            "ne-pw",
            "ne-md5",
            "navi-pw",
            "ha-token-123",
            "mcp-srv-token",
        ] {
            assert!(
                !redacted_yaml.contains(secret),
                "脱敏后的 YAML 绝不能泄露明文密钥 {secret}: {redacted_yaml}"
            );
        }
        assert!(redacted_yaml.contains(REDACTED_PLACEHOLDER));
        assert_eq!(view["form"]["llm"]["api_key_set"], true);

        let edited_yaml = redacted_yaml.replace("gpt-5.4-mini", "gpt-5.4-nano");
        let outcome = validate_and_save_yaml(&path, &edited_yaml, &effective, false).unwrap();
        assert!(outcome.applied.contains(&"llm"));
        assert!(outcome.should_reload);

        let saved_cfg = AppConfig::load(&path).unwrap();
        assert_eq!(saved_cfg.llm.model, "gpt-5.4-nano");
        assert_eq!(saved_cfg.llm.api_key, "llm-secret-key");
        assert_eq!(saved_cfg.asr.open_ai.api_key, "asr-secret-1");
        assert_eq!(saved_cfg.asr.openai_realtime.api_key, "asr-rt-secret");
        assert_eq!(saved_cfg.tts.openai.api_key, "tts-secret-key");
        assert_eq!(saved_cfg.agent.web_search.api_key, "tavily-secret");
        assert_eq!(saved_cfg.server.token, "panel-token-xyz");
        assert_eq!(saved_cfg.airplay.password, "airplay-pw");
        assert_eq!(saved_cfg.music.netease.password, "ne-pw");
        assert_eq!(saved_cfg.music.netease.md5_password, "ne-md5");
        assert_eq!(saved_cfg.music.navidrome.password, "navi-pw");
        assert_eq!(saved_cfg.mcp.home_assistant.token, "ha-token-123");
        assert_eq!(saved_cfg.mcp.servers[0].token, "mcp-srv-token");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_form_and_yaml_save_atomically_create_0600_backup_and_rollback() {
        let dir = temp_dir("rollback");
        let path = dir.join("agent.yaml");
        fs::write(
            &path,
            "llm:\n  model: model-v1\n  api_key: keep-secret\ntts:\n  provider: native\n",
        )
        .unwrap();
        let effective = AppConfig::load(&path).unwrap();

        let patch = json!({
            "llm": {
                "protocol": "open_ai_chat",
                "model": "model-v2",
                "api_key": "",
            }
        });
        let save_res = validate_and_save_form(&path, &patch, &effective).unwrap();
        assert!(save_res.has_backup);
        assert!(save_res.applied.contains(&"llm"));
        assert_eq!(AppConfig::load(&path).unwrap().llm.model, "model-v2");
        assert_eq!(AppConfig::load(&path).unwrap().llm.api_key, "keep-secret");

        let bak = backup_path_for(&path);
        assert!(bak.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let b_mode = fs::metadata(&bak).unwrap().permissions().mode() & 0o777;
            assert_eq!(p_mode, 0o600);
            assert_eq!(b_mode, 0o600);
        }

        let err = validate_and_save_yaml(&path, "llm: [\nbad_yaml", &effective, false).unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(AppConfig::load(&path).unwrap().llm.model, "model-v2");

        let after_v2 = AppConfig::load(&path).unwrap();
        let rb = rollback_from_backup(&path, &after_v2).unwrap();
        assert!(rb.should_reload);
        assert_eq!(AppConfig::load(&path).unwrap().llm.model, "model-v1");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_form_roundtrips_all_sections_and_classifies_restart_vs_hot() {
        let dir = temp_dir("all-sections");
        let path = dir.join("agent.yaml");
        fs::write(
            &path,
            r#"
runtime:
  mode: dual
  speech_socket: /tmp/keep/speech.usock
server:
  enabled: true
  host: 127.0.0.1
  port: 8765
  token: srv-secret
asr:
  provider: open_ai
  open_ai:
    api_key: asr-secret
    model: whisper-1
dual:
  kws:
    endpoint: "192.168.31.1:5005"
music:
  enabled: true
  provider: navidrome
  navidrome:
    password: navi-secret
airplay:
  enabled: false
  password: ap-secret
mcp:
  servers:
    - name: ha
      enabled: false
      url: http://127.0.0.1:8123/api/mcp
      token: mcp-secret
"#,
        )
        .unwrap();
        let effective = AppConfig::load(&path).unwrap();

        let patch = json!({
            "runtime": { "speech_socket": "/tmp/hacked.sock" },
            "asr": {
                "provider": "openai_realtime",
                "open_ai": { "model": "whisper-large", "api_key": "" }
            },
            "dual": {
                "kws": { "endpoint": "10.0.0.5:5005", "ping_interval_s": 15 },
                "capture": { "ref_channel": null },
                "native_interlock": { "syslog_path": null, "cooldown_s": 7 }
            },
            "music": {
                "interruption": { "mode": "duck" },
                "navidrome": { "base_url": "http://192.168.1.2:4533", "password": "" }
            },
            "airplay": { "enabled": true, "port": 5001, "password": "" },
            "mcp": {
                "servers": [{
                    "name": "ha",
                    "enabled": true,
                    "url": "http://127.0.0.1:8123/api/mcp",
                    "timeout_s": 12.0,
                    "tools": ["light.turn_on"],
                    "token": ""
                }]
            }
        });

        let out = validate_and_save_form(&path, &patch, &effective).unwrap();
        assert!(out.applied.contains(&"music"));
        assert!(out.applied.contains(&"mcp"));
        assert!(out.needs_restart.contains(&"asr"));
        assert!(out.needs_restart.contains(&"dual.kws"));
        assert!(out.needs_restart.contains(&"dual.capture"));
        assert!(out.needs_restart.contains(&"dual.native_interlock"));
        assert!(out.needs_restart.contains(&"airplay"));

        let saved = AppConfig::load(&path).unwrap();
        assert_eq!(saved.runtime.speech_socket, "/tmp/keep/speech.usock");
        assert_eq!(saved.asr.open_ai.api_key, "asr-secret");
        assert_eq!(saved.asr.open_ai.model, "whisper-large");
        assert_eq!(saved.dual.kws.endpoint, "10.0.0.5:5005");
        assert_eq!(saved.dual.capture.ref_channel, None);
        assert_eq!(saved.dual.native_interlock.syslog_path, None);
        assert_eq!(saved.music.interruption.mode, "duck");
        assert_eq!(saved.music.navidrome.password, "navi-secret");
        assert_eq!(saved.airplay.password, "ap-secret");
        assert_eq!(saved.server.token, "srv-secret");
        assert_eq!(saved.mcp.servers[0].token, "mcp-secret");
        assert_eq!(saved.mcp.servers[0].tools, vec!["light.turn_on".to_string()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn autostart_marker_toggles_cleanly_and_reports_in_config_view() {
        let dir = temp_dir("autostart");
        let path = dir.join("agent.yaml");
        fs::write(&path, "runtime:\n  mode: dual\n").unwrap();
        let effective = AppConfig::load(&path).unwrap();

        let initial = read_config_view(&path, &effective).unwrap();
        assert_eq!(initial["autostart"]["enabled"], true);

        let off = set_autostart_enabled(&path, false).unwrap();
        assert_eq!(off["enabled"], false);
        assert!(dir.join("disabled").exists());
        assert_eq!(
            read_config_view(&path, &effective).unwrap()["autostart"]["enabled"],
            false
        );

        let on = set_autostart_enabled(&path, true).unwrap();
        assert_eq!(on["enabled"], true);
        assert!(!dir.join("disabled").exists());
        assert_eq!(
            read_config_view(&path, &effective).unwrap()["autostart"]["enabled"],
            true
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
