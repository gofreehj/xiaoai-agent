use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Context;
use rand::RngCore;
use reqwest::header::RANGE;
use reqwest::redirect::Policy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::warn;
use url::Url;

use crate::config::{require_music_enabled, timeout_duration, AppConfig, ConfigHandle};
use crate::device::Device;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub fee: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct QueueItem {
    song: Song,
    url: String,
}

#[derive(Debug, Default)]
struct MusicState {
    current: Option<QueueItem>,
    queue: Vec<QueueItem>,
    history: Vec<QueueItem>,
    paused: bool,
    interruption: Option<InterruptionAction>,
    /// 标记本轮语音交互期间是否新启动了曲目播放（用于外部 TTS 播完回复后补拉起音乐流）。
    started_in_turn: bool,
    /// 单调递增的播放代际号：每次切歌/新播/停止时 +1，供结束守望任务过滤过期的 `EndReached`。
    play_gen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptionAction {
    Pause,
    Duck,
    None,
}

pub struct MusicService {
    /// 可整体替换的配置句柄（SIGHUP 热加载）。音乐段的字段都是「用的时候才读」，
    /// 所以换掉句柄里的快照即可生效，不需要重建服务。
    config: ConfigHandle,
    device: Device,
    client: Client,
    no_redirect_client: Client,
    state: Mutex<MusicState>,
    netease_cookie: Mutex<Option<String>>,
}

impl MusicService {
    pub fn new(config: ConfigHandle, device: Device) -> anyhow::Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let no_redirect_client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            config,
            device,
            client,
            no_redirect_client,
            state: Mutex::new(MusicState::default()),
            netease_cookie: Mutex::new(None),
        })
    }

    pub async fn is_actively_playing(&self) -> bool {
        let state = self.state.lock().await;
        state.current.is_some() && !state.paused
    }

    pub async fn search(&self, query: &str, limit: usize) -> Value {
        match self.search_inner(query.trim(), limit.clamp(1, 20)).await {
            Ok(songs) => json!(songs),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn request_login_code(&self) -> Value {
        let snapshot = self.config.get().await;
        match self
            .netease_request(
                "/captcha/sent",
                &[("phone", Self::netease_phone(&snapshot))],
            )
            .await
        {
            Ok(data) => data,
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn submit_login_code(&self, code: &str) -> Value {
        let snapshot = self.config.get().await;
        let phone = Self::netease_phone(&snapshot);
        let verified = self
            .netease_request(
                "/captcha/verify",
                &[("phone", phone.clone()), ("captcha", code.to_string())],
            )
            .await;
        if let Err(err) = verified {
            return json!({"error": err.to_string(), "verified": false});
        }
        let data = self
            .netease_request(
                "/login/cellphone",
                &[("phone", phone), ("captcha", code.to_string())],
            )
            .await;
        match data {
            Ok(data) => {
                self.remember_cookie_from_response(&data).await;
                data
            }
            Err(err) => json!({"error": err.to_string(), "verified": true}),
        }
    }

    pub async fn play_query(&self, query: &str, song_id: &str) -> Value {
        match self.resolve_one(query, song_id).await {
            Ok(item) => {
                let play = self.device.play_url(&item.url).await;
                match play {
                    Ok(()) => {
                        let mut state = self.state.lock().await;
                        state.play_gen = state.play_gen.wrapping_add(1);
                        state.current = Some(item.clone());
                        state.queue.clear();
                        state.history.clear();
                        state.paused = false;
                        state.interruption = None;
                        state.started_in_turn = true;
                        json!({"playing": true, "song": item.song, "queue_size": state.queue.len()})
                    }
                    Err(err) => json!({"error": err.to_string(), "song": item.song}),
                }
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn add_to_queue(&self, queries: &str, song_ids: &str) -> Value {
        let mut added = Vec::new();
        for id in split_many(song_ids) {
            if let Ok(item) = self.resolve_one("", &id).await {
                added.push(item);
            }
        }
        for query in split_many(queries) {
            if let Ok(item) = self.resolve_one(&query, "").await {
                added.push(item);
            }
        }
        if added.is_empty() {
            return json!({"error": "no playable songs resolved"});
        }
        let start_now;
        {
            let mut state = self.state.lock().await;
            start_now = state.current.is_none();
            state.queue.extend(added.clone());
        }
        if start_now {
            if let Err(err) = self.play_next_from_queue().await {
                return json!({"error": err.to_string()});
            }
        }
        json!({"added": added.iter().map(|item| &item.song).collect::<Vec<_>>()})
    }

    pub async fn add_random(&self, count: usize) -> Value {
        match self.random_queue_items(count).await {
            Ok(items) => {
                let mut state = self.state.lock().await;
                let start_now = state.current.is_none();
                state.queue.extend(items.clone());
                drop(state);
                if start_now {
                    if let Err(err) = self.play_next_from_queue().await {
                        return json!({"error": err.to_string()});
                    }
                }
                json!({"added": items.iter().map(|item| &item.song).collect::<Vec<_>>()})
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn play_random(&self, count: usize) -> Value {
        match self.random_queue_items(count).await {
            Ok(items) => {
                {
                    let mut state = self.state.lock().await;
                    state.current = None;
                    state.history.clear();
                    state.queue = items;
                    state.paused = false;
                    state.interruption = None;
                }

                match self.play_next_from_queue().await {
                    Ok(item) => {
                        let state = self.state.lock().await;
                        json!({
                            "playing": true,
                            "song": item.song,
                            "queue_size": state.queue.len(),
                        })
                    }
                    Err(err) => json!({"error": err.to_string()}),
                }
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn next(&self) -> Value {
        match self.play_next_from_queue().await {
            Ok(item) => json!({"playing": true, "song": item.song}),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn previous(&self) -> Value {
        match self.play_previous_from_history().await {
            Ok(item) => json!({"playing": true, "song": item.song}),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn pause(&self) -> Value {
        if !self.has_current().await {
            return json!({"paused": false, "error": "no current song"});
        }
        let paused = self.device.pause_audio().await;
        match paused {
            Ok(()) => {
                let mut state = self.state.lock().await;
                state.paused = true;
                // 用户明确要求暂停，退出唤醒会话时不得再被 restore_after_interruption() 拉起。
                state.interruption = None;
                state.started_in_turn = false;
                json!({"paused": true})
            }
            Err(err) => json!({"paused": false, "error": err.to_string()}),
        }
    }

    pub async fn resume(&self) -> Value {
        if !self.has_current().await {
            return json!({"playing": false, "error": "no current song"});
        }
        let resumed = self.device.resume_audio().await;
        match resumed {
            Ok(()) => {
                let mut state = self.state.lock().await;
                state.paused = false;
                state.interruption = None;
                state.started_in_turn = false;
                json!({"playing": true})
            }
            Err(err) => json!({"playing": false, "error": err.to_string()}),
        }
    }

    pub async fn stop(&self) -> Value {
        let was_ducked = {
            let mut state = self.state.lock().await;
            state.interruption.take() == Some(InterruptionAction::Duck)
        };
        if was_ducked {
            let _ = self.device.unduck_audio().await;
        }
        let stopped = self.device.stop_audio().await;
        let mut state = self.state.lock().await;
        state.play_gen = state.play_gen.wrapping_add(1);
        state.current = None;
        state.queue.clear();
        state.history.clear();
        state.paused = false;
        state.interruption = None;
        state.started_in_turn = false;
        match stopped {
            Ok(()) => json!({"stopped": true}),
            Err(err) => json!({"stopped": false, "error": err.to_string()}),
        }
    }

    pub async fn interrupt_for_wake(&self) -> bool {
        let snapshot = self.config.get().await;
        let mode = Self::interruption_mode(&snapshot);
        {
            let state = self.state.lock().await;
            if state.interruption.is_some() {
                return true;
            }
            if state.current.is_none() || state.paused {
                return false;
            }
        }

        match mode {
            InterruptionAction::Pause => {
                if let Err(err) = self.device.pause_audio().await {
                    warn!("failed to pause music for wake interruption: {err:?}");
                    return false;
                }
            }
            InterruptionAction::Duck => {
                if let Err(err) = self.device.duck_audio().await {
                    warn!("failed to duck music for wake interruption: {err:?}");
                    return false;
                }
            }
            InterruptionAction::None => {}
        }

        let mut state = self.state.lock().await;
        if state.current.is_none() {
            return false;
        }
        if mode == InterruptionAction::Pause {
            state.paused = true;
        }
        state.interruption = Some(mode);
        state.started_in_turn = false;
        true
    }

    /// 当外部 TTS (`DEFAULT_PLAYBACK_COMMAND`) 在工具调用 `play_query` 之后执行了 `killall miplayer` 时，
    /// 在 TTS 回答播报完毕后补拉起本轮刚点播的歌曲。
    pub async fn ensure_playing_after_external_tts(&self) {
        let url_to_replay = {
            let mut state = self.state.lock().await;
            let should_replay =
                state.started_in_turn && state.current.is_some() && !state.paused;
            state.started_in_turn = false;
            if should_replay {
                state.current.as_ref().map(|item| item.url.clone())
            } else {
                None
            }
        };
        if let Some(url) = url_to_replay {
            if let Err(err) = self.device.play_url(&url).await {
                warn!("failed to restart music after external TTS: {err:?}");
            } else {
                let mut state = self.state.lock().await;
                state.play_gen = state.play_gen.wrapping_add(1);
            }
        }
    }

    /// 启动后台曲目结束守望任务：轮询 `/tmp/xiaoai-miplayer.log` 的 `EndReached`，
    /// 在曲目自然播完时自动切下一首或清空 `current`（避免僵尸状态导致后续误拉起 `mphelper play`）。
    #[allow(dead_code)]
    pub fn start_playback_watcher(self: &std::sync::Arc<Self>) {
        if cfg!(test) {
            return;
        }
        let weak = std::sync::Arc::downgrade(self);
        tokio::spawn(async move {
            const MUSIC_LOG_PATH: &str = "/tmp/xiaoai-miplayer.log";
            let mut last_seen_gen: u64 = 0;
            let mut gen_observed_at: Option<tokio::time::Instant> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(service) = weak.upgrade() else {
                    break;
                };
                let active_gen = {
                    let state = service.state.lock().await;
                    if state.current.is_some()
                        && !state.paused
                        && state.interruption.is_none()
                    {
                        Some(state.play_gen)
                    } else {
                        None
                    }
                };
                let Some(play_gen) = active_gen else {
                    continue;
                };
                if play_gen != last_seen_gen {
                    last_seen_gen = play_gen;
                    gen_observed_at = Some(tokio::time::Instant::now());
                    // 给 miplayer 至少 1 个周期重新打开并截断日志，避免读到上一首残留的 EndReached。
                    continue;
                }
                if gen_observed_at
                    .map(|t| t.elapsed() < Duration::from_secs(2))
                    .unwrap_or(true)
                {
                    continue;
                }
                if let Ok(log) = tokio::fs::read_to_string(MUSIC_LOG_PATH).await {
                    if log.contains("EndReached") {
                        let _ = service.on_track_finished(play_gen).await;
                    }
                }
            }
        });
    }

    /// 当当前曲目自然播完时调用：
    /// - 仅当 `play_gen == expected_gen` 且曲目处于活跃播放（非暂停、非唤醒打断）时生效；
    /// - 队列非空则自动切下一首；
    /// - 队列为空则清空 `current` 与 `started_in_turn`，避免僵尸曲目触发后续误拉起。
    pub async fn on_track_finished(&self, expected_gen: u64) -> bool {
        let should_advance = {
            let mut state = self.state.lock().await;
            if state.play_gen != expected_gen
                || state.current.is_none()
                || state.paused
                || state.interruption.is_some()
            {
                return false;
            }
            if state.queue.is_empty() {
                state.current = None;
                state.started_in_turn = false;
                return true;
            }
            true
        };
        if should_advance {
            match self.play_next_from_queue().await {
                Ok(_) => {
                    let mut state = self.state.lock().await;
                    state.started_in_turn = false;
                    return true;
                }
                Err(err) => {
                    warn!("auto-advance to next track failed: {err:?}");
                }
            }
        }
        false
    }

    pub async fn restore_after_interruption(&self) {
        let action = {
            let mut state = self.state.lock().await;
            state.started_in_turn = false;
            state.interruption.take()
        };
        match action {
            Some(InterruptionAction::Pause) => {
                if let Err(err) = self.device.resume_audio().await {
                    warn!("failed to resume music after wake interruption: {err:?}");
                    return;
                }
                let mut state = self.state.lock().await;
                if state.current.is_some() {
                    state.paused = false;
                }
            }
            Some(InterruptionAction::Duck) => {
                if let Err(err) = self.device.unduck_audio().await {
                    warn!("failed to restore music volume after wake interruption: {err:?}");
                }
            }
            Some(InterruptionAction::None) | None => {}
        }
    }

    /// 让位给原生小爱：清空自建音乐队列与打断恢复标记（绝不再自动 `mphelper play`），
    /// 若先前已 duck 系统音量则恢复，并精准清理 Agent 在线音乐 `miplayer`。
    pub async fn yield_to_native(&self) -> bool {
        let (had_current, was_ducked) = {
            let mut state = self.state.lock().await;
            let had_current = state.current.is_some();
            let was_ducked = state.interruption == Some(InterruptionAction::Duck);
            state.play_gen = state.play_gen.wrapping_add(1);
            state.current = None;
            state.queue.clear();
            state.history.clear();
            state.paused = false;
            state.interruption = None;
            state.started_in_turn = false;
            (had_current, was_ducked)
        };
        if was_ducked {
            let _ = self.device.unduck_audio().await;
        }
        if had_current {
            self.device.stop_agent_http_music().await;
        }
        had_current || was_ducked
    }

    pub async fn status(&self) -> Value {
        let state = self.state.lock().await;
        json!({
            "playing": state.current.is_some(),
            "paused": state.paused,
            "interrupted": state.interruption.is_some(),
            "current": state.current.as_ref().map(|item| &item.song),
            "queue_size": state.queue.len(),
            "queue": state.queue.iter().take(10).map(|item| &item.song).collect::<Vec<_>>(),
        })
    }

    async fn resolve_one(&self, query: &str, song_id: &str) -> anyhow::Result<QueueItem> {
        if !song_id.trim().is_empty() {
            let song = self.song_detail(song_id.trim()).await?;
            let url = self.song_url(&song.id).await?;
            return Ok(QueueItem { song, url });
        }
        let snapshot = self.config.get().await;
        let limit = if snapshot.music.provider == "navidrome" { 10 } else { 1 };
        let candidates = rank_song_candidates(query, self.search_inner(query, limit).await?);
        if candidates.is_empty() {
            anyhow::bail!("no songs found");
        }
        let mut last_err: Option<anyhow::Error> = None;
        for song in candidates {
            match self.song_url(&song.id).await {
                Ok(url) => return Ok(QueueItem { song, url }),
                Err(err) => {
                    warn!(song_id = %song.id, title = %song.name, error = %err, "candidate stream url unreachable, trying next candidate");
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no playable songs found")))
    }

    async fn search_inner(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        let snapshot = self.config.get().await;
        require_music_enabled(&snapshot)?;
        let songs = match snapshot.music.provider.as_str() {
            "netease" => self.search_netease(query, limit).await?,
            "navidrome" => self.search_navidrome(query, limit).await?,
            other => anyhow::bail!("Unsupported music provider: {other}"),
        };
        Ok(rank_song_candidates(query, songs))
    }

    async fn random_songs(&self, limit: usize) -> anyhow::Result<Vec<Song>> {
        let snapshot = self.config.get().await;
        require_music_enabled(&snapshot)?;
        match snapshot.music.provider.as_str() {
            "navidrome" => {
                let data = self
                    .navidrome_request("/rest/getRandomSongs.view", &[("size", limit.to_string())])
                    .await?;
                let songs = data
                    .get("randomSongs")
                    .and_then(|value| value.get("song"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !songs.is_empty() {
                    return Ok(songs.iter().map(parse_navidrome_song).collect());
                }
                // 部分 OpenSubsonic 后端（如 lxserver）的 getRandomSongs 返回空数组，
                // 降级从随机专辑列表中拉取真实曲目。
                self.random_songs_from_albums(limit).await
            }
            _ => self.search_netease("", limit).await,
        }
    }

    async fn random_queue_items(&self, count: usize) -> anyhow::Result<Vec<QueueItem>> {
        let songs = self.random_songs(count.clamp(1, 100)).await?;
        let mut items = Vec::new();
        for song in songs {
            if let Ok(url) = self.song_url(&song.id).await {
                items.push(QueueItem { song, url });
            }
        }
        if items.is_empty() {
            anyhow::bail!("no playable random songs resolved");
        }
        Ok(items)
    }

    async fn song_detail(&self, song_id: &str) -> anyhow::Result<Song> {
        let snapshot = self.config.get().await;
        match snapshot.music.provider.as_str() {
            "navidrome" => {
                let data = self
                    .navidrome_request("/rest/getSong.view", &[("id", song_id.to_string())])
                    .await?;
                let item = data.get("song").context("song detail not found")?;
                Ok(parse_navidrome_song(item))
            }
            _ => Ok(Song {
                id: song_id.to_string(),
                name: song_id.to_string(),
                artists: vec![],
                album: String::new(),
                duration_ms: None,
                fee: None,
            }),
        }
    }

    async fn song_url(&self, song_id: &str) -> anyhow::Result<String> {
        let snapshot = self.config.get().await;
        match snapshot.music.provider.as_str() {
            "navidrome" => {
                let raw_url = Self::navidrome_stream_url(&snapshot, song_id)?;
                self.probe_navidrome_stream_url(
                    &raw_url,
                    timeout_duration(snapshot.music.navidrome.timeout_s),
                )
                .await
            }
            _ => {
                let data = self
                    .netease_request(
                        "/song/url/v1",
                        &[
                            ("id", song_id.to_string()),
                            ("level", snapshot.music.netease.default_level.clone()),
                        ],
                    )
                    .await?;
                data.get("data")
                    .and_then(Value::as_array)
                    .and_then(|items| items.first())
                    .and_then(|item| item.get("url"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .context("song has no playable url")
            }
        }
    }

    async fn play_next_from_queue(&self) -> anyhow::Result<QueueItem> {
        let item = {
            let mut state = self.state.lock().await;
            let item = state.queue.first().cloned().context("queue is empty")?;
            state.queue.remove(0);
            if let Some(current) = state.current.take() {
                state.history.push(current);
            }
            state.play_gen = state.play_gen.wrapping_add(1);
            state.current = Some(item.clone());
            state.paused = false;
            state.interruption = None;
            state.started_in_turn = true;
            item
        };
        self.device.play_url(&item.url).await?;
        Ok(item)
    }

    pub async fn has_current(&self) -> bool {
        self.state.lock().await.current.is_some()
    }

    #[cfg(test)]
    pub(crate) async fn set_current_for_test(&self, song_id: &str, name: &str, url: &str) {
        let mut state = self.state.lock().await;
        state.play_gen = state.play_gen.wrapping_add(1);
        state.current = Some(QueueItem {
            song: Song {
                id: song_id.to_string(),
                name: name.to_string(),
                artists: vec!["周杰伦".to_string()],
                album: String::new(),
                duration_ms: None,
                fee: None,
            },
            url: url.to_string(),
        });
        state.paused = false;
        state.interruption = None;
        state.started_in_turn = false;
    }

    async fn play_previous_from_history(&self) -> anyhow::Result<QueueItem> {
        let item = {
            let mut state = self.state.lock().await;
            let item = state.history.pop().context("history is empty")?;
            if let Some(current) = state.current.take() {
                state.queue.insert(0, current);
            }
            state.play_gen = state.play_gen.wrapping_add(1);
            state.current = Some(item.clone());
            state.paused = false;
            state.interruption = None;
            state.started_in_turn = true;
            item
        };
        self.device.play_url(&item.url).await?;
        Ok(item)
    }

    fn interruption_mode(config: &AppConfig) -> InterruptionAction {
        match config.music.interruption.mode.trim() {
            "duck" | "lower_volume" | "volume" => InterruptionAction::Duck,
            "none" | "keep_playing" => InterruptionAction::None,
            _ => InterruptionAction::Pause,
        }
    }

    async fn search_netease(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        let data = self
            .netease_request(
                "/cloudsearch",
                &[
                    ("keywords", query.to_string()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        let songs = data
            .get("result")
            .and_then(|value| value.get("songs"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().map(parse_netease_song).collect())
    }

    async fn search_navidrome(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        let data = self
            .navidrome_request(
                "/rest/search3.view",
                &[
                    ("query", query.to_string()),
                    ("songCount", limit.to_string()),
                    ("artistCount", "3".to_string()),
                    ("albumCount", "3".to_string()),
                ],
            )
            .await?;
        let songs = data
            .get("searchResult3")
            .and_then(|value| value.get("song"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().map(parse_navidrome_song).collect())
    }

    async fn netease_request(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        let snapshot = self.config.get().await;
        require_music_enabled(&snapshot)?;
        self.ensure_netease_server().await?;
        let cookie = self.read_cookie().await;
        let mut url = Url::parse(&snapshot.music.netease.api_base_url)?;
        url.set_path(path);
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                if !value.is_empty() {
                    pairs.append_pair(key, value);
                }
            }
            if let Some(cookie) = cookie {
                pairs.append_pair("cookie", &cookie);
            }
        }
        Ok(self
            .client
            .get(url)
            .timeout(timeout_duration(snapshot.music.netease.timeout_s))
            .send()
            .await?
            .json::<Value>()
            .await?)
    }

    async fn random_songs_from_albums(&self, limit: usize) -> anyhow::Result<Vec<Song>> {
        let data = self
            .navidrome_request(
                "/rest/getAlbumList2.view",
                &[("type", "random".to_string()), ("size", "5".to_string())],
            )
            .await?;
        let albums = data
            .get("albumList2")
            .and_then(|v| v.get("album"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut collected = Vec::new();
        for album in albums {
            if collected.len() >= limit {
                break;
            }
            let album_id = value_string(&album["id"]);
            if album_id.is_empty() || album_id == "null" {
                continue;
            }
            let Ok(album_data) = self
                .navidrome_request("/rest/getAlbum.view", &[("id", album_id)])
                .await
            else {
                continue;
            };
            let songs = album_data
                .get("album")
                .and_then(|a| a.get("song"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in songs {
                collected.push(parse_navidrome_song(&item));
                if collected.len() >= limit {
                    break;
                }
            }
        }
        Ok(collected)
    }

    async fn probe_navidrome_stream_url(
        &self,
        raw_url: &str,
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let mut current = Url::parse(raw_url)?;
        for _hop in 0..5 {
            let resp = self.send_stream_hop(&current, timeout).await?;
            let status = resp.status();
            if status.is_redirection() {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .context("stream redirect missing Location header")?;
                current = current.join(loc)?;
                continue;
            }
            if !status.is_success() {
                anyhow::bail!("stream url returned HTTP {status}");
            }
            if let Some(ct) = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
            {
                let lower = ct.to_ascii_lowercase();
                if lower.contains("application/json") || lower.contains("xml") {
                    anyhow::bail!("stream url returned API error payload ({lower})");
                }
            }
            return Ok(current.to_string());
        }
        anyhow::bail!("too many redirects resolving stream url")
    }

    async fn send_stream_hop(
        &self,
        url: &Url,
        timeout: Duration,
    ) -> anyhow::Result<reqwest::Response> {
        match self
            .no_redirect_client
            .get(url.as_str())
            .header(RANGE, "bytes=0-1")
            .timeout(timeout)
            .send()
            .await
        {
            Ok(resp) => Ok(resp),
            Err(err) => {
                // 部分运营/路由 DNS 会把音源跳转域名污染到 0.0.0.0（导致 Connection refused）。
                // 对公网域名通过 AliDNS DoH 二次解析并在单跳 Client 内覆盖解析，避免修改系统 hosts/DNS。
                if let Some(url::Host::Domain(domain)) = url.host() {
                    if !domain.eq_ignore_ascii_case("localhost") && !domain.ends_with(".local") {
                        if let Some(ip) = self.resolve_domain_via_alidns(domain).await {
                            let port = url.port_or_known_default().unwrap_or(443);
                            let override_client = Client::builder()
                                .connect_timeout(Duration::from_secs(5))
                                .redirect(Policy::none())
                                .resolve(domain, SocketAddr::new(IpAddr::V4(ip), port))
                                .build()?;
                            return override_client
                                .get(url.as_str())
                                .header(RANGE, "bytes=0-1")
                                .timeout(timeout)
                                .send()
                                .await
                                .with_context(|| "stream url connection failed after DoH fallback");
                        }
                    }
                }
                Err(err).context("stream url connection failed")
            }
        }
    }

    async fn resolve_domain_via_alidns(&self, domain: &str) -> Option<Ipv4Addr> {
        let mut doh = Url::parse("https://223.5.5.5/resolve").ok()?;
        doh.query_pairs_mut()
            .append_pair("name", domain)
            .append_pair("type", "A");
        let resp = self
            .client
            .get(doh)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .ok()?;
        let data = resp.json::<Value>().await.ok()?;
        for ans in data.get("Answer")?.as_array()? {
            if let Some(ip_str) = ans.get("data").and_then(Value::as_str) {
                if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        return Some(ip);
                    }
                }
            }
        }
        None
    }

    async fn navidrome_request(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        let snapshot = self.config.get().await;
        let timeout = timeout_duration(snapshot.music.navidrome.timeout_s);
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..=1 {
            let mut url = Url::parse(&snapshot.music.navidrome.base_url)?;
            url.set_path(path);
            let (salt, token) = subsonic_token(&snapshot.music.navidrome.password);
            {
                let mut pairs = url.query_pairs_mut();
                pairs
                    .append_pair("u", &snapshot.music.navidrome.username)
                    .append_pair("t", &token)
                    .append_pair("s", &salt)
                    .append_pair("v", &snapshot.music.navidrome.api_version)
                    .append_pair("c", "xiaoai-agent")
                    .append_pair("f", "json");
                for (key, value) in params {
                    pairs.append_pair(key, value);
                }
            }
            match self.client.get(url).timeout(timeout).send().await {
                Ok(resp) => {
                    let data = resp.json::<Value>().await?;
                    let response = data
                        .get("subsonic-response")
                        .context("Invalid Navidrome response")?;
                    if response.get("status").and_then(Value::as_str) != Some("ok") {
                        anyhow::bail!(
                            "{}",
                            response
                                .get("error")
                                .and_then(|err| err.get("message"))
                                .and_then(Value::as_str)
                                .unwrap_or("Navidrome request failed")
                        );
                    }
                    return Ok(response.clone());
                }
                Err(err) if attempt == 0 && (err.is_timeout() || err.is_connect()) => {
                    warn!(path = %path, error = %err, "Navidrome request transient failure, retrying once");
                    last_err = Some(err.into());
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Navidrome request failed")))
    }

    fn navidrome_stream_url(config: &AppConfig, song_id: &str) -> anyhow::Result<String> {
        let mut url = Url::parse(&config.music.navidrome.base_url)?;
        url.set_path("/rest/stream.view");
        let (salt, token) = subsonic_token(&config.music.navidrome.password);
        url.query_pairs_mut()
            .append_pair("u", &config.music.navidrome.username)
            .append_pair("t", &token)
            .append_pair("s", &salt)
            .append_pair("v", &config.music.navidrome.api_version)
            .append_pair("c", "xiaoai-agent")
            .append_pair("f", "json")
            .append_pair("id", song_id);
        Ok(url.to_string())
    }

    async fn ensure_netease_server(&self) -> anyhow::Result<()> {
        let snapshot = self.config.get().await;
        if self
            .client
            .get(format!(
                "{}/login/status",
                snapshot.music.netease.api_base_url.trim_end_matches('/')
            ))
            .timeout(timeout_duration(3.0))
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        anyhow::bail!(
            "Netease API is not reachable at {}; run the API service externally and set music.netease.api_base_url",
            snapshot.music.netease.api_base_url
        );
    }

    async fn read_cookie(&self) -> Option<String> {
        let snapshot = self.config.get().await;
        if let Some(cookie) = self.netease_cookie.lock().await.clone() {
            return Some(cookie);
        }
        let path = snapshot.music.netease.cookie_file.as_ref()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("cookie")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .or_else(|| Some(text.trim().to_string()))
            .filter(|text| !text.is_empty())
    }

    async fn remember_cookie_from_response(&self, data: &Value) {
        let Some(cookie) = data.get("cookie").and_then(Value::as_str) else {
            return;
        };
        *self.netease_cookie.lock().await = Some(cookie.to_string());
    }

    fn netease_phone(config: &AppConfig) -> String {
        if !config.music.netease.phone.trim().is_empty() {
            config.music.netease.phone.trim().to_string()
        } else {
            config.music.netease.account.trim().to_string()
        }
    }
}

fn split_many(text: &str) -> Vec<String> {
    text.split(['\n', ';', '；'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn rank_song_candidates(query: &str, mut songs: Vec<Song>) -> Vec<Song> {
    let trimmed = query.trim();
    if trimmed.is_empty() || songs.len() <= 1 {
        return songs;
    }
    let query_lower = trimmed.to_ascii_lowercase();
    let mut terms: Vec<String> = Vec::new();
    for part in query_lower.split(|c: char| c.is_whitespace() || matches!(c, '、' | ',' | '，' | '-' | '—')) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if !terms.iter().any(|t| t == p) {
            terms.push(p.to_string());
        }
        if let Some((a, b)) = p.split_once('的') {
            let (a, b) = (a.trim(), b.trim());
            if !a.is_empty() && !b.is_empty() {
                if !terms.iter().any(|t| t == a) {
                    terms.push(a.to_string());
                }
                if !terms.iter().any(|t| t == b) {
                    terms.push(b.to_string());
                }
            }
        }
    }

    const COVER_MARKERS: &[&str] = &[
        "原唱", "翻唱", "cover", "女声", "男声", "钢琴", "吉他", "伴奏", "纯音乐", "dj", "remix",
    ];
    const LIVE_MARKERS: &[&str] = &["live", "演唱会", "现场"];

    let wants_cover = COVER_MARKERS.iter().any(|m| query_lower.contains(m));
    let wants_live = LIVE_MARKERS.iter().any(|m| query_lower.contains(m));

    let score_song = |song: &Song| -> i32 {
        let title_lower = song.name.trim().to_ascii_lowercase();
        let base_title = title_lower
            .split(['(', '（', '['])
            .next()
            .unwrap_or(&title_lower)
            .trim();
        let artists_lower: Vec<String> = song
            .artists
            .iter()
            .map(|a| a.trim().to_ascii_lowercase())
            .collect();
        let artists_joined = artists_lower.join(" ");

        let mut score: i32 = 0;
        for term in &terms {
            if artists_lower.iter().any(|a| a == term) {
                score += 60;
            } else if term.chars().count() >= 2 && artists_joined.contains(term.as_str()) {
                score += 35;
            }
            if base_title == term.as_str() {
                score += 45;
            } else if term.chars().count() >= 2 && base_title.contains(term.as_str()) {
                score += 25;
            }
        }
        if !wants_cover && COVER_MARKERS.iter().any(|m| title_lower.contains(m)) {
            score -= 80;
        }
        if !wants_live && LIVE_MARKERS.iter().any(|m| title_lower.contains(m)) {
            score -= 20;
        }
        score
    };

    songs.sort_by(|a, b| score_song(b).cmp(&score_song(a)));
    songs
}

fn parse_netease_song(item: &Value) -> Song {
    Song {
        id: value_string(&item["id"]),
        name: value_string(&item["name"]),
        artists: item
            .get("ar")
            .or_else(|| item.get("artists"))
            .and_then(Value::as_array)
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        album: item
            .get("al")
            .or_else(|| item.get("album"))
            .and_then(|album| album.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        duration_ms: item
            .get("dt")
            .or_else(|| item.get("duration"))
            .and_then(Value::as_u64),
        fee: item.get("fee").cloned(),
    }
}

fn parse_navidrome_song(item: &Value) -> Song {
    let mut artists = item
        .get("artists")
        .and_then(Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if artists.is_empty() {
        if let Some(artist) = item.get("artist").and_then(Value::as_str) {
            artists.push(artist.to_string());
        }
    }
    Song {
        id: value_string(&item["id"]),
        name: item
            .get("title")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        artists,
        album: item
            .get("album")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        duration_ms: item
            .get("duration")
            .and_then(Value::as_u64)
            .map(|seconds| seconds * 1000),
        fee: None,
    }
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string().trim_matches('"').to_string())
}

fn subsonic_token(password: &str) -> (String, String) {
    let mut bytes = [0_u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let salt = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let token = format!("{:x}", md5::compute(format!("{password}{salt}")));
    (salt, token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_many_handles_all_supported_separators() {
        assert_eq!(
            split_many("周杰伦 晴天\n稻香;七里香；夜曲"),
            vec!["周杰伦 晴天", "稻香", "七里香", "夜曲"]
        );
        // 全是分隔符和空白时不能产出空条目（否则会拿空串去搜索）
        assert!(split_many("\n  \n;\n；").is_empty());
        assert_eq!(split_many("  只有一条  "), vec!["只有一条"]);
        assert!(split_many("").is_empty());
    }

    #[test]
    fn value_string_coerces_json_scalars() {
        // id 既可能是字符串也可能是数字（Navidrome 用 "al-123"，网易云用 186016）
        assert_eq!(value_string(&json!("al-123")), "al-123");
        assert_eq!(value_string(&json!(186016)), "186016");
        assert_eq!(value_string(&json!(1.5)), "1.5");
        // 没有值的时候退化成 JSON 字面量，而不是空串：调用方据此能判断「缺 id」
        assert_eq!(value_string(&json!(null)), "null");
    }

    #[test]
    fn parse_navidrome_song_falls_back_to_single_artist() {
        // Subsonic 的 id3 结构：artists 是数组
        let modern = json!({
            "id": "al-306",
            "title": "晴天",
            "artists": [{"name": "周杰伦"}, {"name": "方文山"}],
            "album": "叶惠美",
            "duration": 269
        });
        let song = parse_navidrome_song(&modern);
        assert_eq!(song.id, "al-306");
        assert_eq!(song.name, "晴天", "title 优先于 name");
        assert_eq!(song.artists, vec!["周杰伦", "方文山"]);
        assert_eq!(song.album, "叶惠美");
        // 注意：Navidrome 的 duration 是秒，这里换算成毫秒
        assert_eq!(song.duration_ms, Some(269_000));
        assert!(song.fee.is_none(), "Navidrome 没有付费标记");

        // 老实现只给单个 artist 字符串时要能用它兜底
        let legacy = json!({"id": 42, "name": "稻香", "artist": "周杰伦"});
        let song = parse_navidrome_song(&legacy);
        assert_eq!(song.id, "42");
        assert_eq!(song.name, "稻香", "没有 title 时回退到 name");
        assert_eq!(song.artists, vec!["周杰伦"]);
        assert!(song.duration_ms.is_none());
    }

    #[test]
    fn subsonic_token_matches_subsonic_auth_scheme() {
        let (salt, token) = subsonic_token("secret");
        // Subsonic 的 salt 是 8 字节十六进制，token = md5(password + salt)
        assert_eq!(salt.len(), 16);
        assert!(salt.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(
            token,
            format!("{:x}", md5::compute(format!("secret{salt}")))
        );

        let (second_salt, _) = subsonic_token("secret");
        assert_ne!(salt, second_salt, "salt 必须每次随机，否则可被重放");
    }

    #[test]
    fn parse_netease_song_supports_both_response_shapes() {
        // 新版接口用 ar / al / dt
        let modern = json!({
            "id": 186016,
            "name": "晴天",
            "ar": [{"name": "周杰伦"}],
            "al": {"name": "叶惠美"},
            "dt": 269000,
            "fee": 0
        });
        let song = parse_netease_song(&modern);
        assert_eq!(song.id, "186016");
        assert_eq!(song.name, "晴天");
        assert_eq!(song.artists, vec!["周杰伦"]);
        assert_eq!(song.album, "叶惠美");
        assert_eq!(song.duration_ms, Some(269000));

        // 老版/搜索接口用 artists / album / duration
        let legacy = json!({
            "id": "abc123",
            "name": "稻香",
            "artists": [{"name": "周杰伦"}, {"name": "方文山"}],
            "album": {"name": "魔杰座"},
            "duration": 223000
        });
        let song = parse_netease_song(&legacy);
        assert_eq!(song.id, "abc123");
        assert_eq!(song.artists, vec!["周杰伦", "方文山"]);
        assert_eq!(song.album, "魔杰座");
        assert_eq!(song.duration_ms, Some(223000));
        assert!(song.fee.is_none());
    }

    async fn spawn_mock_subsonic<F, Fut>(handler: F) -> String
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = (u16, String, Vec<(String, String)>)> + Send,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = std::sync::Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let h = handler.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let (code, body, headers) = h(path).await;
                    let reason = match code {
                        200 => "OK",
                        206 => "Partial Content",
                        302 => "Found",
                        _ => "Error",
                    };
                    let mut resp = format!(
                        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");
                    resp.push_str(&body);
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    fn navidrome_test_service(
        base_url: String,
        timeout_s: f64,
    ) -> (MusicService, std::sync::Arc<crate::shell::RecordingShell>) {
        let mut cfg = AppConfig::default();
        cfg.music.enabled = true;
        cfg.music.provider = "navidrome".to_string();
        cfg.music.navidrome.base_url = base_url;
        cfg.music.navidrome.username = "u".to_string();
        cfg.music.navidrome.password = "p".to_string();
        cfg.music.navidrome.timeout_s = timeout_s;
        let shell = std::sync::Arc::new(crate::shell::RecordingShell::always_ok());
        let device = Device::with_runner(cfg.device.clone(), shell.clone());
        let handle = ConfigHandle::new(std::sync::Arc::new(cfg));
        (MusicService::new(handle, device).unwrap(), shell)
    }

    #[tokio::test]
    async fn pause_clears_wake_interruption_so_restore_does_not_resume() {
        let (music, shell) = navidrome_test_service("http://127.0.0.1:9".into(), 2.0);
        {
            let mut st = music.state.lock().await;
            st.current = Some(QueueItem {
                song: Song {
                    id: "wy_1".into(),
                    name: "晴天".into(),
                    artists: vec!["周杰伦".into()],
                    album: "".into(),
                    duration_ms: None,
                    fee: None,
                },
                url: "http://example.com/1.mp3".into(),
            });
            st.paused = false;
        }
        assert!(music.is_actively_playing().await);
        assert!(music.interrupt_for_wake().await);
        assert!(!music.is_actively_playing().await);

        let paused = music.pause().await;
        assert_eq!(paused["paused"], json!(true));
        let before = shell.count_matching("mphelper play");

        music.restore_after_interruption().await;
        assert_eq!(
            shell.count_matching("mphelper play"),
            before,
            "pause() 后退出会话不应由 restore_after_interruption 重新拉起播放"
        );
    }

    #[tokio::test]
    async fn stop_after_wake_interruption_never_resumes_audio_before_stopping() {
        let (music, shell) = navidrome_test_service("http://127.0.0.1:9".into(), 2.0);
        music
            .set_current_for_test("wy_1", "晴天", "http://example.com/1.mp3")
            .await;
        assert!(music.interrupt_for_wake().await);

        let before_resume = shell.count_matching("mphelper play");
        let stopped = music.stop().await;
        assert_eq!(stopped["stopped"], json!(true));
        assert_eq!(
            shell.count_matching("mphelper play"),
            before_resume,
            "stop() 前不得先调用 resume_audio，否则已被唤醒暂停的音乐会在被杀前突然爆响一下"
        );
    }

    #[tokio::test]
    async fn play_query_skips_unreachable_candidate_and_falls_back_to_next_playable_song() {
        let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead_listener.local_addr().unwrap().port();
        drop(dead_listener);

        let base = spawn_mock_subsonic(move |path| async move {
            if path.starts_with("/rest/search3.view") {
                let body = json!({
                    "subsonic-response": {
                        "status": "ok",
                        "searchResult3": {
                            "song": [
                                {"id": "kw_bad", "title": "晴天(坏源)", "artist": "周杰伦", "duration": 269},
                                {"id": "wy_good", "title": "晴天", "artist": "周杰伦", "duration": 269}
                            ]
                        }
                    }
                })
                .to_string();
                return (200, body, vec![("Content-Type".into(), "application/json".into())]);
            }
            if path.starts_with("/rest/stream.view") && path.contains("id=kw_bad") {
                return (
                    302,
                    String::new(),
                    vec![("Location".into(), format!("http://127.0.0.1:{dead_port}/dead.flac"))],
                );
            }
            if path.starts_with("/rest/stream.view") && path.contains("id=wy_good") {
                return (200, "FAKEAUDIO".into(), vec![("Content-Type".into(), "audio/mpeg".into())]);
            }
            (404, "not found".into(), vec![])
        })
        .await;

        let (music, _shell) = navidrome_test_service(base, 3.0);
        let res = music.play_query("晴天", "").await;
        assert_eq!(res["playing"], json!(true), "res={res}");
        assert_eq!(res["song"]["id"], json!("wy_good"));
        assert!(music.is_actively_playing().await);
    }

    #[tokio::test]
    async fn play_random_falls_back_to_random_albums_when_get_random_songs_is_empty() {
        let base = spawn_mock_subsonic(|path| async move {
            if path.starts_with("/rest/getRandomSongs.view") {
                let body = json!({"subsonic-response": {"status": "ok", "randomSongs": {"song": []}}})
                    .to_string();
                return (200, body, vec![("Content-Type".into(), "application/json".into())]);
            }
            if path.starts_with("/rest/getAlbumList2.view") {
                let body = json!({
                    "subsonic-response": {
                        "status": "ok",
                        "albumList2": {"album": [{"id": "alb_1", "name": "Album 1"}]}
                    }
                })
                .to_string();
                return (200, body, vec![("Content-Type".into(), "application/json".into())]);
            }
            if path.starts_with("/rest/getAlbum.view") {
                let body = json!({
                    "subsonic-response": {
                        "status": "ok",
                        "album": {"song": [{"id": "tx_1", "title": "Random Hit", "artist": "Artist"}]}
                    }
                })
                .to_string();
                return (200, body, vec![("Content-Type".into(), "application/json".into())]);
            }
            if path.starts_with("/rest/stream.view") {
                return (200, "AUDIO".into(), vec![("Content-Type".into(), "audio/mpeg".into())]);
            }
            (404, "not found".into(), vec![])
        })
        .await;

        let (music, _) = navidrome_test_service(base, 3.0);
        let res = music.play_random(3).await;
        assert_eq!(res["playing"], json!(true), "res={res}");
        assert_eq!(res["song"]["id"], json!("tx_1"));
    }

    #[tokio::test]
    async fn navidrome_request_retries_once_on_timeout_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let attempts_srv = attempts.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let n = attempts_srv.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    if n == 0 {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        return;
                    }
                    let body = json!({
                        "subsonic-response": {
                            "status": "ok",
                            "searchResult3": {"song": []}
                        }
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let (music, _) = navidrome_test_service(format!("http://{addr}"), 0.25);
        let res = music.search("晴天", 3).await;
        assert_eq!(res, json!([]));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rank_song_candidates_prefers_original_artist_over_cover() {
        let songs = vec![
            Song {
                id: "wy_2668397359".into(),
                name: "晴天 (原唱 周杰伦)".into(),
                artists: vec!["RyaVocal".into()],
                album: "晴天".into(),
                duration_ms: Some(270_000),
                fee: None,
            },
            Song {
                id: "wy_3440441479".into(),
                name: "晴天".into(),
                artists: vec!["Jay".into()],
                album: "晴天".into(),
                duration_ms: Some(112_000),
                fee: None,
            },
            Song {
                id: "kw_80456317".into(),
                name: "晴天 (Live)".into(),
                artists: vec!["周杰伦".into()],
                album: "演唱会".into(),
                duration_ms: Some(249_000),
                fee: None,
            },
            Song {
                id: "kw_228908".into(),
                name: "晴天".into(),
                artists: vec!["周杰伦".into()],
                album: "叶惠美".into(),
                duration_ms: Some(269_000),
                fee: None,
            },
        ];
        let ranked = rank_song_candidates("周杰伦的晴天", songs);
        assert_eq!(ranked[0].id, "kw_228908", "录音室原唱周杰伦《晴天》必须排第一");
        assert_eq!(ranked[1].id, "kw_80456317", "周杰伦 Live 应排在同名翻唱/封面前面");
        assert_eq!(ranked.last().unwrap().id, "wy_2668397359", "带原唱字样的翻唱应垫底");
    }

    #[tokio::test]
    async fn on_track_finished_auto_advances_queue_clears_when_empty_and_ignores_stale_gen() {
        let (music, shell) = navidrome_test_service("http://127.0.0.1:9".into(), 2.0);
        music
            .set_current_for_test("s1", "第一首", "http://example.com/1.mp3")
            .await;
        let gen1 = {
            let mut st = music.state.lock().await;
            st.queue.push(QueueItem {
                song: Song {
                    id: "s2".into(),
                    name: "第二首".into(),
                    artists: vec!["周杰伦".into()],
                    album: String::new(),
                    duration_ms: None,
                    fee: None,
                },
                url: "http://example.com/2.mp3".into(),
            });
            st.play_gen
        };

        // Stale play_gen must be ignored.
        assert!(
            !music.on_track_finished(gen1.wrapping_sub(1)).await,
            "过期 play_gen 必须被忽略"
        );
        assert_eq!(music.status().await["current"]["id"], json!("s1"));

        // Matching play_gen with non-empty queue auto-advances to s2.
        assert!(music.on_track_finished(gen1).await);
        assert_eq!(music.status().await["current"]["id"], json!("s2"));
        assert_eq!(shell.count_matching("http://example.com/2.mp3"), 1);

        let gen2 = music.state.lock().await.play_gen;
        assert!(gen2 > gen1, "切歌后 play_gen 必须递增");

        // Matching play_gen with empty queue clears current and started_in_turn.
        assert!(music.on_track_finished(gen2).await);
        assert!(!music.has_current().await);
        assert!(!music.is_actively_playing().await);
        assert!(!music.state.lock().await.started_in_turn);
    }
}
