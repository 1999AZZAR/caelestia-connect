use super::mpris::{OBJECT, PLAYER, PROPERTIES, ROOT};
use crate::state::State;
use anyhow::Result;
use dbus::{
    arg::{self, AppendAll, PropMap, Variant},
    blocking::{LocalConnection, Proxy},
    message::MatchRule,
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
};

mod art;

pub const PLUGIN: &str = "mpriscontrol";
const DBUS: &str = "org.freedesktop.DBus";
const PREFIX: &str = "org.mpris.MediaPlayer2.";
const DBUS_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_PLAYERS: usize = 16;

pub struct Request {
    pub id: String,
    pub serial: u64,
    pub body: Value,
}

pub struct PayloadPeer {
    pub address: Ipv4Addr,
    pub local_address: Ipv4Addr,
    pub certificate: Vec<u8>,
}

type Targets = BTreeSet<(String, u64)>;

#[derive(Default)]
struct Signals {
    names_changed: bool,
    changed: BTreeSet<String>,
    seeked: BTreeMap<String, i64>,
}

struct Player {
    service: String,
    owner: String,
}

#[derive(Default)]
struct CallPlayback {
    restore: Option<bool>,
    seen: BTreeSet<String>,
    paused: BTreeMap<String, &'static str>,
}

impl CallPlayback {
    fn update(
        &mut self,
        bus: &LocalConnection,
        players: &BTreeMap<String, Player>,
        requested: Option<bool>,
    ) {
        let Some(restore) = requested else {
            if self.restore.take() == Some(true) {
                for (owner, expected) in &self.paused {
                    let proxy = bus.with_proxy(owner.as_str(), OBJECT, DBUS_TIMEOUT);
                    if property::<String>(&proxy, PLAYER, "PlaybackStatus").as_deref()
                        == Some(expected)
                    {
                        call(&proxy, "Play", ());
                    }
                }
            }
            self.seen.clear();
            self.paused.clear();
            return;
        };
        self.restore = Some(self.restore.unwrap_or(true) && restore);
        self.seen
            .retain(|owner| players.values().any(|player| player.owner == *owner));
        self.paused.retain(|owner, _| self.seen.contains(owner));
        for player in players
            .values()
            .filter(|player| !self.seen.contains(&player.owner))
            .collect::<Vec<_>>()
        {
            self.seen.insert(player.owner.clone());
            let proxy = bus.with_proxy(player.owner.as_str(), OBJECT, DBUS_TIMEOUT);
            let Some(props) = properties(&proxy) else {
                continue;
            };
            if arg::prop_cast::<String>(&props, "PlaybackStatus")
                .is_none_or(|status| status != "Playing")
                || arg::prop_cast::<bool>(&props, "CanControl") == Some(&false)
            {
                continue;
            }
            let (method, reached) = if arg::prop_cast::<bool>(&props, "CanPause") == Some(&false) {
                ("Stop", "Stopped")
            } else {
                ("Pause", "Paused")
            };
            if proxy.method_call::<(), _, _, _>(PLAYER, method, ()).is_ok() {
                self.paused.insert(player.owner.clone(), reached);
            }
        }
    }
}

fn wanted(service: &str) -> bool {
    service.strip_prefix(PREFIX).is_some_and(|name| {
        !name.starts_with("caelestiaconnect.")
            && !name.starts_with("kdeconnect.")
            && name != "playerctld"
    })
}

fn peer_list(players: &BTreeMap<String, Player>) -> Value {
    json!({"playerList": players.keys().collect::<Vec<_>>(), "supportAlbumArtPayload": true})
}

fn subscribe(bus: &LocalConnection) -> Result<Rc<RefCell<Signals>>> {
    let signals = Rc::new(RefCell::new(Signals::default()));
    let registry = signals.clone();
    let mut names = MatchRule::new_signal(DBUS, "NameOwnerChanged");
    names.sender = Some(DBUS.into());
    bus.add_match(
        names,
        move |(name, _, _): (String, String, String), _, _| {
            if wanted(&name) {
                registry.borrow_mut().names_changed = true;
            }
            true
        },
    )?;

    let changes = signals.clone();
    let mut properties = MatchRule::new_signal(PROPERTIES, "PropertiesChanged");
    properties.path = Some(OBJECT.into());
    bus.add_match(
        properties,
        move |(interface, _, _): (String, PropMap, Vec<String>), _, message| {
            if interface == PLAYER {
                let mut signals = changes.borrow_mut();
                if let Some(sender) = message
                    .sender()
                    .filter(|_| signals.changed.len() < MAX_PLAYERS * 2)
                {
                    signals.changed.insert(sender.to_string());
                }
            }
            true
        },
    )?;

    let playback = signals.clone();
    let mut seeked = MatchRule::new_signal(PLAYER, "Seeked");
    seeked.path = Some(OBJECT.into());
    bus.add_match(seeked, move |(position,): (i64,), _, message| {
        let mut signals = playback.borrow_mut();
        if let Some(sender) = message
            .sender()
            .filter(|_| signals.seeked.len() < MAX_PLAYERS * 2)
        {
            signals.seeked.insert(sender.to_string(), position.max(0));
        }
        true
    })?;
    Ok(signals)
}

