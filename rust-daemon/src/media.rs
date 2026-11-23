use crate::{lan::authenticated_stream, wire::Packet};
use anyhow::{Context, Result, bail, ensure};
use openssl::ssl::SslContext;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    net::SocketAddrV4,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tempfile::TempPath;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

pub mod local;
mod mpris;
pub use mpris::{ExportedPlayer, run_exports};

pub const INFO: &str = "kdeconnect.mpris";
pub const REQUEST: &str = "kdeconnect.mpris.request";
pub const PLUGIN: &str = "mprisremote";
const IDLE: Duration = Duration::from_secs(600);
const MAX_PLAYERS: usize = 16;
pub const MAX_ART_SIZE: u64 = 8 * 1024 * 1024;

#[derive(Default)]
pub struct Media {
    players: BTreeMap<String, Player>,
    current: String,
}

#[derive(Clone)]
struct Player {
    title: String,
    artist: String,
    album: String,
    length: u64,
    position: u64,
    updated: Instant,
    volume: u64,
    playing: bool,
    last_playing: Option<Instant>,
    can_seek: bool,
    can_play: bool,
    can_pause: bool,
    can_go_next: bool,
    can_go_previous: bool,
    shuffle: Option<bool>,
    loop_status: Option<LoopStatus>,
    art_url: String,
    art: Artwork,
}

#[derive(Clone, Default)]
enum Artwork {
    #[default]
    Requested,
    Fetching(Arc<()>),
    Ready(Arc<TempPath>),
}

#[derive(Clone, Copy)]
enum LoopStatus {
    None,
    Track,
    Playlist,
}

impl LoopStatus {
    fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }
}

impl Default for Player {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            length: 0,
            position: 0,
            updated: Instant::now(),
            volume: 50,
            playing: false,
            last_playing: None,
            can_seek: false,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: true,
            shuffle: None,
            loop_status: None,
            art_url: String::new(),
            art: Artwork::default(),
        }
    }
}

impl Player {
    fn position(&self) -> u64 {
        let position = if self.playing {
            self.position
                .saturating_add(self.updated.elapsed().as_millis() as u64)
        } else {
            self.position
        };
        if self.length == 0 {
            position
        } else {
            position.min(self.length)
        }
    }

    fn visible(&self, hide_idle: bool, now: Instant) -> bool {
        !hide_idle
            || self.playing
            || self
                .last_playing
                .is_some_and(|last| now.duration_since(last) < IDLE)
    }

    fn update(&mut self, body: &Value, now: Instant) {
        for (key, text) in [
            ("title", &mut self.title),
            ("artist", &mut self.artist),
            ("album", &mut self.album),
        ] {
            if let Some(value) = body.get(key).and_then(Value::as_str) {
                *text = value.chars().take(256).collect();
            }
        }
        let is_playing = body
            .get("isPlaying")
            .and_then(Value::as_bool)
            .unwrap_or(self.playing);
        if let Some(position) = number(body, "pos") {
            self.position = position;
            self.updated = now;
        } else if is_playing != self.playing {
            self.position = self.position();
            self.updated = now;
        }
        if self.playing || is_playing {
            self.last_playing = Some(now);
        }
        self.playing = is_playing;
        if let Some(length) = number(body, "length") {
            self.length = length;
        }
        if let Some(volume) = number(body, "volume") {
            self.volume = volume.min(100);
        }
        for (key, target) in [
            ("canSeek", &mut self.can_seek),
            ("canPlay", &mut self.can_play),
            ("canPause", &mut self.can_pause),
            ("canGoNext", &mut self.can_go_next),
            ("canGoPrevious", &mut self.can_go_previous),
        ] {
            if let Some(value) = body.get(key).and_then(Value::as_bool) {
                *target = value;
            }
        }
        if let Some(value) = body.get("shuffle").and_then(Value::as_bool) {
            self.shuffle = Some(value);
        }
        if let Some(value) = body.get("loopStatus").and_then(Value::as_str) {
            self.loop_status = match value {
                "None" => Some(LoopStatus::None),
                "Track" => Some(LoopStatus::Track),
                "Playlist" => Some(LoopStatus::Playlist),
                _ => self.loop_status,
            };
        }
    }

    fn snapshot(&self, name: &str) -> Value {
        let art = match &self.art {
            Artwork::Ready(path) => path.to_string_lossy(),
            _ => "".into(),
        };
        json!({
            "name": name, "title": self.title, "artist": self.artist, "album": self.album, "art": art,
            "position": self.position(), "length": self.length, "volume": self.volume, "playing": self.playing,
            "canSeek": self.can_seek, "canPlay": self.can_play, "canPause": self.can_pause,
            "canGoNext": self.can_go_next, "canGoPrevious": self.can_go_previous,
            "loopStatus": self.loop_status.unwrap_or(LoopStatus::None).name(), "hasLoop": self.loop_status.is_some(),
            "shuffle": self.shuffle.unwrap_or(false), "hasShuffle": self.shuffle.is_some(),
        })
    }
}

