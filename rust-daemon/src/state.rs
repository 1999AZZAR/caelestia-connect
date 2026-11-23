mod audio;
mod commands;
mod messaging;
mod notifications;
mod pairing;
pub mod telephony;

pub use pairing::PairCommand;

use crate::{
    android, audio as volume,
    clipboard::{self, TEXT},
    commands as command,
    config::{Config, TrustStore, Trusted, validate_incoming_path},
    desktop::{self, BATTERY, CONNECTIVITY, RING},
    media::{self, Media, local},
    phone_audio,
    share::{self, SharePeer},
    transfers::Transfers,
    wire::{self, PAIR, PING, Packet, SHARE},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

const MAX_LINKS: usize = 64;
const MAX_UNPAIRED: usize = 16;
static ART_DOWNLOADS: Semaphore = Semaphore::const_new(4);

pub struct Transmission {
    pub packet: Packet,
    pub completed: Option<oneshot::Sender<Result<()>>>,
}

pub struct Peer {
    pub certificate: openssl::x509::X509,
    pub port: u16,
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub version: u32,
    pub fingerprint: String,
    pub address: Ipv4Addr,
    pub local_address: Ipv4Addr,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

impl Peer {
    fn accepts(&self, packet: &str) -> bool {
        self.incoming_capabilities.iter().any(|kind| kind == packet)
    }

    fn sends(&self, packet: &str) -> bool {
        self.outgoing_capabilities.iter().any(|kind| kind == packet)
    }
}

struct Link {
    peer: Peer,
    serial: u64,
    preferred: bool,
    outbound: mpsc::Sender<Transmission>,
    pending: Option<pairing::Pending>,
    pair_error: &'static str,
    status: BTreeMap<&'static str, Value>,
    notifications: crate::notifications::Inbox,
    media: Media,
    remote_sinks: volume::Sinks,
    call_activity: Option<telephony::Activity>,
    remote_commands: command::Remote,
}

struct Links {
    active: BTreeMap<String, Link>,
    last_seen: BTreeMap<String, u64>,
    trust: TrustStore,
    blocked: BTreeSet<String>,
    next_serial: u64,
}

impl Links {
    fn media_link(&mut self, id: &str) -> Option<(&mut Link, &Trusted)> {
        let trusted = self
            .trust
            .get(id)
            .filter(|trusted| !self.blocked.contains(id) && trusted.enabled(media::PLUGIN))?;
        let link = self
            .active
            .get_mut(id)
            .filter(|link| link.peer.supports_media())?;
        Some((link, trusted))
    }

    fn start_media(&mut self, id: &str) {
        if let Some((link, _)) = self.media_link(id) {
            link.queue(media::request_list());
        }
    }

    fn local_media_link(&self, id: &str, serial: Option<u64>) -> Option<(&Link, &Trusted)> {
        let trusted = self
            .trust
            .get(id)
            .filter(|entry| !self.blocked.contains(id) && entry.enabled(local::PLUGIN))?;
        let link = self.active.get(id).filter(|link| {
            serial.is_none_or(|serial| link.serial == serial) && link.peer.controls_local_media()
        })?;
        Some((link, trusted))
    }
}

impl Peer {
    fn supports_media(&self) -> bool {
        self.accepts(media::REQUEST) && self.sends(media::INFO)
    }

    fn controls_local_media(&self) -> bool {
        self.accepts(media::INFO) && self.sends(media::REQUEST)
    }
}

impl Link {
    fn queue(&self, packet: Packet) -> bool {
        self.outbound
            .try_send(Transmission {
                packet,
                completed: None,
            })
            .is_ok()
    }
}

pub struct Workers {
    pub local_media: mpsc::Sender<local::Request>,
    pub phone_audio: mpsc::Sender<phone_audio::Request>,
    pub messaging: std::sync::mpsc::SyncSender<crate::messaging::Request>,
    pub popups: mpsc::Sender<crate::notifications::Popup>,
    pub audio: mpsc::Sender<volume::Request>,
}

pub struct State {
    pub config: Config,
    pub tcp_port: u16,
    pub transfers: Transfers,
    links: Mutex<Links>,
    revision: watch::Sender<u64>,
    clipboard_updates: watch::Sender<Option<String>>,
    artwork_directory: PathBuf,
    local_media: mpsc::Sender<local::Request>,
    phone_audio: mpsc::Sender<phone_audio::Request>,
    audio_devices: Mutex<Vec<phone_audio::Phone>>,
    messaging: std::sync::mpsc::SyncSender<crate::messaging::Request>,
    popups: mpsc::Sender<crate::notifications::Popup>,
    audio: mpsc::Sender<volume::Request>,
    pub command_jobs: command::Jobs,
    pub android: android::Manager,
}

impl State {
    pub fn new(
        config: Config,
        trust: TrustStore,
        tcp_port: u16,
        artwork_directory: PathBuf,
        workers: Workers,
    ) -> Self {
        let revision = watch::channel(0).0;
        Self {
            config,
            tcp_port,
            links: Mutex::new(Links {
                active: BTreeMap::new(),
                last_seen: BTreeMap::new(),
                trust,
                blocked: BTreeSet::new(),
                next_serial: 0,
            }),
            transfers: Transfers::new(revision.clone()),
            revision,
            clipboard_updates: watch::channel(None).0,
            artwork_directory,
            local_media: workers.local_media,
            phone_audio: workers.phone_audio,
            audio_devices: Mutex::new(Vec::new()),
            messaging: workers.messaging,
            popups: workers.popups,
            audio: workers.audio,
            command_jobs: Default::default(),
            android: Default::default(),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub fn wants_idle_inhibition(&self) -> bool {
        let links = self.links.lock().unwrap();
        links.active.iter().any(|(id, link)| {
            link.peer.accepts(PING)
                && !links.blocked.contains(id)
                && links
                    .trust
                    .get(id)
                    .is_some_and(|entry| entry.enabled(crate::idle::PLUGIN))
        })
    }

    pub(crate) fn changed(&self) {
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub fn android_device(&self, id: &str) -> Result<android::Device> {
        let links = self.links.lock().unwrap();
        let trusted = links
            .trust
            .get(id)
            .filter(|trusted| trusted.enabled(android::PLUGIN) && !links.blocked.contains(id))
            .context("Screen mirroring is unavailable for this device.")?;
        let peer = links.active.get(id).map(|link| &link.peer);
        let kind = peer.map_or(trusted.device_type.as_str(), |peer| {
            peer.device_type.as_str()
        });
        ensure!(
            android::supports(kind),
            "This device does not offer Android mirroring."
        );
        Ok(android::Device {
            name: peer.map_or(&trusted.name, |peer| &peer.name).clone(),
            address: peer.map(|peer| peer.address).or(trusted.address),
            options: trusted.android.clone(),
            phoneaudio: trusted.enabled(phone_audio::PLUGIN),
        })
    }

    pub fn register(
        &self,
        mut peer: Peer,
        dialed: bool,
        outbound: mpsc::Sender<Transmission>,
    ) -> Result<u64> {
        let mut links = self.links.lock().unwrap();
        ensure!(
            !links.blocked.contains(&peer.id),
            "could not persist withdrawn pairing"
        );
        let trusted = links.trust.get(&peer.id);
        let paired = trusted.is_some();
        if let Some(trusted) = trusted {
            ensure!(
                peer.version >= trusted.version
                    && peer.certificate.to_der()? == trusted.certificate,
                "peer no longer matches its saved identity",
            );
        }
        if !wire::PORTS.contains(&peer.port) {
            peer.port = trusted.map_or(wire::DISCOVERY_PORT, |entry| entry.port);
        }
        let preferred = dialed == (self.config.id < peer.id);
        let replacing = if let Some(old) = links.active.get(&peer.id) {
            ensure!(
                old.peer.fingerprint == peer.fingerprint,
                "device identity changed"
            );
            let address_changed = paired && old.peer.address != peer.address;
            ensure!(
                address_changed || preferred && !old.preferred,
                "device already connected"
            );
            true
        } else {
            ensure!(links.active.len() < MAX_LINKS, "too many connected devices");
            if !paired {
                let unpaired = links
                    .active
                    .keys()
                    .filter(|id| links.trust.get(id).is_none())
                    .count();
                ensure!(unpaired < MAX_UNPAIRED, "too many unpaired devices");
            }
            false
        };
        links.next_serial += 1;
        let serial = links.next_serial;
        if paired {
            if let Err(error) = links
                .trust
                .remember_endpoint(&peer.id, peer.address, peer.port)
            {
                eprintln!("Could not save the paired LAN endpoint: {error:#}");
            }
            links
                .last_seen
                .insert(peer.id.clone(), wire::timestamp_ms() / 1000);
        }
        let id = peer.id.clone();
        links.active.insert(
            id.clone(),
            Link {
                peer,
                serial,
                preferred,
                outbound,
                pending: None,
                pair_error: "",
                status: BTreeMap::new(),
                notifications: Default::default(),
                media: Media::default(),
                remote_sinks: Default::default(),
                call_activity: None,
                remote_commands: Default::default(),
            },
        );
        links.start_media(&id);
        drop(links);
        if replacing {
            self.transfers.cancel_device(&id);
        }
        self.start_messaging(&id);
        self.start_notifications(&id);
        self.request_remote_sinks(&id);
        self.start_commands(&id);
        self.changed();
        Ok(serial)
    }

    pub fn disconnect(&self, id: &str, serial: u64) -> bool {
        let mut links = self.links.lock().unwrap();
        if links
            .active
            .get(id)
            .is_some_and(|link| link.serial == serial)
        {
            links.active.remove(id);
            let paired = links.trust.get(id).is_some();
            if paired {
                links
                    .last_seen
                    .insert(id.to_owned(), wire::timestamp_ms() / 1000);
            }
            drop(links);
            self.transfers.cancel_device(id);
            self.changed();
            return paired;
        }
        false
    }

    pub fn can_dial(&self, id: &str) -> bool {
        let links = self.links.lock().unwrap();
        !links.blocked.contains(id) && !links.active.contains_key(id)
    }

    pub fn has_pairing(&self, id: &str) -> bool {
        let links = self.links.lock().unwrap();
        !links.blocked.contains(id) && links.trust.get(id).is_some()
    }

    pub fn certificate_pin(&self, id: &str, version: u32) -> Result<Option<Vec<u8>>> {
        let links = self.links.lock().unwrap();
        ensure!(
            !links.blocked.contains(id),
            "could not persist withdrawn pairing"
        );
        let entry = links.trust.get(id);
        ensure!(
            entry.is_none_or(|entry| version >= entry.version),
            "peer attempted a protocol downgrade"
        );
        Ok(entry.map(|entry| entry.certificate.clone()))
    }

    pub fn can_transmit(&self, id: &str, serial: u64, packet: &Packet) -> bool {
        let links = self.links.lock().unwrap();
        let Some(link) = links.active.get(id).filter(|link| link.serial == serial) else {
            return false;
        };
        let trusted = links.trust.get(id).filter(|_| !links.blocked.contains(id));
        match packet.kind.as_str() {
            PING => trusted.is_some_and(|entry| entry.enabled("ping")),
            SHARE => trusted.is_some_and(|entry| entry.enabled("share")),
            BATTERY => trusted.is_some_and(|entry| entry.enabled("battery")),
            RING => trusted.is_some_and(|entry| entry.enabled("findmyphone")),
            TEXT => trusted.is_some_and(|entry| entry.enabled("clipboard")),
            command::LIST | command::OUTPUT => {
                link.peer.accepts(&packet.kind)
                    && link.peer.supports_commands(command::LOCAL)
                    && trusted.is_some_and(|entry| entry.enabled(command::LOCAL))
            }
            command::REQUEST => {
                link.peer.supports_commands(command::REMOTE)
                    && trusted.is_some_and(|entry| entry.enabled(command::REMOTE))
            }
            volume::INFO => {
                link.peer.supports_volume(volume::LOCAL)
                    && trusted.is_some_and(|entry| entry.enabled(volume::LOCAL))
            }
            volume::REQUEST => {
                link.peer.supports_volume(volume::REMOTE)
                    && trusted.is_some_and(|entry| entry.enabled(volume::REMOTE))
            }
            telephony::MUTE => {
                link.peer.accepts(telephony::MUTE)
                    && trusted.is_some_and(|entry| entry.enabled(telephony::PLUGIN))
            }
            media::REQUEST => trusted.is_some_and(|entry| entry.enabled(media::PLUGIN)),
            media::INFO => trusted.is_some_and(|entry| entry.enabled(local::PLUGIN)),
            kind if crate::notifications::OUTGOING.contains(&kind) => {
                link.peer.accepts(kind)
                    && trusted.is_some_and(|entry| entry.enabled(crate::notifications::PLUGIN))
            }
            kind if crate::messaging::outgoing_plugin(kind).is_some() => {
                link.peer.accepts(kind)
                    && trusted.is_some_and(|entry| {
                        crate::messaging::outgoing_plugin(kind)
                            .is_some_and(|plugin| entry.enabled(plugin))
                    })
            }
            PAIR if packet.body.get("pair").and_then(Value::as_bool) == Some(true) => {
                if let Some(timestamp) = packet.body.get("timestamp").and_then(Value::as_u64) {
                    link.pending
                        .as_ref()
                        .is_some_and(|p| p.matches_sent_request(timestamp))
                } else {
                    trusted.is_some()
                }
            }
            _ => true,
        }
    }

    pub fn known_addresses(&self) -> Vec<(String, SocketAddrV4, u32)> {
        let links = self.links.lock().unwrap();
        links
            .trust
            .iter()
            .filter_map(|(id, trusted)| {
                if links.blocked.contains(id) || links.active.contains_key(id) {
                    return None;
                }
                Some((
                    id.clone(),
                    SocketAddrV4::new(trusted.address?, trusted.port),
                    trusted.version.max(wire::VERSION),
                ))
            })
            .collect()
    }

    pub fn share_peer(&self, id: &str, serial: Option<u64>) -> Result<SharePeer> {
        let links = self.links.lock().unwrap();
        let trusted = links
            .trust
            .get(id)
            .filter(|_| !links.blocked.contains(id))
            .context("device is not paired")?;
        let link = links
            .active
            .get(id)
            .filter(|link| serial.is_none_or(|serial| serial == link.serial))
            .context("device is disconnected")?;
        ensure!(
            trusted.enabled("share"),
            "sharing is disabled for this device"
        );
        ensure!(
            serial.is_some() || link.peer.accepts(SHARE),
            "device does not accept sharing"
        );
        Ok(SharePeer {
            serial: link.serial,
            name: link.peer.name.clone(),
            address: link.peer.address,
            local_address: link.peer.local_address,
            certificate: trusted.certificate.clone(),
            directory: share::destination(
                &self.config.incoming_path(&trusted.share_directory),
                &link.peer.name,
            ),
            open_urls: trusted.share_open_urls,
        })
    }

    pub fn set_plugin_setting(
        &self,
        id: &str,
        plugin: &str,
        key: &str,
        value: Value,
    ) -> Result<()> {
        match (plugin, key) {
            (android::PLUGIN, key) => android::validate(key, &value)?,
            ("share", "incomingPath") => {
                validate_incoming_path(value.as_str().context("expected a directory path")?)?
            }
            ("share", "openUrls")
            | (media::PLUGIN, "hideIdle" | "exportMpris")
            | (
                telephony::PAUSE,
                "conditionTalking" | "actionPause" | "actionMute" | "actionResume",
            )
            | (crate::notifications::PLUGIN, "showPopups" | "copyAuthCodes") => {
                ensure!(value.is_boolean(), "expected an on/off value")
            }
            _ => anyhow::bail!("unknown feature setting"),
        }
        let mut links = self.links.lock().unwrap();
        ensure!(!links.blocked.contains(id), "device is not paired");
        links.trust.plugin_setting(id, plugin, key, value)?;
        drop(links);
        self.changed();
        Ok(())
    }

    pub fn set_plugin_enabled(&self, id: &str, plugin: &str, enabled: bool) -> Result<()> {
        ensure!(
            matches!(
                plugin,
                "battery"
                    | "clipboard"
                    | "connectivity_report"
                    | "findmyphone"
                    | "ping"
                    | "share"
                    | media::PLUGIN
                    | local::PLUGIN
                    | phone_audio::PLUGIN
                    | crate::messaging::SMS
                    | crate::messaging::CONTACTS
                    | crate::notifications::PLUGIN
                    | telephony::PLUGIN
                    | volume::LOCAL
                    | volume::REMOTE
                    | telephony::PAUSE
                    | crate::idle::PLUGIN
                    | android::PLUGIN
                    | command::LOCAL
                    | command::REMOTE
            ),
            "plugin is unavailable"
        );
        let mut links = self.links.lock().unwrap();
        ensure!(!links.blocked.contains(id), "device is not paired");
        links.trust.set_enabled(id, plugin, enabled)?;
        if !enabled {
            if let Some(link) = links.active.get_mut(id) {
                link.status.remove(plugin);
                if plugin == media::PLUGIN {
                    link.media = Media::default();
                }
                if plugin == crate::notifications::PLUGIN {
                    link.notifications = Default::default();
                }
                if plugin == volume::REMOTE {
                    link.remote_sinks.clear();
                }
                if plugin == telephony::PAUSE {
                    link.call_activity = None;
                }
                if plugin == command::REMOTE {
                    link.remote_commands = Default::default();
                }
            }
            if plugin == "share" {
                self.transfers.cancel_device(id);
            }
        } else if plugin == media::PLUGIN {
            links.start_media(id);
        }
        drop(links);
        if matches!(plugin, crate::messaging::SMS | crate::messaging::CONTACTS) {
            self.refresh_message_cache(id, enabled);
        }
        if enabled && plugin == crate::notifications::PLUGIN {
            self.start_notifications(id);
        }
        if enabled && plugin == volume::REMOTE {
            self.request_remote_sinks(id);
        }
        if enabled && matches!(plugin, command::LOCAL | command::REMOTE) {
            self.start_commands(id);
        }
        self.changed();
        Ok(())
    }

    pub fn set_name(&self, requested_name: &str) -> Result<String> {
        let old = self.config.name();
        let name = self.config.set_name(requested_name)?;
        if name != old {
            let mut links = self.links.lock().unwrap();
            for id in links.active.keys() {
                self.transfers.cancel_device(id);
            }
            links.active.clear();
        }
        self.changed();
        Ok(name)
    }

    pub fn set_incoming_path(&self, path: &str) -> Result<()> {
        self.config.set_incoming_path(path)?;
        self.changed();
        Ok(())
    }

    pub fn set_custom_address(&self, address: Ipv4Addr, add: bool) -> Result<()> {
        self.config.set_custom_address(address, add)?;
        self.changed();
        Ok(())
    }

    pub fn snapshot(&self) -> Value {
        let links = self.links.lock().unwrap();
        let audio_devices = self.audio_devices.lock().unwrap();
        let ids: BTreeSet<&str> = links
            .trust
            .iter()
            .map(|(id, _)| id)
            .chain(links.active.keys())
            .map(String::as_str)
            .collect();
        let mut devices = Vec::with_capacity(ids.len());
        for id in ids {
            let link = links.active.get(id);
            let peer = link.map(|link| &link.peer);
            let trusted = links.trust.get(id).filter(|_| !links.blocked.contains(id));
            let pending = link
                .and_then(|link| link.pending.as_ref())
                .filter(|pending| pending.is_current());
            let pairing = if trusted.is_some() {
                "paired"
            } else if let Some(pending) = pending {
                pending.state()
            } else {
                "notPaired"
            };
            if peer.is_none() && trusted.is_none() {
                continue;
            }
            let name = peer
                .map(|p| p.name.as_str())
                .or_else(|| trusted.map(|t| t.name.as_str()))
                .unwrap_or(id);
            let device_type = peer
                .map(|p| p.device_type.as_str())
                .or_else(|| trusted.map(|t| t.device_type.as_str()))
                .unwrap_or("unknown");
            let fingerprint = peer
                .map(|p| p.fingerprint.as_str())
                .or_else(|| trusted.map(|t| t.fingerprint.as_str()))
                .unwrap_or_default();
            let mut plugins = Vec::new();
            let mut supported = Vec::new();
            let mut plugin_state = json!({});
            if let Some(trusted) = trusted.filter(|_| android::supports(device_type)) {
                let enabled = trusted.enabled(android::PLUGIN);
                supported.push(android::PLUGIN);
                plugins.push(trusted.android.plugin(enabled));
                if enabled {
                    plugin_state[android::PLUGIN] = self.android.snapshot(id);
                }
            }
            if let (Some(trusted), Some(peer)) = (trusted, peer) {
                if peer.supports_commands(command::LOCAL) {
                    let enabled = trusted.enabled(command::LOCAL);
                    supported.push(command::LOCAL);
                    plugins.push(json!({"id": command::LOCAL, "name": "Run commands", "description": "Run predefined commands on this computer from the device",
                        "enabled": enabled, "loaded": enabled,
                        "settings": [{"key": "commands", "type": "commands", "label": "Commands", "default": command::defaults()}],
                        "values": {"commands": trusted.local_commands}}));
                    if enabled {
                        plugin_state[command::LOCAL] = json!({"commands": command::definitions_snapshot(&trusted.local_commands)});
                    }
                }
                if let Some(link) = link.filter(|link| link.peer.supports_commands(command::REMOTE))
                {
                    let enabled = trusted.enabled(command::REMOTE);
                    supported.push(command::REMOTE);
                    plugins.push(json!({"id": command::REMOTE, "name": "Remote commands", "description": "Trigger commands defined on the device",
                        "enabled": enabled, "loaded": enabled, "settings": [], "values": {}}));
                    if enabled {
                        plugin_state[command::REMOTE] = link.remote_commands.snapshot();
                    }
                }
                if peer.sends(telephony::EVENT) {
                    supported.push(telephony::PAUSE);
                    plugins.push(
                        trusted
                            .pause_music
                            .schema(trusted.enabled(telephony::PAUSE)),
                    );
                }
                for (id, title, description, available) in [
                    (
                        volume::LOCAL,
                        "System volume",
                        "Control this computer's volume from the device",
                        self.config.system_volume && peer.supports_volume(volume::LOCAL),
                    ),
                    (
                        volume::REMOTE,
                        "Remote system volume",
                        "Control the device's volume",
                        peer.supports_volume(volume::REMOTE),
                    ),
                ] {
                    if !available {
                        continue;
                    }
                    let enabled = trusted.enabled(id);
                    supported.push(id);
                    plugins.push(json!({"id": id, "name": title, "description": description,
                        "enabled": enabled, "loaded": enabled, "settings": [], "values": {}}));
                    if id == volume::REMOTE && enabled {
                        if let Some(link) = link {
                            plugin_state[id] =
                                json!({"sinks": link.remote_sinks.values().collect::<Vec<_>>()});
                        }
                    }
                }
                if let Some(link) = link.filter(|link| link.peer.supports_notification_sync()) {
                    let enabled = trusted.enabled(crate::notifications::PLUGIN);
                    supported.push(crate::notifications::PLUGIN);
                    plugins.push(json!({
                        "id": crate::notifications::PLUGIN, "name": "Notification sync",
                        "description": "Show the device's notifications on this computer",
                        "enabled": enabled, "loaded": enabled,
                        "settings": [
                            {"key": "showPopups", "type": "bool", "label": "Show notifications on this computer", "default": true},
                            {"key": "copyAuthCodes", "type": "bool", "label": "Copy verification codes when using a notification action", "default": true},
                        ],
                        "values": {"showPopups": trusted.notification_popups, "copyAuthCodes": trusted.notification_copy_codes},
                    }));
                    if enabled {
                        plugin_state[crate::notifications::PLUGIN] = link
                            .notifications
                            .snapshot(|app| link.media.matches_app(app));
                    }
                }
                for (plugin, name, description) in crate::messaging::FEATURES {
                    if peer.supports_messaging(plugin) {
                        supported.push(plugin);
                        let enabled = trusted.enabled(plugin);
                        plugins.push(json!({
                            "id": plugin, "name": name, "description": description,
                            "enabled": enabled, "loaded": enabled, "settings": [], "values": {},
                        }));
                        if enabled {
                            if let Some(status) = link.and_then(|link| link.status.get(plugin)) {
                                plugin_state[plugin] = status.clone();
                            }
                        }
                    }
                }
                if peer.accepts(PING) {
                    let enabled = trusted.enabled(phone_audio::PLUGIN);
                    supported.push(phone_audio::PLUGIN);
                    plugins.push(json!({
                        "id": phone_audio::PLUGIN, "name": "Audio output",
                        "description": "Choose whether the phone's sound plays on the phone or on this computer",
                        "enabled": enabled, "loaded": enabled, "settings": [], "values": {},
                    }));
                    if enabled {
                        let phone = phone_audio::find(
                            &audio_devices,
                            &trusted.phone_audio_address,
                            &peer.name,
                        );
                        plugin_state[phone_audio::PLUGIN] = match phone {
                            Some(phone) => {
                                json!({"available": true, "onComputer": phone.on_computer, "playing": phone.playing})
                            }
                            None => json!({"available": false}),
                        };
                    }
                }
                if peer.controls_local_media() {
                    let enabled = trusted.enabled(local::PLUGIN);
                    supported.push(local::PLUGIN);
                    plugins.push(json!({
                        "id": local::PLUGIN, "name": "Multimedia control receiver",
                        "description": "Control this computer's media players from the device",
                        "enabled": enabled, "loaded": enabled, "settings": [], "values": {},
                    }));
                }
                if let Some(link) = link.filter(|link| link.peer.supports_media()) {
                    let enabled = trusted.enabled(media::PLUGIN);
                    supported.push(media::PLUGIN);
                    plugins.push(json!({
                        "id": media::PLUGIN, "name": "Multimedia controls", "description": "Control the device's media players",
                        "enabled": enabled, "loaded": enabled,
                        "settings": [
                            {"key": "exportMpris", "type": "bool", "label": "Show device players in the shell media controls", "default": true},
                            {"key": "hideIdle", "type": "bool", "label": "Hide players paused for more than 10 minutes", "default": true},
                        ],
                        "values": {"hideIdle": trusted.media_hide_idle, "exportMpris": trusted.media_export},
                    }));
                    if enabled {
                        plugin_state[media::PLUGIN] = link.media.snapshot(trusted.media_hide_idle);
                    }
                }
                for (id, title, description, available) in [
                    (
                        "battery",
                        "Battery monitor",
                        "Show battery levels",
                        peer.sends(BATTERY) || peer.accepts(BATTERY),
                    ),
                    (
                        "connectivity_report",
                        "Connectivity monitor",
                        "Show mobile network signal",
                        peer.sends(CONNECTIVITY),
                    ),
                    (
                        "findmyphone",
                        "Ring my phone",
                        "Make the device ring so you can find it",
                        peer.accepts(RING),
                    ),
                    (
                        telephony::PLUGIN,
                        "Telephony integration",
                        "Show notifications for calls",
                        peer.sends(telephony::EVENT),
                    ),
                    (
                        crate::idle::PLUGIN,
                        "Prevent screensaver",
                        "Keep the screen awake while the device is connected",
                        peer.accepts(PING),
                    ),
                ] {
                    if !available {
                        continue;
                    }
                    supported.push(id);
                    plugins.push(json!({
                        "id": id, "name": title, "description": description, "enabled": trusted.enabled(id),
                        "loaded": trusted.enabled(id), "settings": [], "values": {},
                    }));
                    if let Some(status) = link.and_then(|link| link.status.get(id)) {
                        plugin_state[id] = status.clone();
                    }
                }
                if self.config.clipboard.is_some() && (peer.accepts(TEXT) || peer.sends(TEXT)) {
                    supported.push("clipboard");
                    plugins.push(json!({
                        "id": "clipboard", "name": "Clipboard", "description": "Share text from the clipboard",
                        "enabled": trusted.enabled("clipboard"), "loaded": trusted.enabled("clipboard") && peer.accepts(TEXT),
                        "settings": [], "values": {},
                    }));
                }
                if peer.accepts(PING) || peer.sends(PING) {
                    supported.push("ping");
                    plugins.push(json!({
                        "id": "ping", "name": "Ping", "description": "Send a ping", "enabled": trusted.enabled("ping"),
                        "loaded": trusted.enabled("ping") && peer.accepts(PING), "settings": [], "values": {},
                    }));
                }
                if peer.accepts(SHARE) || peer.sends(SHARE) {
                    supported.push("share");
                    plugins.push(json!({
                        "id": "share", "name": "Share and receive", "description": "Send and receive files, links and text",
                        "enabled": trusted.enabled("share"), "loaded": trusted.enabled("share"),
                        "settings": [
                            {"key": "incomingPath", "type": "folder", "label": "Save received files to", "default": ""},
                            {"key": "openUrls", "type": "bool", "label": "Open received links automatically", "default": true},
                        ],
                        "values": {"incomingPath": trusted.share_directory, "openUrls": trusted.share_open_urls},
                        "actions": if peer.accepts(SHARE) { json!(["share_url", "share_text", "share_files", "open_folder"]) }
                            else { json!(["open_folder"]) },
                    }));
                    plugin_state["share"] = json!({"destination": share::destination(
                        &self.config.incoming_path(&trusted.share_directory), name
                    )});
                }
            }
            let paired = trusted.is_some();
            let reachable = peer.is_some();
            let device = json!({
                "id": id, "name": name, "type": device_type, "paired": paired, "reachable": reachable,
                "pairState": pairing, "pairError": link.map(|link| link.pair_error).unwrap_or_default(),
                "verificationKey": pending.map(|pending| pending.verification_key.as_str()).unwrap_or_default(),
                "address": peer.map(|p| p.address.to_string()).unwrap_or_default(),
                "bluetoothAddress": "", "sameHost": false,
                "protocolVersion": peer.map(|p| p.version).or_else(|| trusted.map(|t| t.version)).unwrap_or_default(),
                "lastSeen": links.last_seen.get(id).copied().unwrap_or_default(), "fingerprint": fingerprint,
                "supportedPlugins": supported, "plugins": plugins,
                "state": plugin_state,
            });
            devices.push((!paired, !reachable, name.to_lowercase(), device));
        }
        devices.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
        let devices: Vec<_> = devices
            .into_iter()
            .map(|(_, _, _, device)| device)
            .collect();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "self": {"id": self.config.id, "name": self.config.name(), "type": "desktop", "tcpPort": self.tcp_port,
                "fingerprint": self.config.fingerprint},
            "settings": self.config.settings_state(),
            "capabilities": {"uinput": false, "uinputError": "", "sshfs": false, "clipboard": self.config.clipboard.is_some(), "audio": false},
            "devices": devices, "transfers": self.transfers.snapshot(),
        })
    }

    pub fn tray_status(&self) -> crate::tray::Presentation {
        use crate::tray::Presentation;
        let links = self.links.lock().unwrap();
        let requests: Vec<_> = links
            .active
            .values()
            .filter(|link| {
                link.pending.as_ref().is_some_and(|pending| {
                    pending.is_current() && pending.state() == "requestedByPeer"
                })
            })
            .map(|link| link.peer.name.as_str())
            .collect();
        if !requests.is_empty() {
            return Presentation::new(
                "NeedsAttention",
                "phone",
                format!("Pair request from {}", requests.join(", ")),
            );
        }
        let peers: Vec<_> = links
            .active
            .iter()
            .filter(|(id, _)| links.trust.get(id).is_some() && !links.blocked.contains(*id))
            .map(|(_, link)| {
                let name = &link.peer.name;
                let battery = link.status.get("battery");
                match battery
                    .and_then(|battery| battery.get("charge"))
                    .and_then(Value::as_i64)
                    .filter(|charge| *charge >= 0)
                {
                    Some(charge) => {
                        let charging = battery
                            .and_then(|battery| battery.get("charging"))
                            .and_then(Value::as_bool)
                            == Some(true);
                        format!("{name}: {charge}%{}", if charging { " ⚡" } else { "" })
                    }
                    None => format!("{name} (Connected)"),
                }
            })
            .collect();
        if peers.is_empty() {
            Presentation::new(
                "Active",
                "org.caelestia.Connect",
                "No devices connected".to_owned(),
            )
        } else {
            Presentation::new("Active", "phone", peers.join(" • "))
        }
    }

    pub fn receive_status(&self, id: &str, serial: u64, packet: &Packet) {
        let mut links = self.links.lock().unwrap();
        let Some(trusted) = links.trust.get(id).filter(|_| !links.blocked.contains(id)) else {
            return;
        };
        if packet.kind == TEXT && trusted.enabled("clipboard") {
            if self.config.clipboard.is_some()
                && links
                    .active
                    .get(id)
                    .is_some_and(|link| link.serial == serial)
            {
                if let Some(text) = packet
                    .body
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|text| text.len() <= clipboard::LIMIT)
                {
                    self.copy_text(text);
                }
            }
            return;
        }
        if packet.kind == PING && trusted.enabled("ping") {
            if let Some(link) = links.active.get(id).filter(|link| link.serial == serial) {
                let message = packet
                    .body
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Ping!");
                desktop::notify(
                    link.peer.name.clone(),
                    message.to_owned(),
                    "org.caelestia.Connect",
                );
            }
            return;
        }
        let Some((plugin, value)) =
            desktop::reported_status(packet).filter(|(id, _)| trusted.enabled(id))
        else {
            return;
        };
        let Some(link) = links
            .active
            .get_mut(id)
            .filter(|link| link.serial == serial)
        else {
            return;
        };
        if link.status.get(plugin) != Some(&value) {
            link.status.insert(plugin, value);
            drop(links);
            self.changed();
        }
    }

    pub fn receive_media(self: &Arc<Self>, id: &str, serial: u64, packet: Packet) {
        if packet
            .body
            .get("transferringAlbumArt")
            .and_then(Value::as_bool)
            == Some(true)
        {
            self.receive_art(id, serial, packet);
            return;
        }
        let mut links = self.links.lock().unwrap();
        let Some((link, _)) = links
            .media_link(id)
            .filter(|(link, _)| link.serial == serial)
        else {
            return;
        };
        for request in link.media.receive(&packet.body) {
            link.queue(request);
        }
        drop(links);
        self.changed();
    }

    fn receive_art(self: &Arc<Self>, id: &str, serial: u64, packet: Packet) {
        let (Some(url), Some(size), Some(port)) = (
            packet
                .body
                .get("albumArtUrl")
                .and_then(Value::as_str)
                .filter(|url| url.len() <= 2048),
            packet
                .payload_size
                .and_then(|size| u64::try_from(size).ok())
                .filter(|size| (12..=media::MAX_ART_SIZE).contains(size)),
            packet
                .payload_transfer_info
                .map(|transfer| transfer.port)
                .filter(|port| *port != 0),
        ) else {
            return;
        };
        let Ok(permit) = ART_DOWNLOADS.try_acquire() else {
            return;
        };
        let (address, certificate, token) = {
            let mut links = self.links.lock().unwrap();
            let Some((link, trusted)) = links
                .media_link(id)
                .filter(|(link, _)| link.serial == serial)
            else {
                return;
            };
            let Some(token) = link.media.start_art(url) else {
                return;
            };
            (
                SocketAddrV4::new(link.peer.address, port),
                trusted.certificate.clone(),
                token,
            )
        };
        let id = id.to_owned();
        let url = url.to_owned();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            let path = media::download_art(
                &state.artwork_directory,
                &state.config.tls,
                address,
                &id,
                certificate,
                size,
            )
            .await
            .ok();
            let mut links = state.links.lock().unwrap();
            if let Some((link, _)) = links
                .media_link(&id)
                .filter(|(link, _)| link.serial == serial)
            {
                if link.media.finish_art(&url, &token, path) {
                    drop(links);
                    state.changed();
                }
            }
        });
    }

    pub fn media_action(
        &self,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<bool> {
        let mut links = self.links.lock().unwrap();
        let (link, trusted) = links
            .media_link(id)
            .context("media controls unavailable for this device")?;
        let Some(packet) = link.media.action(action, args, trusted.media_hide_idle)? else {
            return Ok(false);
        };
        let player = packet
            .body
            .get("player")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        ensure!(link.queue(packet), "device is busy or disconnected");
        if action == "select_player" {
            link.media.selected(&player);
            drop(links);
            self.changed();
        }
        Ok(true)
    }

    pub async fn expire_idle_media(&self) {
        let mut timer = tokio::time::interval(Duration::from_secs(15));
        loop {
            timer.tick().await;
            let mut links = self.links.lock().unwrap();
            let mut expired = false;
            for link in links.active.values_mut() {
                expired |= link.media.expire_idle();
            }
            if expired {
                drop(links);
                self.changed();
            }
        }
    }

    pub fn exported_players(&self, limit: usize) -> Vec<media::ExportedPlayer> {
        let links = self.links.lock().unwrap();
        links
            .active
            .iter()
            .filter_map(|(id, link)| {
                let trusted = links.trust.get(id).filter(|trusted| {
                    !links.blocked.contains(id)
                        && trusted.enabled(media::PLUGIN)
                        && trusted.media_export
                })?;
                Some(
                    link.media
                        .exports(id, &link.peer.name, link.serial, trusted.media_hide_idle),
                )
            })
            .flatten()
            .take(limit)
            .collect()
    }

    pub fn send_shell_media(
        &self,
        id: &str,
        serial: u64,
        player: &str,
        mut body: Value,
    ) -> Result<()> {
        let mut links = self.links.lock().unwrap();
        let (link, _) = links
            .media_link(id)
            .filter(|(link, trusted)| {
                link.serial == serial
                    && trusted.media_export
                    && link.media.accepts_command(player, trusted.media_hide_idle)
            })
            .context("media player is no longer available")?;
        body["player"] = json!(player);
        ensure!(
            link.queue(Packet::new(media::REQUEST, body)),
            "device is busy or disconnected"
        );
        Ok(())
    }

    pub fn receive_local_media(&self, id: &str, serial: u64, body: Value) {
        if self.local_media_allowed(id, serial) {
            let _ = self.local_media.try_send(local::Request {
                id: id.to_owned(),
                serial,
                body,
            });
        }
    }

    pub fn local_media_allowed(&self, id: &str, serial: u64) -> bool {
        self.links
            .lock()
            .unwrap()
            .local_media_link(id, Some(serial))
            .is_some()
    }

    pub fn local_media_targets(&self) -> BTreeSet<(String, u64)> {
        let links = self.links.lock().unwrap();
        links
            .active
            .keys()
            .filter_map(|id| {
                links
                    .local_media_link(id, None)
                    .map(|(link, _)| (id.clone(), link.serial))
            })
            .collect()
    }

    pub fn send_local_media(&self, id: &str, serial: u64, body: Value) -> bool {
        self.links
            .lock()
            .unwrap()
            .local_media_link(id, Some(serial))
            .is_some_and(|(link, _)| link.queue(Packet::new(media::INFO, body)))
    }

    pub fn local_media_payload_peer(&self, id: &str, serial: u64) -> Option<local::PayloadPeer> {
        let links = self.links.lock().unwrap();
        let (link, trusted) = links.local_media_link(id, Some(serial))?;
        Some(local::PayloadPeer {
            address: link.peer.address,
            local_address: link.peer.local_address,
            certificate: trusted.certificate.clone(),
        })
    }

    pub fn phone_audio_target(&self, id: &str, serial: u64) -> Option<phone_audio::Phone> {
        let links = self.links.lock().unwrap();
        let trusted = links.trust.get(id).filter(|trusted| {
            trusted.enabled(phone_audio::PLUGIN) && !links.blocked.contains(id)
        })?;
        let link = links
            .active
            .get(id)
            .filter(|link| link.serial == serial && link.peer.accepts(PING))?;
        phone_audio::find(
            &self.audio_devices.lock().unwrap(),
            &trusted.phone_audio_address,
            &link.peer.name,
        )
        .cloned()
    }

    pub fn set_phone_audio_devices(&self, devices: Vec<phone_audio::Phone>) {
        let mut current = self.audio_devices.lock().unwrap();
        if *current != devices {
            *current = devices;
            drop(current);
            self.changed();
        }
    }

    pub async fn route_phone_audio(&self, id: &str, on_computer: Option<bool>) -> Result<Value> {
        let (serial, playing) = {
            let links = self.links.lock().unwrap();
            let link = links.active.get(id).context("device is disconnected")?;
            (link.serial, link.media.playing_player().map(str::to_owned))
        };
        let Some(phone) = self.phone_audio_target(id, serial) else {
            return Ok(json!({"error": "Phone not found"}));
        };
        let (completion, result) = oneshot::channel();
        self.phone_audio
            .try_send(phone_audio::Request {
                id: id.to_owned(),
                serial,
                path: phone.path,
                on_computer,
                completion,
            })
            .context("Bluetooth audio is busy or unavailable")?;
        match tokio::time::timeout(Duration::from_secs(25), result).await {
            Ok(Ok(Ok(false))) => {
                if let Some(playing) = playing {
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    let mut links = self.links.lock().unwrap();
                    if let Some((link, _)) = links
                        .media_link(id)
                        .filter(|(link, _)| link.serial == serial)
                    {
                        if link.media.is_paused(&playing) {
                            link.queue(Packet::new(
                                media::REQUEST,
                                json!({"player": playing, "action": "Play"}),
                            ));
                        }
                    }
                }
            }
            Ok(Ok(Ok(true))) => (),
            _ => {
                return Ok(
                    json!({"error": "Could not change the audio output. Check that Bluetooth and Media audio are enabled."}),
                );
            }
        }
        Ok(json!({}))
    }

    pub async fn report_battery(&self) {
        let mut readings = tokio::time::interval(Duration::from_secs(30));
        let mut last = BTreeMap::new();
        loop {
            readings.tick().await;
            let Some(battery) = desktop::local_battery() else {
                continue;
            };
            let recipients: Vec<_> = {
                let links = self.links.lock().unwrap();
                links
                    .active
                    .iter()
                    .filter(|(id, link)| {
                        links
                            .trust
                            .get(id)
                            .is_some_and(|entry| entry.enabled("battery"))
                            && !links.blocked.contains(*id)
                            && link.peer.accepts(BATTERY)
                    })
                    .map(|(id, link)| (id.clone(), link.serial))
                    .collect()
            };
            last.retain(|id, _| recipients.iter().any(|(peer, _)| peer == id));
            for (id, serial) in recipients {
                if last.get(&id) == Some(&(serial, battery.clone())) {
                    continue;
                }
                if self
                    .send_on(&id, Some(serial), Packet::new(BATTERY, battery.clone()))
                    .await
                    .is_ok()
                {
                    last.insert(id, (serial, battery.clone()));
                }
            }
        }
    }

    pub fn copy_text(&self, text: &str) {
        if self.config.clipboard.is_some() {
            self.clipboard_updates.send_replace(Some(text.to_owned()));
        }
    }

    pub async fn run_clipboard(&self) {
        let Some(clipboard) = self.config.clipboard else {
            return std::future::pending().await;
        };
        let mut updates = self.clipboard_updates.subscribe();
        while updates.changed().await.is_ok() {
            let text = updates.borrow_and_update().clone();
            if let Some(text) = text {
                let _ = clipboard.write(&text).await;
            }
        }
    }

    pub async fn send_clipboard(&self, id: &str) -> Result<bool> {
        let clipboard = self.config.clipboard.context("clipboard is unavailable")?;
        let serial = {
            let links = self.links.lock().unwrap();
            let trusted = links
                .trust
                .get(id)
                .filter(|_| !links.blocked.contains(id))
                .context("device is not paired")?;
            let link = links.active.get(id).context("device is disconnected")?;
            ensure!(
                trusted.enabled("clipboard") && link.peer.accepts(TEXT),
                "clipboard sharing unavailable"
            );
            link.serial
        };
        let text = clipboard.read().await?;
        if text.is_empty() {
            return Ok(false);
        }
        self.send_on(
            id,
            Some(serial),
            Packet::new(TEXT, json!({"content": text})),
        )
        .await?;
        Ok(true)
    }

    pub async fn ping(&self, id: &str, message: &str) -> Result<()> {
        ensure!(message.len() <= 4096, "ping message is too long");
        let body = if message.is_empty() {
            json!({})
        } else {
            json!({ "message": message })
        };
        self.send(id, Packet::new(PING, body)).await
    }

    pub async fn share(&self, id: &str, url: bool, content: &str) -> Result<()> {
        ensure!(
            !content.trim().is_empty() && content.len() <= 8192,
            "share content must be between 1 and 8192 bytes"
        );
        if url {
            ensure!(
                !content.chars().any(|c| c.is_whitespace() || c.is_control())
                    && ["http://", "https://", "mailto:", "tel:", "geo:"]
                        .iter()
                        .any(|prefix| content
                            .strip_prefix(prefix)
                            .is_some_and(|rest| !rest.is_empty())),
                "unsupported link",
            );
        }
        let body = if url {
            json!({"url": content})
        } else {
            json!({"text": content})
        };
        self.send(id, Packet::new(SHARE, body)).await
    }

    pub async fn send(&self, id: &str, packet: Packet) -> Result<()> {
        self.send_on(id, None, packet).await
    }

    pub async fn send_on(&self, id: &str, serial: Option<u64>, packet: Packet) -> Result<()> {
        let outbound = {
            let links = self.links.lock().unwrap();
            let trusted = links
                .trust
                .get(id)
                .filter(|_| !links.blocked.contains(id))
                .context("device is not paired")?;
            let link = links
                .active
                .get(id)
                .filter(|link| serial.is_none_or(|serial| serial == link.serial))
                .context("device is disconnected")?;
            match packet.kind.as_str() {
                PING => ensure!(
                    trusted.enabled("ping") && link.peer.accepts(PING),
                    "Ping unavailable for this device"
                ),
                SHARE => ensure!(
                    trusted.enabled("share") && link.peer.accepts(SHARE),
                    "sharing unavailable for this device"
                ),
                BATTERY => ensure!(
                    trusted.enabled("battery") && link.peer.accepts(BATTERY),
                    "battery sharing unavailable"
                ),
                TEXT => ensure!(
                    trusted.enabled("clipboard") && link.peer.accepts(TEXT),
                    "clipboard sharing unavailable"
                ),
                RING => ensure!(
                    trusted.enabled("findmyphone") && link.peer.accepts(RING),
                    "ringing unavailable"
                ),
                media::INFO => ensure!(
                    trusted.enabled(local::PLUGIN) && link.peer.controls_local_media(),
                    "desktop media controls unavailable"
                ),
                _ => anyhow::bail!("unsupported plugin packet"),
            }
            link.outbound.clone()
        };
        let (completed, result) = oneshot::channel();
        outbound
            .try_send(Transmission {
                packet,
                completed: Some(completed),
            })
            .context("device is busy or disconnected")?;
        tokio::time::timeout(Duration::from_secs(5), result)
            .await
            .context("sending to device timed out")?
            .context("device disconnected before sending")?
    }
}