fn refresh(bus: &LocalConnection, players: &mut BTreeMap<String, Player>) -> Result<bool> {
    let registry = bus.with_proxy(DBUS, "/org/freedesktop/DBus", DBUS_TIMEOUT);
    let (names,): (Vec<String>,) = registry.method_call(DBUS, "ListNames", ())?;
    let mut names: Vec<_> = names.into_iter().filter(|name| wanted(name)).collect();
    names.sort();
    names.truncate(MAX_PLAYERS);
    let mut changed = false;
    players.retain(|_, player| {
        let retain = names.contains(&player.service);
        changed |= !retain;
        retain
    });
    for service in names {
        let Ok((owner,)): Result<(String,), _> =
            registry.method_call(DBUS, "GetNameOwner", (&service,))
        else {
            continue;
        };
        if players
            .values()
            .any(|player| player.service == service && player.owner == owner)
        {
            continue;
        }
        players.retain(|_, player| player.service != service);
        let proxy = bus.with_proxy(owner.as_str(), OBJECT, DBUS_TIMEOUT);
        let identity = property::<String>(&proxy, ROOT, "Identity").unwrap_or_default();
        let identity: String = identity
            .chars()
            .filter(|ch| !ch.is_control())
            .take(60)
            .collect();
        let identity = if identity.is_empty() {
            service.trim_start_matches(PREFIX)
        } else {
            &identity
        };
        let mut name = identity.to_owned();
        let mut index = 2;
        while players.contains_key(&name) {
            name = format!("{identity} [{index}]");
            index += 1;
        }
        players.insert(name, Player { service, owner });
        changed = true;
    }
    Ok(changed)
}

fn property<T: for<'a> arg::Get<'a>>(
    proxy: &Proxy<'_, &LocalConnection>,
    interface: &str,
    name: &str,
) -> Option<T> {
    let (value,): (Variant<T>,) = proxy
        .method_call(PROPERTIES, "Get", (interface, name))
        .ok()?;
    Some(value.0)
}

fn properties(proxy: &Proxy<'_, &LocalConnection>) -> Option<PropMap> {
    let (props,): (PropMap,) = proxy.method_call(PROPERTIES, "GetAll", (PLAYER,)).ok()?;
    Some(props)
}

fn set<T: arg::Append + arg::Arg>(proxy: &Proxy<'_, &LocalConnection>, key: &str, value: T) {
    let _: Result<(), _> = proxy.method_call(PROPERTIES, "Set", (PLAYER, key, Variant(value)));
}

fn call(proxy: &Proxy<'_, &LocalConnection>, method: &str, args: impl AppendAll) {
    let _: Result<(), _> = proxy.method_call(PLAYER, method, args);
}

fn text(props: &PropMap, key: &str, limit: usize) -> String {
    arg::prop_cast::<String>(props, key)
        .map(|value| value.chars().take(limit).collect())
        .unwrap_or_default()
}