fn number(body: &Value, key: &str) -> Option<u64> {
    body.get(key)?.as_i64().map(|value| value.max(0) as u64)
}

fn name(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control))
}

pub fn request_list() -> Packet {
    Packet::new(REQUEST, json!({"requestPlayerList": true}))
}

fn request_player(name: &str) -> Packet {
    Packet::new(
        REQUEST,
        json!({"player": name, "requestNowPlaying": true, "requestVolume": true}),
    )
}

impl Media {
    pub fn receive(&mut self, body: &Value) -> Vec<Packet> {
        let mut requests = Vec::new();
        let now = Instant::now();
        if let Some(list) = body.get("playerList").and_then(Value::as_array) {
            let names: Vec<_> = list.iter().filter_map(name).take(MAX_PLAYERS).collect();
            self.players
                .retain(|name, _| names.contains(&name.as_str()));
            for name in names {
                if !self.players.contains_key(name) {
                    self.players.insert(name.to_owned(), Player::default());
                    requests.push(request_player(name));
                }
            }
        }
        if let Some(name) = body.get("player").and_then(name) {
            let room = self.players.len() < MAX_PLAYERS;
            let was_playing = self
                .players
                .get(&self.current)
                .is_some_and(|player| player.playing);
            if let Some(player) = if room {
                Some(self.players.entry(name.to_owned()).or_default())
            } else {
                self.players.get_mut(name)
            } {
                let started = !player.playing;
                player.update(body, now);
                if let Some(url) = body
                    .get("albumArtUrl")
                    .and_then(Value::as_str)
                    .filter(|url| url.len() <= 2048 && !url.chars().any(char::is_control))
                    .filter(|url| *url != player.art_url)
                {
                    player.art_url = url.to_owned();
                    player.art = Artwork::Requested;
                    if !url.is_empty() {
                        requests.push(Packet::new(
                            REQUEST,
                            json!({"player": name, "albumArtUrl": url}),
                        ));
                    }
                }
                if started && player.playing && !was_playing {
                    self.current = name.to_owned();
                }
            }
        }
        requests
    }

    pub fn start_art(&mut self, url: &str) -> Option<Arc<()>> {
        if url.is_empty() || !self.players.values().any(|player| player.art_url == url) {
            return None;
        }
        if self
            .players
            .values()
            .any(|player| player.art_url == url && matches!(player.art, Artwork::Fetching(_)))
        {
            return None;
        }
        let token = Arc::new(());
        let mut started = false;
        for player in self
            .players
            .values_mut()
            .filter(|player| player.art_url == url)
        {
            if matches!(player.art, Artwork::Requested) {
                player.art = Artwork::Fetching(token.clone());
                started = true;
            }
        }
        started.then_some(token)
    }

    pub fn finish_art(&mut self, url: &str, token: &Arc<()>, path: Option<TempPath>) -> bool {
        let path = path.map(Arc::new);
        let mut changed = false;
        for player in self
            .players
            .values_mut()
            .filter(|player| player.art_url == url)
        {
            if matches!(&player.art, Artwork::Fetching(current) if Arc::ptr_eq(current, token)) {
                player.art = match &path {
                    Some(path) => {
                        changed = true;
                        Artwork::Ready(path.clone())
                    }
                    None => Artwork::Requested,
                };
            }
        }
        changed
    }

