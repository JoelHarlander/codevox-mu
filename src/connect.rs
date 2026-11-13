//! Spotify Connect playback through the Web API.
//!
//! Premium audio plays on a device the user already has open (the Spotify
//! app on this computer, or a phone). This process does not decode Spotify
//! audio. librespot's in-app path is disabled: newer accounts fail with
//! audio key error 0/1 and librespot 0.8 has no fix
//! (librespot-org/librespot#1649).
//!
//! Calls used here: `GET /v1/me/player/devices`, `PUT /v1/me/player/play`,
//! `PUT /v1/me/player/pause`, `POST /v1/me/player/next`,
//! `POST /v1/me/player/previous`, `PUT /v1/me/player/seek`,
//! `PUT /v1/me/player/volume`, and `GET /v1/me/player`.
//! A 401 refreshes the Web API token once. A 403 is a missing playback
//! scope or a non-Premium account. A 404 means no active device. A 429
//! waits for `Retry-After` inside the shared request helper. None of the
//! log lines include a token.

use std::time::{Duration, Instant};

use crate::music::Error;
use crate::player::{
    DeviceOption, ERR_MISSING_SCOPE, ERR_NO_DEVICE, ERR_NOT_PREMIUM, SpotifyEngine, SpotifyFeed,
};

use super::{SpotifyAccount, Verb};

const DEVICES_URL: &str = "https://api.spotify.com/v1/me/player/devices";
const PLAYER_URL: &str = "https://api.spotify.com/v1/me/player";
const PLAY_URL: &str = "https://api.spotify.com/v1/me/player/play";
const PAUSE_URL: &str = "https://api.spotify.com/v1/me/player/pause";
const NEXT_URL: &str = "https://api.spotify.com/v1/me/player/next";
const PREVIOUS_URL: &str = "https://api.spotify.com/v1/me/player/previous";
const SEEK_URL: &str = "https://api.spotify.com/v1/me/player/seek";
const VOLUME_URL: &str = "https://api.spotify.com/v1/me/player/volume";

const POLL_EVERY: Duration = Duration::from_secs(1);
const DEVICE_EVERY: Duration = Duration::from_secs(5);
const HOLD: Duration = Duration::from_secs(2);
const END_SLACK: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConnectDevice {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub active: bool,
    pub restricted: bool,
    pub volume: Option<u8>,
    pub supports_volume: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConnectError {
    SignedOut,
    Unauthorized,
    PremiumRequired,
    MissingScope,
    NoDevice,
    RateLimited(String),
    Failed(String),
}

impl ConnectError {
    fn message(&self) -> String {
        match self {
            Self::SignedOut => "Spotify is signed out.".to_string(),
            Self::Unauthorized => "Spotify sign-in needs to be renewed.".to_string(),
            Self::PremiumRequired => ERR_NOT_PREMIUM.to_string(),
            Self::MissingScope => ERR_MISSING_SCOPE.to_string(),
            Self::NoDevice => ERR_NO_DEVICE.to_string(),
            Self::RateLimited(message) | Self::Failed(message) => message.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Unauthorized,
    PremiumRequired,
    MissingScope,
    NoDevice,
    Failed,
}

fn classify(status: u16, body: &str) -> Outcome {
    match status {
        200 | 202 | 204 => Outcome::Ok,
        401 => Outcome::Unauthorized,
        403 if missing_scope_body(body) => Outcome::MissingScope,
        403 => Outcome::PremiumRequired,
        404 => Outcome::NoDevice,
        _ => Outcome::Failed,
    }
}

fn missing_scope_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("scope") || lower.contains("permission")
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok",
        Outcome::Unauthorized => "unauthorized",
        Outcome::PremiumRequired => "premium required",
        Outcome::MissingScope => "missing scope",
        Outcome::NoDevice => "no active device",
        Outcome::Failed => "failed",
    }
}

fn log_outcome(verb: &str, url: &str, status: u16, outcome: &str) {
    log::info!(
        "spotify connect: {verb} {} {status} {outcome}",
        super::path_for_log(url)
    );
}

/// Active device, otherwise the first computer that can be controlled.
///
/// A restricted device is skipped. `preferred` wins when it is still in the
/// list, so a choice the user made sticks until that device disappears.
pub(super) fn choose_device<'a>(
    devices: &'a [ConnectDevice],
    preferred: Option<&str>,
) -> Option<&'a ConnectDevice> {
    fn usable(device: &ConnectDevice) -> bool {
        !device.restricted && !device.id.is_empty()
    }

    if let Some(id) = preferred
        && let Some(device) = devices
            .iter()
            .find(|device| usable(device) && device.id == id)
    {
        return Some(device);
    }
    if let Some(device) = devices
        .iter()
        .find(|device| usable(device) && device.active)
    {
        return Some(device);
    }
    devices
        .iter()
        .find(|device| usable(device) && device.kind.eq_ignore_ascii_case("computer"))
}

