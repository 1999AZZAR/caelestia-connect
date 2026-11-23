use super::{Artwork, LoopStatus, Player};
use crate::state::State;
use anyhow::{Result, ensure};
use dbus::{
    Message, MethodErr, Path,
    arg::{PropMap, Variant},
    blocking::{LocalConnection, stdintf::org_freedesktop_dbus::RequestNameReply},
    channel::{MatchingReceiver, Sender},
    message::MatchRule,
};
use dbus_crossroads::Crossroads;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Semaphore, watch};

pub(super) const ROOT: &str = "org.mpris.MediaPlayer2";
pub(super) const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
pub(super) const OBJECT: &str = "/org/mpris/MediaPlayer2";
pub(super) const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const MAX_EXPORTS: usize = 8;
static EXPORT_CONNECTIONS: Semaphore = Semaphore::const_new(MAX_EXPORTS * 2);

#[derive(Clone)]
pub struct ExportedPlayer {
    device: String,
    serial: u64,
    identity: String,
    name: String,
    player: Player,
}

fn digest(parts: &[&str]) -> String {
    let mut sha = openssl::sha::Sha256::new();
    for part in parts {
        sha.update(&(part.len() as u64).to_be_bytes());
        sha.update(part.as_bytes());
    }
    sha.finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl ExportedPlayer {
    pub(super) fn new(
        device: &str,
        device_name: &str,
        serial: u64,
        name: &str,
        player: Player,
    ) -> Self {
        Self {
            device: device.to_owned(),
            serial,
            identity: format!("{name} ({device_name})"),
            name: name.to_owned(),
            player,
        }
    }

    fn bus_name(&self, local_device: &str) -> String {
        format!(
            "{ROOT}.caelestiaconnect.rust_{}",
            digest(&[local_device, &self.device, &self.name])
        )
    }
}

struct MediaObject {
    current: ExportedPlayer,
    state: std::sync::Weak<State>,
}

impl MediaObject {
    fn command(&self, body: Value) -> Result<(), MethodErr> {
        let current = &self.current;
        self.state
            .upgrade()
            .ok_or_else(|| MethodErr::failed(&"daemon stopped"))?
            .send_shell_media(&current.device, current.serial, &current.name, body)
            .map_err(|error| MethodErr::failed(&error.to_string()))
    }

    fn track(&self) -> Path<'static> {
        let p = &self.current.player;
        Path::new(format!(
            "/org/mpris/MediaPlayer2/Track/{}",
            digest(&[&p.title, &p.artist, &p.album, &p.art_url])
        ))
        .expect("track path uses a hexadecimal digest")
    }

    fn metadata(&self) -> PropMap {
        let p = &self.current.player;
        let mut meta = PropMap::new();
        meta.insert("mpris:trackid".into(), Variant(Box::new(self.track())));
        meta.insert("xesam:title".into(), Variant(Box::new(p.title.clone())));
        meta.insert(
            "xesam:artist".into(),
            Variant(Box::new(vec![p.artist.clone()])),
        );
        meta.insert("xesam:album".into(), Variant(Box::new(p.album.clone())));
        if p.length > 0 {
            meta.insert("mpris:length".into(), Variant(Box::new(micros(p.length))));
        }
        if let Artwork::Ready(path) = &p.art {
            let path = path.to_string_lossy();
            let uri = format!(
                "file://{}",
                percent_encoding::utf8_percent_encode(&path, URI_ESCAPED)
            );
            meta.insert("mpris:artUrl".into(), Variant(Box::new(uri)));
        }
        meta
    }

    fn status(&self) -> String {
        if self.current.player.playing {
            "Playing".into()
        } else {
            "Paused".into()
        }
    }

    fn seek(&self, position: i64) -> Result<(), MethodErr> {
        let p = &self.current.player;
        if p.can_seek {
            let ms = position.max(0) as u64 / 1000;
            self.command(json!({"SetPosition": if p.length > 0 { ms.min(p.length) } else { ms }}))?;
        }
        Ok(())
    }

    fn properties(&self) -> PropMap {
        let p = &self.current.player;
        let mut props = PropMap::new();
        props.insert("Metadata".into(), Variant(Box::new(self.metadata())));
        props.insert("PlaybackStatus".into(), Variant(Box::new(self.status())));
        props.insert("Volume".into(), Variant(Box::new(p.volume as f64 / 100.0)));
        props.insert(
            "LoopStatus".into(),
            Variant(Box::new(
                p.loop_status.unwrap_or(LoopStatus::None).name().to_owned(),
            )),
        );
        props.insert(
            "Shuffle".into(),
            Variant(Box::new(p.shuffle.unwrap_or(false))),
        );
        for (property, value) in [
            ("CanGoNext", p.can_go_next),
            ("CanGoPrevious", p.can_go_previous),
            ("CanPlay", p.can_play),
            ("CanPause", p.can_pause),
            ("CanSeek", p.can_seek),
        ] {
            props.insert(property.into(), Variant(Box::new(value)));
        }
        props
    }
}