    pub fn exports<'a>(
        &'a self,
        device: &'a str,
        device_name: &'a str,
        serial: u64,
        hide_idle: bool,
    ) -> impl Iterator<Item = ExportedPlayer> + 'a {
        let now = Instant::now();
        self.players
            .iter()
            .filter(move |(_, player)| player.visible(hide_idle, now))
            .map(move |(name, player)| {
                ExportedPlayer::new(device, device_name, serial, name, player.clone())
            })
    }

    pub fn accepts_command(&self, name: &str, hide_idle: bool) -> bool {
        self.players
            .get(name)
            .is_some_and(|player| player.visible(hide_idle, Instant::now()))
    }

    pub fn playing_player(&self) -> Option<&str> {
        self.players
            .iter()
            .find(|(_, player)| player.playing)
            .map(|(name, _)| name.as_str())
    }

    pub fn is_paused(&self, name: &str) -> bool {
        self.players.get(name).is_some_and(|player| !player.playing)
    }

    pub fn matches_app(&self, app: &str) -> bool {
        if app.is_empty() {
            return false;
        }
        let app = app.to_lowercase();
        self.players
            .keys()
            .any(|name| name.to_lowercase().contains(&app))
    }

    fn current(&self, hide_idle: bool, now: Instant) -> &str {
        if self
            .players
            .get(&self.current)
            .is_some_and(|player| player.visible(hide_idle, now))
        {
            return &self.current;
        }
        self.players
            .iter()
            .filter(|(_, player)| player.visible(hide_idle, now))
            .max_by_key(|(_, player)| (player.playing, player.last_playing))
            .map(|(name, _)| name.as_str())
            .unwrap_or_default()
    }

    pub fn snapshot(&self, hide_idle: bool) -> Value {
        let now = Instant::now();
        let mut players = Vec::new();
        let mut hidden = Vec::new();
        for (name, player) in &self.players {
            if player.visible(hide_idle, now) {
                players.push(player.snapshot(name));
            } else {
                hidden.push(name);
            }
        }
        json!({"current": self.current(hide_idle, now), "players": players, "hidden": hidden})
    }

    pub fn action(
        &self,
        action: &str,
        args: Option<&Map<String, Value>>,
        hide_idle: bool,
    ) -> Result<Option<Packet>> {
        if action == "refresh" {
            return Ok(Some(request_list()));
        }
        let current = self.current(hide_idle, Instant::now());
        let target = args
            .and_then(|args| args.get("player"))
            .map(|name| name.as_str().context("expected player name"))
            .transpose()?
            .filter(|name| !name.is_empty())
            .unwrap_or(current);
        let Some(player) = self.players.get(target) else {
            return Ok(None);
        };
        let packet = match action {
            "select_player" => return Ok(Some(request_player(target))),
            "media_action" => match args
                .and_then(|args| args.get("action"))
                .and_then(Value::as_str)
            {
                Some(command @ ("Play" | "Pause" | "PlayPause" | "Stop" | "Next" | "Previous")) => {
                    json!({"action": command})
                }
                Some("shuffle") if player.shuffle.is_some() => {
                    json!({"setShuffle": !player.shuffle.unwrap_or(false)})
                }
                Some("repeat") if player.loop_status.is_some() => {
                    let next = match player.loop_status {
                        Some(LoopStatus::None) => LoopStatus::Track,
                        Some(LoopStatus::Track) => LoopStatus::Playlist,
                        _ => LoopStatus::None,
                    };
                    json!({"setLoopStatus": next.name()})
                }
                _ => return Ok(None),
            },
            "set_position" => {
                let position = args
                    .and_then(|args| args.get("position"))
                    .and_then(Value::as_u64)
                    .context("invalid playback position")?;
                json!({"SetPosition": position.min(if player.length == 0 { i64::MAX as u64 } else { player.length })})
            }
            "set_volume" => {
                let volume = args
                    .and_then(|args| args.get("volume"))
                    .and_then(Value::as_u64)
                    .filter(|volume| *volume <= 100)
                    .context("volume must be between 0 and 100")?;
                json!({"setVolume": volume})
            }
            _ => bail!("unknown media action"),
        };
        let mut packet = packet;
        packet["player"] = json!(target);
        Ok(Some(Packet::new(REQUEST, packet)))
    }

    pub fn selected(&mut self, name: &str) {
        if self.players.contains_key(name) {
            self.current = name.to_owned();
        }
    }

    pub fn expire_idle(&mut self) -> bool {
        let mut expired = false;
        for player in self.players.values_mut() {
            if !player.playing
                && player
                    .last_playing
                    .is_some_and(|last| last.elapsed() >= IDLE)
            {
                player.last_playing = None;
                expired = true;
            }
        }
        expired
    }
}

pub async fn download_art(
    directory: &Path,
    context: &SslContext,
    address: SocketAddrV4,
    id: &str,
    certificate: Vec<u8>,
    size: u64,
) -> Result<TempPath> {
    timeout(Duration::from_secs(20), async {
        let socket = TcpStream::connect(address).await?;
        let mut tls = authenticated_stream(context, socket, id, Some(certificate), false).await?;
        let mut header = [0; 12];
        tls.read_exact(&mut header).await?;
        ensure!(valid_art_header(&header), "unsupported album art format");
        let file = tempfile::NamedTempFile::with_prefix_in("album-", directory)?;
        let mut writer = tokio::fs::File::from_std(file.as_file().try_clone()?);
        writer.write_all(&header).await?;
        let remaining = size - header.len() as u64;
        ensure!(
            tokio::io::copy(&mut (&mut tls).take(remaining), &mut writer).await? == remaining,
            "incomplete album art"
        );
        writer.flush().await?;
        Ok(file.into_temp_path())
    })
    .await
    .context("album art transfer timed out")?
}

fn valid_art_header(header: &[u8]) -> bool {
    header.starts_with(b"\x89PNG\r\n\x1a\n")
        || header.starts_with(b"\xff\xd8\xff")
        || header.starts_with(b"GIF87a")
        || header.starts_with(b"GIF89a")
        || (header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP"))
}