/// A device selftest can start. Auto-pick first, then any controllable device,
/// so a phone that is open still counts as "a device exists".
pub(super) fn device_for_playback<'a>(
    devices: &'a [ConnectDevice],
    preferred: Option<&str>,
) -> Option<&'a ConnectDevice> {
    choose_device(devices, preferred).or_else(|| {
        devices
            .iter()
            .find(|device| !device.restricted && !device.id.is_empty())
    })
}

fn devices_from_json(body: &str) -> Result<Vec<ConnectDevice>, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    if let Some(message) = super::api_error_message(&value) {
        return Err(message);
    }
    let Some(items) = value.get("devices").and_then(|entry| entry.as_array()) else {
        return Err("Spotify's device list could not be read.".to_string());
    };
    let mut devices = Vec::new();
    for item in items {
        let id = super::text_field(item, "id");
        if id.is_empty() {
            continue;
        }
        let name = super::text_field(item, "name");
        devices.push(ConnectDevice {
            id,
            name: if name.is_empty() {
                "Spotify".to_string()
            } else {
                name
            },
            kind: super::text_field(item, "type"),
            active: item
                .get("is_active")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            restricted: item
                .get("is_restricted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            volume: item
                .get("volume_percent")
                .and_then(serde_json::Value::as_u64)
                .and_then(|percent| u8::try_from(percent).ok()),
            supports_volume: item
                .get("supports_volume")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(devices)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RemotePlayer {
    playing: bool,
    progress: Duration,
    track_id: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    duration: Option<Duration>,
    volume: Option<u8>,
}

fn player_from_json(body: &str) -> Result<Option<RemotePlayer>, String> {
    if body.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    if value.is_null() {
        return Ok(None);
    }
    if let Some(message) = super::api_error_message(&value) {
        return Err(message);
    }
    let playing = value
        .get("is_playing")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let progress = Duration::from_millis(
        value
            .get("progress_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    );
    let item = value.get("item").filter(|entry| !entry.is_null());
    let (track_id, title, artist, duration) = if let Some(item) = item {
        let id = super::text_field(item, "id");
        let title = super::text_field(item, "name");
        let artist = item
            .get("artists")
            .and_then(serde_json::Value::as_array)
            .map(|artists| {
                artists
                    .iter()
                    .map(|artist| super::text_field(artist, "name"))
                    .filter(|name| !name.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let duration_ms = item
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        (
            (!id.is_empty()).then_some(id),
            (!title.is_empty()).then_some(title),
            (!artist.is_empty()).then_some(artist),
            (duration_ms > 0).then_some(Duration::from_millis(duration_ms)),
        )
    } else {
        (None, None, None, None)
    };
    let volume = value
        .pointer("/device/volume_percent")
        .and_then(serde_json::Value::as_u64)
        .and_then(|percent| u8::try_from(percent).ok());
    Ok(Some(RemotePlayer {
        playing,
        progress,
        track_id,
        title,
        artist,
        duration,
        volume,
    }))
}

fn device_query(device_id: &str) -> Vec<(String, String)> {
    if device_id.is_empty() {
        Vec::new()
    } else {
        vec![("device_id".to_string(), device_id.to_string())]
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Web API player. The device list is cached. Position between polls is estimated.
pub(super) struct ConnectEngine {
    account: SpotifyAccount,
    preferred: Option<String>,
    devices: Vec<ConnectDevice>,
    devices_at: Option<Instant>,
    track_id: Option<String>,
    playing: bool,
    pause_requested: bool,
    position: Duration,
    anchor_at: Instant,
    duration: Option<Duration>,
    finished: bool,
    failed: Option<String>,
    volume_set: Option<u8>,
    observed_at: Option<Instant>,
    hold_until: Option<Instant>,
    remote_playing: Option<bool>,
    remote_id: Option<String>,
    remote_title: Option<String>,
    remote_artist: Option<String>,
    remote_duration: Option<Duration>,
    logged_playing: Option<bool>,
    logged_track: Option<String>,
}

impl ConnectEngine {
    pub(super) fn new(account: SpotifyAccount) -> Self {
        Self {
            account,
            preferred: None,
            devices: Vec::new(),
            devices_at: None,
            track_id: None,
            playing: false,
            pause_requested: false,
            position: Duration::ZERO,
            anchor_at: Instant::now(),
            duration: None,
            finished: false,
            failed: None,
            volume_set: None,
            observed_at: None,
            hold_until: None,
            remote_playing: None,
            remote_id: None,
            remote_title: None,
            remote_artist: None,
            remote_duration: None,
            logged_playing: None,
            logged_track: None,
        }
    }

    pub(super) fn pause_result(&mut self) -> Result<(), String> {
        self.pause();
        match self.failed.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn displayed_position(&self) -> Duration {
        if self.playing {
            self.position.saturating_add(self.anchor_at.elapsed())
        } else {
            self.position
        }
    }

    fn hold_active(&self) -> bool {
        self.hold_until.is_some_and(|until| Instant::now() < until)
    }

    fn selected(&self) -> Option<&ConnectDevice> {
        choose_device(&self.devices, self.preferred.as_deref())
    }

    fn token(&self) -> Result<String, ConnectError> {
        match self.account.bearer() {
            Ok(Some(token)) => Ok(token),
            Ok(None) => Err(ConnectError::SignedOut),
            Err(error) => Err(ConnectError::Failed(error.to_string())),
        }
    }

    fn send(
        &self,
        verb: Verb,
        url: &str,
        query: &[(String, String)],
        body: Option<serde_json::Value>,
    ) -> Result<(u16, String), ConnectError> {
        let token = self.token()?;
        let first = self
            .account
            .request(verb, url, query, body.clone(), &token)
            .map_err(connect_transport)?;
        let outcome = classify(first.status, &first.body);
        if outcome != Outcome::Unauthorized {
            log_outcome(verb.name(), url, first.status, outcome_label(outcome));
            return self.finish(first.status, first.body, outcome);
        }
        log_outcome(verb.name(), url, first.status, "refreshing");
        let refreshed = self
            .account
            .force_refresh()
            .map_err(connect_transport)?
            .ok_or(ConnectError::SignedOut)?;
        let second = self
            .account
            .request(verb, url, query, body, &refreshed)
            .map_err(connect_transport)?;
        let outcome = classify(second.status, &second.body);
        let label = match outcome {
            Outcome::Unauthorized => "unauthorized after refresh",
            other => outcome_label(other),
        };
        log_outcome(verb.name(), url, second.status, label);
        self.finish(second.status, second.body, outcome)
    }

    fn finish(
        &self,
        status: u16,
        body: String,
        outcome: Outcome,
    ) -> Result<(u16, String), ConnectError> {
        match outcome {
            Outcome::Ok => Ok((status, body)),
            Outcome::Unauthorized => Err(ConnectError::Unauthorized),
            Outcome::PremiumRequired => Err(ConnectError::PremiumRequired),
            Outcome::MissingScope => {
                self.account.clear_playback_grant();
                Err(ConnectError::MissingScope)
            }
            Outcome::NoDevice => Err(ConnectError::NoDevice),
            Outcome::Failed => {
                let message = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| super::api_error_message(&value))
                    .unwrap_or_else(|| format!("Spotify replied {status}"));
                Err(ConnectError::Failed(message))
            }
        }
    }

    fn command(
        &self,
        verb: Verb,
        url: &str,
        query: Vec<(String, String)>,
        body: Option<serde_json::Value>,
    ) -> Result<(), String> {
        self.send(verb, url, &query, body)
            .map(|_| ())
            .map_err(|error| error.message())
    }

    fn reload_devices(&mut self) -> Result<Vec<ConnectDevice>, String> {
        let (_status, body) = self
            .send(Verb::Get, DEVICES_URL, &[], None)
            .map_err(|error| error.message())?;
        let devices = devices_from_json(&body).inspect_err(|_error| {
            log::info!("spotify connect: device list could not be read");
        })?;
        log::info!("spotify connect: {} devices", devices.len());
        self.devices = devices.clone();
        self.devices_at = Some(Instant::now());
        if let Some(volume) = self.selected().and_then(|device| device.volume) {
            self.volume_set = Some(volume);
        }
        Ok(devices)
    }

    fn play_uri(&self, device_id: &str, track_id: &str, position: Duration) -> Result<(), String> {
        let body = serde_json::json!({
            "uris": [format!("spotify:track:{track_id}")],
            "position_ms": millis(position),
        });
        self.command(Verb::Put, PLAY_URL, device_query(device_id), Some(body))
    }

    fn resume(&self, device_id: &str) -> Result<(), String> {
        self.command(
            Verb::Put,
            PLAY_URL,
            device_query(device_id),
            Some(serde_json::json!({})),
        )
    }

    fn pause_device(&self) -> Result<(), String> {
        let device_id = self
            .selected()
            .map(|device| device.id.clone())
            .unwrap_or_default();
        match self.command(Verb::Put, PAUSE_URL, device_query(&device_id), None) {
            Err(message) if message == ERR_NO_DEVICE => {
                log::info!("spotify connect: pause found no active device");
                Ok(())
            }
            other => other,
        }
    }

    fn skip(&self, url: &str) -> Result<(), String> {
        let device_id = self
            .selected()
            .map(|device| device.id.clone())
            .unwrap_or_default();
        self.command(Verb::Post, url, device_query(&device_id), None)
    }

    fn observe(&mut self) {
        self.observed_at = Some(Instant::now());
        let body = match self.send(Verb::Get, PLAYER_URL, &[], None) {
            Ok((_status, body)) => body,
            Err(ConnectError::NoDevice) if self.pause_requested => {
                self.remote_playing = Some(false);
                return;
            }
            Err(error) => {
                self.failed = Some(error.message());
                return;
            }
        };
        let snapshot = match player_from_json(&body) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::info!("spotify connect: player state could not be read");
                self.failed = Some(error);
                return;
            }
        };
        let Some(snapshot) = snapshot else {
            self.remote_playing = Some(false);
            self.remote_id = None;
            self.note_transport(false, None);
            if self.playing
                && !self.pause_requested
                && !self.hold_active()
                && self.near_end(self.displayed_position())
            {
                self.playing = false;
                self.finished = true;
                log::info!("spotify connect: track ended");
            }
            return;
        };
        self.position = snapshot.progress;
        self.anchor_at = Instant::now();
        self.remote_playing = Some(snapshot.playing);
        self.remote_id = snapshot.track_id.clone();
        self.remote_title = snapshot.title;
        self.remote_artist = snapshot.artist;
        self.remote_duration = snapshot.duration;
        if snapshot.duration.is_some() {
            self.duration = snapshot.duration;
        }
        if let Some(volume) = snapshot.volume {
            self.volume_set = Some(volume);
        }
        self.note_transport(snapshot.playing, snapshot.track_id.as_deref());
        let ended = !snapshot.playing
            && !self.pause_requested
            && !self.hold_active()
            && self.near_end(snapshot.progress);
        if ended {
            self.playing = false;
            self.finished = true;
            log::info!("spotify connect: track ended");
            return;
        }
        if snapshot.playing {
            self.pause_requested = false;
        }
        self.playing = snapshot.playing;
    }

    fn note_transport(&mut self, playing: bool, track_id: Option<&str>) {
        let track_changed = track_id.is_some() && self.logged_track.as_deref() != track_id;
        let play_changed = self.logged_playing != Some(playing);
        if track_changed || play_changed {
            self.logged_playing = Some(playing);
            if let Some(track_id) = track_id {
                self.logged_track = Some(track_id.to_string());
            }
            let id = track_id.unwrap_or("-");
            if playing {
                log::info!("spotify connect: player reports {id} playing");
            } else {
                log::info!("spotify connect: player reports {id} paused");
            }
        }
    }

    fn near_end(&self, progress: Duration) -> bool {
        let Some(duration) = self.duration else {
            return false;
        };
        progress.saturating_add(END_SLACK) >= duration
    }

    fn arm_hold(&mut self) {
        self.hold_until = Some(Instant::now() + HOLD);
    }
}

fn connect_transport(error: Error) -> ConnectError {
    let message = error.to_string();
    if message.contains("limiting requests") {
        ConnectError::RateLimited(message)
    } else {
        ConnectError::Failed(message)
    }
}

impl SpotifyEngine for ConnectEngine {
    fn can_play(&self) -> bool {
        self.account.is_premium()
    }

    fn load(&mut self, track_id: &str) -> Result<(), String> {
        self.failed = None;
        self.finished = false;
        if !self.account.has_playback_grant() {
            log::info!("spotify connect: play skipped, missing playback scope");
            return Err(ERR_MISSING_SCOPE.to_string());
        }
        if super::track_uri(track_id).is_err() {
            return Err("this Spotify id cannot be played".to_string());
        }
        let devices = self.reload_devices()?;
        let Some(device) = choose_device(&devices, self.preferred.as_deref()) else {
            log::info!("spotify connect: play skipped, no device");
            return Err(ERR_NO_DEVICE.to_string());
        };
        let device_id = device.id.clone();
        let device_name = device.name.clone();
        self.play_uri(&device_id, track_id, Duration::ZERO)?;
        self.track_id = Some(track_id.to_string());
        self.playing = true;
        self.pause_requested = false;
        self.position = Duration::ZERO;
        self.anchor_at = Instant::now();
        self.arm_hold();
        self.observed_at = None;
        log::info!("spotify connect: playing {track_id} on {device_name}");
        Ok(())
    }

    fn play(&mut self) {
        self.failed = None;
        self.finished = false;
        self.pause_requested = false;
        self.playing = true;
        self.anchor_at = Instant::now();
        self.arm_hold();
        let Some(device_id) = self.selected().map(|device| device.id.clone()) else {
            log::info!("spotify connect: resume skipped, no device");
            self.failed = Some(ERR_NO_DEVICE.to_string());
            self.playing = false;
            return;
        };
        if let Err(error) = self.resume(&device_id) {
            self.playing = false;
            self.failed = Some(error);
        }
    }

    fn pause(&mut self) {
        self.pause_requested = true;
        self.position = self.displayed_position();
        self.anchor_at = Instant::now();
        self.playing = false;
        self.arm_hold();
        if let Err(error) = self.pause_device() {
            self.failed = Some(error);
        }
    }

    fn stop(&mut self) {
        self.pause_requested = true;
        self.playing = false;
        self.finished = false;
        self.track_id = None;
        if let Err(error) = self.pause_device() {
            log::info!("spotify connect: stop: {error}");
        }
        self.failed = None;
    }

    fn poll(&mut self) -> SpotifyFeed {
        let due = self.observed_at.is_none_or(|at| at.elapsed() >= POLL_EVERY);
        if self.track_id.is_some() && due {
            self.observe();
        }
        let finished = self.finished;
        self.finished = false;
        SpotifyFeed {
            position: self.displayed_position(),
            finished,
            failed: self.failed.take(),
            playing: self.remote_playing.unwrap_or(false),
            remote_playing: self.remote_playing,
            remote_id: self.remote_id.clone(),
            remote_title: self.remote_title.clone(),
            remote_artist: self.remote_artist.clone(),
            remote_duration: self.remote_duration,
        }
    }

    fn refresh_devices(&mut self) -> bool {
        if !self.account.is_premium() || !self.account.has_playback_grant() {
            return false;
        }
        if self
            .devices_at
            .is_some_and(|at| at.elapsed() < DEVICE_EVERY)
        {
            return false;
        }
        let before = self.devices.clone();
        match self.reload_devices() {
            Ok(devices) => devices != before,
            Err(error) => {
                log::info!("spotify connect: device refresh failed: {error}");
                self.devices_at = Some(Instant::now());
                false
            }
        }
    }

    fn devices(&self) -> Vec<DeviceOption> {
        let selected = self.selected().map(|device| device.id.clone());
        self.devices
            .iter()
            .filter(|device| !device.id.is_empty())
            .map(|device| DeviceOption {
                id: device.id.clone(),
                name: device.name.clone(),
                selected: selected.as_deref() == Some(device.id.as_str()),
            })
            .collect()
    }

    fn volume(&self) -> Option<u8> {
        let device = self.selected()?;
        if !device.supports_volume {
            return None;
        }
        Some(self.volume_set.or(device.volume).unwrap_or(50))
    }

    fn select_device(&mut self, id: &str) {
        if let Some(device) = self.devices.iter().find(|device| device.id == id) {
            if device.restricted {
                log::info!("spotify connect: selected device cannot be controlled");
                return;
            }
            log::info!("spotify connect: selected {}", device.name);
        } else if !self.devices.is_empty() {
            log::info!("spotify connect: selected device is not in the list");
            return;
        } else {
            log::info!("spotify connect: will use the chosen device");
        }
        self.preferred = Some(id.to_string());
        if self.playing
            && let Some(track_id) = self.track_id.clone()
        {
            let position = self.displayed_position();
            if let Err(error) = self.play_uri(id, &track_id, position) {
                self.failed = Some(error);
            } else {
                self.arm_hold();
                self.anchor_at = Instant::now();
            }
        }
    }

    fn seek(&mut self, position: Duration) {
        let Some(device_id) = self.selected().map(|device| device.id.clone()) else {
            log::info!("spotify connect: seek skipped, no device");
            self.failed = Some(ERR_NO_DEVICE.to_string());
            return;
        };
        let mut query = device_query(&device_id);
        query.push(("position_ms".to_string(), millis(position).to_string()));
        self.position = position;
        self.anchor_at = Instant::now();
        self.arm_hold();
        if let Err(error) = self.command(Verb::Put, SEEK_URL, query, None) {
            self.failed = Some(error);
        }
    }

    fn set_volume(&mut self, percent: u8) {
        let percent = percent.min(100);
        let Some(device) = self.selected() else {
            log::info!("spotify connect: volume skipped, no device");
            return;
        };
        if !device.supports_volume {
            log::info!("spotify connect: volume skipped, device does not support it");
            return;
        }
        let device_id = device.id.clone();
        self.volume_set = Some(percent);
        let mut query = device_query(&device_id);
        query.push(("volume_percent".to_string(), percent.to_string()));
        if let Err(error) = self.command(Verb::Put, VOLUME_URL, query, None) {
            self.failed = Some(error);
        }
    }

    fn device_next(&mut self) {
        self.arm_hold();
        self.pause_requested = false;
        self.observed_at = None;
        if let Err(error) = self.skip(NEXT_URL) {
            self.failed = Some(error);
        }
    }

    fn device_previous(&mut self) {
        self.arm_hold();
        self.pause_requested = false;
        self.observed_at = None;
        if let Err(error) = self.skip(PREVIOUS_URL) {
            self.failed = Some(error);
        }
    }
}

/// Device list for `--selftest`. The account must already hold a token.
pub(super) fn list_devices(account: &SpotifyAccount) -> Result<Vec<ConnectDevice>, String> {
    let mut engine = ConnectEngine::new(account.clone());
    engine.reload_devices()
}

/// `GET /v1/me/player` reports this track as playing.
#[cfg(test)]
fn player_reports_track(body: &str, track_id: &str) -> bool {
    match player_from_json(body) {
        Ok(Some(snapshot)) => snapshot.playing && snapshot.track_id.as_deref() == Some(track_id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, kind: &str, active: bool, restricted: bool) -> ConnectDevice {
        ConnectDevice {
            id: id.to_string(),
            name: id.to_string(),
            kind: kind.to_string(),
            active,
            restricted,
            volume: Some(40),
            supports_volume: true,
        }
    }

    #[test]
    fn device_choice_prefers_the_user_then_the_active_then_a_computer() {
        let devices = vec![
            device("phone", "Smartphone", false, false),
            device("desk", "Computer", false, false),
            device("living", "Speaker", true, false),
            device("locked", "Computer", true, true),
        ];
        assert_eq!(choose_device(&devices, Some("desk")).unwrap().id, "desk");
        assert_eq!(choose_device(&devices, None).unwrap().id, "living");
        assert_eq!(
            choose_device(&devices, Some("missing")).unwrap().id,
            "living"
        );

        let only_computers = vec![
            device("locked", "Computer", true, true),
            device("laptop", "Computer", false, false),
            device("old", "computer", false, false),
        ];
        assert_eq!(choose_device(&only_computers, None).unwrap().id, "laptop");

        let phone_only = vec![device("phone", "Smartphone", false, false)];
        assert!(choose_device(&phone_only, None).is_none());
        assert_eq!(device_for_playback(&phone_only, None).unwrap().id, "phone");
        assert!(choose_device(&[], None).is_none());
    }

    #[test]
    fn device_json_keeps_controllable_rows() {
        let body = r#"{
            "devices": [
                {
                    "id": "computer-1",
                    "is_active": false,
                    "is_restricted": false,
                    "name": "Desk",
                    "type": "Computer",
                    "volume_percent": 70,
                    "supports_volume": true
                },
                {
                    "id": "",
                    "name": "blank",
                    "type": "Speaker"
                },
                {
                    "id": "phone-1",
                    "is_active": true,
                    "is_restricted": false,
                    "name": "",
                    "type": "Smartphone",
                    "supports_volume": false
                }
            ]
        }"#;
        let devices = devices_from_json(body).expect("devices");
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "Desk");
        assert_eq!(devices[0].volume, Some(70));
        assert!(devices[0].supports_volume);
        assert_eq!(devices[1].name, "Spotify");
        assert!(devices[1].active);
        assert!(!devices[1].supports_volume);
        assert!(devices_from_json(r#"{"error":{"status":401,"message":"expired"}}"#).is_err());
    }

    #[test]
    fn player_json_reads_the_current_track_and_an_empty_body() {
        assert_eq!(player_from_json("").unwrap(), None);
        assert_eq!(player_from_json("null").unwrap(), None);
        let body = r#"{
            "is_playing": true,
            "progress_ms": 1500,
            "device": {"volume_percent": 20},
            "item": {
                "id": "5sWHDYs0csV6RS48xBl0tH",
                "name": "Night Drive",
                "duration_ms": 180000,
                "artists": [{"name": "Ada Vega"}, {"name": "Jonah"}]
            }
        }"#;
        let snapshot = player_from_json(body).unwrap().unwrap();
        assert!(snapshot.playing);
        assert_eq!(snapshot.progress, Duration::from_millis(1500));
        assert_eq!(snapshot.track_id.as_deref(), Some("5sWHDYs0csV6RS48xBl0tH"));
        assert_eq!(snapshot.title.as_deref(), Some("Night Drive"));
        assert_eq!(snapshot.artist.as_deref(), Some("Ada Vega, Jonah"));
        assert_eq!(snapshot.duration, Some(Duration::from_secs(180)));
        assert_eq!(snapshot.volume, Some(20));
        assert!(player_reports_track(body, "5sWHDYs0csV6RS48xBl0tH"));
        assert!(!player_reports_track(body, "other"));
    }

    #[test]
    fn status_classification_matches_the_player_errors() {
        assert_eq!(classify(204, ""), Outcome::Ok);
        assert_eq!(classify(202, ""), Outcome::Ok);
        assert_eq!(classify(401, ""), Outcome::Unauthorized);
        assert_eq!(
            classify(
                403,
                r#"{"error":{"status":403,"message":"Insufficient client scope"}}"#
            ),
            Outcome::MissingScope
        );
        assert_eq!(
            classify(
                403,
                r#"{"error":{"status":403,"message":"Player command failed: Premium required"}}"#
            ),
            Outcome::PremiumRequired
        );
        assert_eq!(classify(404, ""), Outcome::NoDevice);
        assert_eq!(classify(500, ""), Outcome::Failed);
        assert_eq!(outcome_label(Outcome::NoDevice), "no active device");
    }
}