const URI_ESCAPED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'#')
    .add(b'?')
    .add(b'"');

fn micros(ms: u64) -> i64 {
    ms.saturating_mul(1000).min(i64::MAX as u64) as i64
}

pub async fn run_exports(state: &Arc<State>) {
    let mut events = state.subscribe();
    let mut connections: BTreeMap<String, watch::Sender<ExportedPlayer>> = BTreeMap::new();
    let mut retry = tokio::time::interval(Duration::from_secs(3));
    loop {
        let mut active = BTreeSet::new();
        for player in state.exported_players(MAX_EXPORTS) {
            let name = player.bus_name(&state.config.id);
            active.insert(name.clone());
            if let Some(sender) = connections.get(&name).filter(|sender| !sender.is_closed()) {
                sender.send_replace(player);
            } else if let Ok(permit) = EXPORT_CONNECTIONS.try_acquire() {
                let (sender, updates) = watch::channel(player);
                connections.insert(name.clone(), sender);
                let state = Arc::downgrade(state);
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    if let Err(error) = serve(&name, state, updates) {
                        eprintln!("Could not export MPRIS player: {error}");
                    }
                });
            }
        }
        connections.retain(|name, _| active.contains(name));
        tokio::select! {
            _ = retry.tick() => (),
            _ = events.changed() => (),
        }
    }
}