fn info(name: &str, props: &PropMap, now_playing: bool, volume: bool, position: i64) -> Value {
    let mut result = json!({"player": name});
    if now_playing {
        let meta = arg::prop_cast::<PropMap>(props, "Metadata");
        let mut title = meta
            .map(|meta| text(meta, "xesam:title", 256))
            .unwrap_or_default();
        let mut album = meta
            .map(|meta| text(meta, "xesam:album", 256))
            .unwrap_or_default();
        let artist: String = meta
            .and_then(|meta| meta.get("xesam:artist"))
            .and_then(|value| value.0.as_iter())
            .map(|items| {
                items
                    .filter_map(|item| item.as_str())
                    .take(16)
                    .collect::<Vec<_>>()
                    .join(", ")
                    .chars()
                    .take(256)
                    .collect()
            })
            .unwrap_or_default();
        let url = meta
            .map(|meta| text(meta, "xesam:url", 1024))
            .unwrap_or_default();
        if title.is_empty() && artist.is_empty() {
            if let Some(path) = art::file_path(&url) {
                title = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if album.is_empty() {
                    album = path
                        .parent()
                        .and_then(|path| path.file_name())
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }
        result["title"] = json!(title);
        result["artist"] = json!(artist);
        result["album"] = json!(album);
        result["url"] = json!(url);
        result["length"] = json!(
            meta.and_then(|meta| arg::prop_cast::<i64>(meta, "mpris:length"))
                .map_or(-1, |v| v / 1000)
        );
        result["albumArtUrl"] = json!(art_url(props));
        result["pos"] = json!(position.max(0) / 1000);
        result["isPlaying"] = json!(
            arg::prop_cast::<String>(props, "PlaybackStatus").is_some_and(|v| v == "Playing")
        );
        for (name, property) in [
            ("canPause", "CanPause"),
            ("canPlay", "CanPlay"),
            ("canGoNext", "CanGoNext"),
            ("canGoPrevious", "CanGoPrevious"),
            ("canSeek", "CanSeek"),
        ] {
            result[name] = json!(
                arg::prop_cast::<bool>(props, property)
                    .copied()
                    .unwrap_or(false)
            );
        }
        if let Some(value) = arg::prop_cast::<String>(props, "LoopStatus")
            .filter(|value| matches!(value.as_str(), "None" | "Track" | "Playlist"))
        {
            result["loopStatus"] = json!(value);
        }
        if let Some(value) = arg::prop_cast::<bool>(props, "Shuffle") {
            result["shuffle"] = json!(value);
        }
    }
    if volume {
        let volume = arg::prop_cast::<f64>(props, "Volume")
            .copied()
            .unwrap_or(1.0);
        result["volume"] = json!((volume.clamp(0.0, 1.0) * 100.0).round() as u64);
    }
    result
}

fn art_url(props: &PropMap) -> String {
    arg::prop_cast::<PropMap>(props, "Metadata")
        .and_then(|meta| arg::prop_cast::<String>(meta, "mpris:artUrl"))
        .filter(|url| url.len() <= 2048 && !url.chars().any(char::is_control))
        .cloned()
        .unwrap_or_default()
}

fn media_commands(
    proxy: &Proxy<'_, &LocalConnection>,
    props: &PropMap,
    request: &Value,
    enabled: impl Fn() -> bool,
) {
    if arg::prop_cast::<bool>(props, "CanControl") == Some(&false) {
        return;
    }
    if let Some(action @ ("Play" | "Pause" | "PlayPause" | "Stop" | "Next" | "Previous")) =
        request.get("action").and_then(Value::as_str)
    {
        if enabled() {
            call(proxy, action, ());
        }
    }
    if let Some(value @ ("None" | "Track" | "Playlist")) =
        request.get("setLoopStatus").and_then(Value::as_str)
    {
        if enabled() {
            set(proxy, "LoopStatus", value);
        }
    }
    if let Some(value) = request.get("setShuffle").and_then(Value::as_bool) {
        if enabled() {
            set(proxy, "Shuffle", value);
        }
    }
    if let Some(value) = request
        .get("setVolume")
        .and_then(Value::as_u64)
        .filter(|value| *value <= 100)
    {
        if enabled() {
            set(proxy, "Volume", value as f64 / 100.0);
        }
    }
    if let Some(offset) = request.get("Seek").and_then(Value::as_i64) {
        if enabled() {
            call(proxy, "Seek", (offset,));
        }
    }
    if let Some(position) = request
        .get("SetPosition")
        .and_then(Value::as_i64)
        .filter(|value| *value >= 0)
    {
        let current = property::<i64>(proxy, PLAYER, "Position").unwrap_or_default();
        if enabled() {
            call(
                proxy,
                "Seek",
                (position.saturating_mul(1000).saturating_sub(current),),
            );
        }
    }
}

fn handle(
    bus: &LocalConnection,
    players: &BTreeMap<String, Player>,
    state: &Arc<State>,
    runtime: &Handle,
    request: Request,
) {
    let Request { id, serial, body } = request;
    if !state.local_media_allowed(&id, serial) {
        return;
    }
    let found = body
        .get("player")
        .and_then(Value::as_str)
        .and_then(|name| players.get(name).map(|player| (name, player)));
    if body.get("requestPlayerList").and_then(Value::as_bool) == Some(true)
        || (found.is_none() && body.get("albumArtUrl").is_none())
    {
        state.send_local_media(&id, serial, peer_list(players));
    }
    let Some((name, player)) = found else {
        return;
    };
    let proxy = bus.with_proxy(player.owner.as_str(), OBJECT, DBUS_TIMEOUT);
    let Some(props) = properties(&proxy) else {
        return;
    };
    if let Some(url) = body.get("albumArtUrl") {
        if let Some(url) = url
            .as_str()
            .filter(|url| !url.is_empty() && *url == art_url(&props))
        {
            art::send(runtime, state, id, serial, name.to_owned(), url.to_owned());
        }
        return;
    }
    media_commands(&proxy, &props, &body, || {
        state.local_media_allowed(&id, serial)
    });
    let now_playing = body.get("requestNowPlaying").and_then(Value::as_bool) == Some(true);
    let volume = body.get("requestVolume").and_then(Value::as_bool) == Some(true);
    if now_playing || volume {
        let props = properties(&proxy).unwrap_or(props);
        let position = arg::prop_cast::<i64>(&props, "Position")
            .copied()
            .or_else(|| property(&proxy, PLAYER, "Position"))
            .unwrap_or_default();
        state.send_local_media(
            &id,
            serial,
            info(name, &props, now_playing, volume, position),
        );
    }
}

fn broadcast(state: &State, targets: &Targets, body: &Value) {
    for (id, serial) in targets {
        state.send_local_media(id, *serial, body.clone());
    }
}

fn serve(
    state: &Weak<State>,
    runtime: &Handle,
    requests: &mut mpsc::Receiver<Request>,
    alive: &watch::Receiver<()>,
) -> Result<()> {
    let bus = LocalConnection::new_session()?;
    let signals = subscribe(&bus)?;
    let mut players = BTreeMap::new();
    let mut previous = Targets::new();
    let mut last_scan = None::<Instant>;
    let mut calls = CallPlayback::default();
    while alive.has_changed().is_ok() {
        let Some(state) = state.upgrade() else {
            break;
        };
        let targets = state.local_media_targets();
        let pause = state.call_audio().pause;
        let pending = signals.take();
        if targets.is_empty() && pause.is_none() {
            players.clear();
            last_scan = None;
        } else {
            let refresh_requested = pending.names_changed
                || last_scan.is_none_or(|last| last.elapsed() > Duration::from_secs(10));
            let list_changed = if refresh_requested {
                last_scan = Some(Instant::now());
                refresh(&bus, &mut players).unwrap_or(false)
            } else {
                false
            };
            if list_changed {
                broadcast(&state, &targets, &peer_list(&players));
            } else if targets != previous {
                let body = peer_list(&players);
                for (id, serial) in targets.difference(&previous) {
                    state.send_local_media(id, *serial, body.clone());
                }
            }
            for (name, player) in &players {
                if pending.changed.contains(&player.owner) {
                    let proxy = bus.with_proxy(player.owner.as_str(), OBJECT, DBUS_TIMEOUT);
                    if let Some(props) = properties(&proxy) {
                        let position = arg::prop_cast::<i64>(&props, "Position")
                            .copied()
                            .or_else(|| property(&proxy, PLAYER, "Position"))
                            .unwrap_or_default();
                        broadcast(&state, &targets, &info(name, &props, true, true, position));
                    }
                }
                if let Some(position) = pending.seeked.get(&player.owner) {
                    broadcast(
                        &state,
                        &targets,
                        &json!({"player": name, "pos": position / 1000}),
                    );
                }
            }
        }
        calls.update(&bus, &players, pause);
        previous = targets;
        for _ in 0..4 {
            let Ok(request) = requests.try_recv() else {
                break;
            };
            handle(&bus, &players, &state, runtime, request);
        }
        drop(state);
        bus.process(Duration::from_millis(75))?;
    }
    calls.update(&bus, &players, None);
    Ok(())
}

pub async fn run(state: &Arc<State>, mut requests: mpsc::Receiver<Request>) {
    let (keep_alive, alive) = watch::channel(());
    let state = Arc::downgrade(state);
    let runtime = Handle::current();
    let _ = tokio::task::spawn_blocking(move || {
        while alive.has_changed().is_ok() {
            match serve(&state, &runtime, &mut requests, &alive) {
                Ok(()) => break,
                Err(error) => {
                    eprintln!("Desktop media controls could not connect to D-Bus: {error}")
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    })
    .await;
    drop(keep_alive);
    std::future::pending::<()>().await;
}