fn serve(
    name: &str,
    state: std::sync::Weak<State>,
    mut updates: watch::Receiver<ExportedPlayer>,
) -> Result<()> {
    let path = Path::from(OBJECT);
    let bus = LocalConnection::new_session()?;
    let mut service = Crossroads::new();
    let root = service.register(ROOT, |builder| {
        for method in ["Raise", "Quit"] {
            builder.method(method, (), (), |_, _: &mut MediaObject, ()| Ok(()));
        }
        builder
            .property("Identity")
            .get(|_, data: &mut MediaObject| Ok(data.current.identity.clone()));
        builder
            .property("DesktopEntry")
            .get(|_, _: &mut MediaObject| Ok("org.caelestia.Connect".to_owned()));
        for property in [
            "CanQuit",
            "CanRaise",
            "HasTrackList",
            "CanSetFullscreen",
            "Fullscreen",
        ] {
            builder
                .property(property)
                .get(|_, _: &mut MediaObject| Ok(false));
        }
        for property in ["SupportedUriSchemes", "SupportedMimeTypes"] {
            builder
                .property(property)
                .get(|_, _: &mut MediaObject| Ok(Vec::<String>::new()));
        }
    });
    let player = service.register(PLAYER, |builder| {
        builder.signal::<(i64,), _>("Seeked", ("Position",));
        builder.method(
            "OpenUri",
            ("Uri",),
            (),
            |_, _: &mut MediaObject, (_uri,): (String,)| Ok(()),
        );
        for verb in ["Next", "Previous", "Pause", "PlayPause", "Stop", "Play"] {
            builder.method(verb, (), (), move |_, data: &mut MediaObject, ()| {
                let p = &data.current.player;
                let supported = match verb {
                    "Next" => p.can_go_next,
                    "Previous" => p.can_go_previous,
                    "Pause" => p.can_pause,
                    "Play" => p.can_play,
                    "PlayPause" => {
                        if p.playing {
                            p.can_pause
                        } else {
                            p.can_play
                        }
                    }
                    _ => true,
                };
                if supported {
                    data.command(json!({"action": verb}))?;
                }
                Ok(())
            });
        }
        builder.method(
            "Seek",
            ("Offset",),
            (),
            |_, data: &mut MediaObject, (offset,): (i64,)| {
                data.seek(micros(data.current.player.position()).saturating_add(offset))
            },
        );
        builder.method(
            "SetPosition",
            ("TrackId", "Position"),
            (),
            |_, data: &mut MediaObject, (track, position): (Path<'static>, i64)| {
                if track == data.track() {
                    data.seek(position)?;
                }
                Ok(())
            },
        );
        builder
            .property("PlaybackStatus")
            .get(|_, data: &mut MediaObject| Ok(data.status()));
        builder
            .property("Metadata")
            .get(|_, data: &mut MediaObject| Ok(data.metadata()));
        builder
            .property("Position")
            .emits_changed_false()
            .get(|_, data: &mut MediaObject| Ok(micros(data.current.player.position())));
        builder
            .property("CanControl")
            .get(|_, _: &mut MediaObject| Ok(true));
        for (property, flag) in [
            (
                "CanGoNext",
                (|p: &Player| p.can_go_next) as fn(&Player) -> bool,
            ),
            ("CanGoPrevious", |p: &Player| p.can_go_previous),
            ("CanPlay", |p: &Player| p.can_play),
            ("CanPause", |p: &Player| p.can_pause),
            ("CanSeek", |p: &Player| p.can_seek),
        ] {
            builder
                .property(property)
                .get(move |_, data: &mut MediaObject| Ok(flag(&data.current.player)));
        }
        builder
            .property("Volume")
            .get(|_, data: &mut MediaObject| Ok(data.current.player.volume as f64 / 100.0))
            .set(|_, data: &mut MediaObject, value: f64| {
                if !value.is_finite() {
                    return Err(MethodErr::invalid_arg(&"Volume"));
                }
                data.command(json!({"setVolume": (value.clamp(0.0, 1.0) * 100.0).round() as u64}))?;
                Ok(None)
            });
        builder
            .property("LoopStatus")
            .get(|_, data: &mut MediaObject| {
                Ok(data
                    .current
                    .player
                    .loop_status
                    .unwrap_or(LoopStatus::None)
                    .name()
                    .to_owned())
            })
            .set(|_, data: &mut MediaObject, value: String| {
                if !matches!(value.as_str(), "None" | "Track" | "Playlist") {
                    return Err(MethodErr::invalid_arg(&"LoopStatus"));
                }
                if data.current.player.loop_status.is_some() {
                    data.command(json!({"setLoopStatus": value}))?;
                }
                Ok(None)
            });
        builder
            .property("Shuffle")
            .get(|_, data: &mut MediaObject| Ok(data.current.player.shuffle.unwrap_or(false)))
            .set(|_, data: &mut MediaObject, value: bool| {
                if data.current.player.shuffle.is_some() {
                    data.command(json!({"setShuffle": value}))?;
                }
                Ok(None)
            });
        builder
            .property("Rate")
            .get(|_, _: &mut MediaObject| Ok(1.0_f64))
            .set(|_, _: &mut MediaObject, value: f64| {
                if value != 1.0 {
                    return Err(MethodErr::invalid_arg(&"Rate"));
                }
                Ok(None)
            });
        for property in ["MinimumRate", "MaximumRate"] {
            builder
                .property(property)
                .get(|_, _: &mut MediaObject| Ok(1.0_f64));
        }
    });
    let current = MediaObject {
        current: updates.borrow_and_update().clone(),
        state,
    };
    service.insert(path.clone(), &[root, player], current);
    ensure!(
        bus.request_name(name, false, false, true)? == RequestNameReply::PrimaryOwner,
        "MPRIS bus name is in use"
    );
    let mut method = MatchRule::new_method_call();
    method.path = Some(path.clone());
    let service = std::rc::Rc::new(std::cell::RefCell::new(service));
    let receiver = service.clone();
    bus.start_receive(
        method,
        Box::new(move |message, bus| {
            let _ = receiver.borrow_mut().handle_message(message, bus);
            true
        }),
    );
    while let Ok(changed) = updates.has_changed() {
        if changed {
            let mut service = service.borrow_mut();
            let object: &mut MediaObject = service
                .data_mut(&path)
                .expect("player object is registered");
            let old = &object.current.player;
            let next = updates.borrow_and_update();
            let mut before = old.snapshot("");
            let mut after = next.player.snapshot("");
            before.as_object_mut().unwrap().remove("position");
            after.as_object_mut().unwrap().remove("position");
            let seeked = next.player.position().abs_diff(old.position()) > 750;
            object.current = next.clone();
            if before != after {
                let changed = Message::new_signal(OBJECT, PROPERTIES, "PropertiesChanged")
                    .expect("valid D-Bus signal")
                    .append3(PLAYER, object.properties(), Vec::<String>::new());
                let _ = bus.send(changed);
            }
            if seeked {
                let signal = Message::new_signal(OBJECT, PLAYER, "Seeked")
                    .expect("valid D-Bus signal")
                    .append1(micros(object.current.player.position()));
                let _ = bus.send(signal);
            }
        }
        bus.process(Duration::from_millis(125))?;
    }
    Ok(())
}
