use crate::{
    config::xdg_path,
    lan::authenticated_stream,
    state::State,
    wire::{self, Packet},
};
use anyhow::{Context, Result, ensure};
use dbus::{
    arg::{PropMap, Variant},
    blocking::LocalConnection,
    message::MatchRule,
};
use image::{ImageFormat, ImageReader, Limits};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    env,
    io::Cursor,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::{Semaphore, mpsc, watch},
    time::timeout,
};

pub const PLUGIN: &str = "notifications";
pub const INFO: &str = "kdeconnect.notification";
pub const REQUEST: &str = "kdeconnect.notification.request";
pub const REPLY: &str = "kdeconnect.notification.reply";
pub const ACTION: &str = "kdeconnect.notification.action";
pub const OUTGOING: &[&str] = &[REQUEST, REPLY, ACTION];
const DESKTOP: &str = "org.freedesktop.Notifications";
const DESKTOP_PATH: &str = "/org/freedesktop/Notifications";
const SYNC_INTERVAL: Duration = Duration::from_secs(120);
const SYNC_GRACE: Duration = Duration::from_secs(6);
const MAX_ITEMS: usize = 128;
const MAX_ICON: u64 = 512 * 1024;
const PNG_HEADER: usize = 24;
const MAX_ICON_EDGE: u32 = 1024;
static ICON_TASKS: Semaphore = Semaphore::const_new(4);
pub const MAX_CALL_THUMBNAIL: usize = 256 * 1024;
const MAX_CALL_IMAGE_EDGE: u32 = 1024;
const CALL_PREVIEW_EDGE: u32 = 128;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub device: String,
    pub serial: u64,
    pub id: String,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PopupKey {
    Phone(Key),
    Call { device: String, serial: u64 },
    MissedCall { device: String, serial: u64 },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub app: String,
    title: String,
    text: String,
    ticker: String,
    pub dismissable: bool,
    pub silent: bool,
    pub reply_id: String,
    pub actions: Vec<String>,
    pub payload_hash: String,
    pub icon: Option<PathBuf>,
    time: u64,
}

fn string(value: &Value, key: &str, max: usize) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .filter(|ch| *ch != '\0')
        .take(max)
        .collect()
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)
}

impl Item {
    fn from_packet(body: &Value) -> Option<Self> {
        let id = body
            .get("id")?
            .as_str()
            .filter(|id| valid_id(id))?
            .to_owned();
        let app = string(body, "appName", 128);
        let mut title = string(body, "title", 256);
        let mut text = string(body, "text", 1400);
        let ticker = string(body, "ticker", 1400);
        let actions = body
            .get("actions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|action| !action.is_empty() && action.len() <= 512 && !action.contains('\0'))
            .take(16)
            .map(str::to_owned)
            .collect();
        let conversation: Vec<_> = body
            .get("conversation")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|msg| msg.is_object())
            .take(16)
            .map(|msg| (string(msg, "sender", 128), string(msg, "content", 400)))
            .collect();
        if let Some((sender, _)) = conversation.first() {
            let group = string(body, "groupName", 256);
            if !group.is_empty() {
                title = group;
            } else if !sender.is_empty() {
                title.clone_from(sender);
            }
            text = conversation
                .into_iter()
                .map(|(sender, content)| {
                    if sender.is_empty() {
                        content
                    } else {
                        format!("{sender}: {content}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(1800)
                .collect();
        }
        if title.is_empty() && text.is_empty() {
            text.clone_from(&ticker);
        }
        let reply_id = body
            .get("requestReplyId")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .unwrap_or_default()
            .to_owned();
        let payload_hash = body
            .get("payloadHash")
            .and_then(Value::as_str)
            .filter(|hash| {
                !hash.is_empty()
                    && hash.len() <= 128
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
            .unwrap_or_default()
            .to_owned();
        Some(Self {
            id,
            app,
            title,
            text,
            ticker,
            dismissable: flag(body, "isClearable"),
            silent: flag(body, "silent"),
            reply_id,
            actions,
            payload_hash,
            icon: None,
            time: wire::timestamp_ms(),
        })
    }

    fn snapshot(&self) -> Value {
        json!({"id": self.id, "appName": self.app, "title": self.title, "text": self.text,
               "ticker": self.ticker, "icon": self.icon.as_ref().map_or("", |path| path.to_str().unwrap_or("")),
               "dismissable": self.dismissable, "replyId": self.reply_id, "actions": self.actions,
               "time": self.time / 1000, "silent": self.silent})
    }
}

#[derive(Default)]
pub struct Inbox {
    pub items: BTreeMap<String, Item>,
    last_sync: Option<Instant>,
    seen: Option<BTreeSet<String>>,
}

impl Inbox {
    pub fn begin_sync(&mut self) {
        self.last_sync = Some(Instant::now());
        self.seen = Some(BTreeSet::new());
    }

    pub fn needs_sync(&self) -> bool {
        self.last_sync
            .is_none_or(|started| started.elapsed() >= SYNC_INTERVAL)
    }

    pub fn expire(&mut self) -> bool {
        if self
            .last_sync
            .is_none_or(|started| started.elapsed() < SYNC_GRACE)
        {
            return false;
        }
        let Some(seen) = self.seen.take() else {
            return false;
        };
        let old = self.items.len();
        self.items.retain(|id, _| seen.contains(id));
        self.items.len() != old
    }

    pub fn receive(&mut self, body: &Value) -> (bool, Option<Item>) {
        if flag(body, "request") {
            return (false, None);
        }
        if flag(body, "isCancel") {
            let id = body.get("id").and_then(Value::as_str).unwrap_or_default();
            let raw = id.strip_prefix("org.kde.kdeconnect_tp::").unwrap_or(id);
            return (
                self.items
                    .remove(raw)
                    .or_else(|| self.items.remove(id))
                    .is_some(),
                None,
            );
        }
        let Some(mut item) = Item::from_packet(body) else {
            return (false, None);
        };
        if let Some(previous) = self.items.get(&item.id) {
            item.time = previous.time;
            if previous.payload_hash == item.payload_hash {
                item.icon = previous.icon.clone();
            }
        } else if self.items.len() >= MAX_ITEMS {
            if let Some(oldest) = self
                .items
                .values()
                .min_by_key(|item| item.time)
                .map(|item| item.id.clone())
            {
                self.items.remove(&oldest);
            }
        }
        if let Some(seen) = &mut self.seen {
            // At most the tracked inbox is needed once the resync ends.
            if seen.len() >= MAX_ITEMS * 2 {
                seen.retain(|id| self.items.contains_key(id));
            }
            seen.insert(item.id.clone());
        }
        let changed = self.items.get(&item.id) != Some(&item);
        self.items.insert(item.id.clone(), item.clone());
        (changed, Some(item))
    }

    pub fn snapshot(&self, is_media: impl Fn(&str) -> bool) -> Value {
        let mut items: Vec<_> = self
            .items
            .values()
            .filter(|item| item.dismissable || !is_media(&item.app))
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.time));
        json!({"items": items.into_iter().map(Item::snapshot).collect::<Vec<_>>()})
    }
}

pub enum Popup {
    Phone {
        key: Key,
        item: Box<Item>,
        source: String,
    },
    Call {
        key: PopupKey,
        title: String,
        contact: String,
        thumbnail: Option<String>,
    },
}

impl Popup {
    fn key(&self) -> PopupKey {
        match self {
            Self::Phone { key, .. } => PopupKey::Phone(key.clone()),
            Self::Call { key, .. } => key.clone(),
        }
    }
}

pub fn download_icon(state: &Arc<State>, key: Key, hash: String, packet: &Packet) {
    let Some(port) = packet
        .payload_transfer_info
        .as_ref()
        .map(|info| info.port)
        .filter(|port| *port != 0)
    else {
        return;
    };
    let Some(size) = packet
        .payload_size
        .and_then(|size| u64::try_from(size).ok())
        .filter(|size| (PNG_HEADER as u64..=MAX_ICON).contains(size))
    else {
        return;
    };
    let Some((address, certificate)) = state.notification_payload_peer(&key) else {
        return;
    };
    let Ok(permit) = ICON_TASKS.try_acquire() else {
        return;
    };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let _permit = permit;
        if let Ok(Ok(path)) = timeout(Duration::from_secs(15), async {
            let path = icon_path(&key.device, &hash)?;
            if tokio::fs::symlink_metadata(&path)
                .await
                .is_ok_and(|meta| meta.is_file())
            {
                return Ok(path);
            }
            let socket = TcpStream::connect((address, port)).await?;
            let mut tls = authenticated_stream(
                &state.config.tls,
                socket,
                &key.device,
                Some(certificate),
                false,
            )
            .await?;
            let mut header = [0; PNG_HEADER];
            tls.read_exact(&mut header).await?;
            ensure!(
                &header[..16] == b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR",
                "unsupported notification icon type"
            );
            for dimension in [&header[16..20], &header[20..24]] {
                let edge = u32::from_be_bytes(dimension.try_into()?);
                ensure!(
                    (1..=MAX_ICON_EDGE).contains(&edge),
                    "notification icon is too large"
                );
            }
            let directory = path.parent().context("missing icon directory")?;
            tokio::fs::create_dir_all(directory).await?;
            let file = tempfile::NamedTempFile::new_in(directory)?;
            let mut output = tokio::fs::File::from_std(file.as_file().try_clone()?);
            output.write_all(&header).await?;
            let remaining = size - PNG_HEADER as u64;
            ensure!(
                tokio::io::copy(&mut (&mut tls).take(remaining), &mut output).await? == remaining,
                "incomplete icon"
            );
            output.flush().await?;
            ensure!(
                state.notification_payload_peer(&key).is_some(),
                "device is disconnected"
            );
            file.persist(&path).map_err(|error| error.error)?;
            Ok::<_, anyhow::Error>(path)
        })
        .await
        {
            state.set_notification_icon(&key, &hash, path);
        }
    });
}

fn icon_path(device: &str, hash: &str) -> Result<PathBuf> {
    let root = if let Some(root) = env::var_os("CAELESTIA_CONNECT_PREVIEW_DATA") {
        PathBuf::from(root).join("cache")
    } else {
        xdg_path("XDG_CACHE_HOME", ".cache")?.join("caelestia-connect")
    };
    Ok(root
        .join("notification-icons")
        .join(device)
        .join(format!("{hash}.png")))
}

pub fn verification_code(action: &str) -> Option<String> {
    let action = action.replace('\u{2063}', "");
    action
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .find(|part| {
            (4..=10).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())
        })
        .map(str::to_owned)
}

enum DesktopAction {
    Click(u32, String),
    Reply(u32, String),
    Closed(u32),
    ServerChanged,
}

fn close(bus: &LocalConnection, id: u32) {
    let server = bus.with_proxy(DESKTOP, DESKTOP_PATH, Duration::from_millis(750));
    let _: Result<(), _> = server.method_call(DESKTOP, "CloseNotification", (id,));
}

fn show(bus: &LocalConnection, popup: &Popup, replaces: u32) -> Result<u32> {
    let Popup::Phone { item, source, .. } = popup else {
        return show_call(bus, popup, replaces);
    };
    let title = if item.title.is_empty() {
        &item.app
    } else {
        &item.title
    };
    let title = if title.is_empty() {
        "Phone notification"
    } else {
        title
    };
    let body = escape(&item.text);
    let icon = item
        .icon
        .as_deref()
        .and_then(Path::to_str)
        .unwrap_or("phone");
    let mut actions = vec!["default".to_owned(), "Open".to_owned()];
    for action in &item.actions {
        actions.extend([format!("action:{action}"), action.clone()]);
    }
    let mut hints = PropMap::new();
    hints.insert(
        "desktop-entry".into(),
        Variant(Box::new("org.caelestia.Connect".to_owned())),
    );
    hints.insert(
        "x-kdeconnect-source-device".into(),
        Variant(Box::new(source.clone())),
    );
    if item.icon.is_some() {
        hints.insert("image-path".into(), Variant(Box::new(icon.to_owned())));
    }
    if !item.reply_id.is_empty() {
        actions.extend(["reply".into(), "Reply".into()]);
        hints.insert(
            "x-kde-reply-placeholder-text".into(),
            Variant(Box::new(format!("Reply to {title}"))),
        );
    }
    if item.dismissable {
        actions.extend(["dismiss".into(), "Dismiss".into()]);
    }
    let server = bus.with_proxy(DESKTOP, DESKTOP_PATH, Duration::from_millis(750));
    let (id,): (u32,) = server.method_call(
        DESKTOP,
        "Notify",
        (
            if item.app.is_empty() {
                source
            } else {
                &item.app
            },
            replaces,
            icon,
            title,
            body,
            actions,
            hints,
            -1_i32,
        ),
    )?;
    Ok(id)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

type ImageHint = (i32, i32, i32, bool, i32, i32, Vec<u8>);

fn call_image(encoded: &str) -> Option<ImageHint> {
    let compact: String = encoded
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let bytes = openssl::base64::decode_block(&compact).ok()?;
    let format = image::guess_format(&bytes).ok()?;
    if !matches!(format, ImageFormat::Jpeg | ImageFormat::Png) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_CALL_IMAGE_EDGE);
    limits.max_image_height = limits.max_image_width;
    limits.max_alloc = Some(16 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let pixels = if image.width().max(image.height()) > CALL_PREVIEW_EDGE {
        image
            .thumbnail(CALL_PREVIEW_EDGE, CALL_PREVIEW_EDGE)
            .into_rgba8()
    } else {
        image.into_rgba8()
    };
    let width = pixels.width() as i32;
    let height = pixels.height() as i32;
    Some((width, height, width * 4, true, 8, 4, pixels.into_raw()))
}

fn show_call(bus: &LocalConnection, popup: &Popup, replaces: u32) -> Result<u32> {
    let Popup::Call {
        key,
        title,
        contact,
        thumbnail,
    } = popup
    else {
        unreachable!();
    };
    let ringing = matches!(key, PopupKey::Call { .. });
    let (event, category, urgency, actions) = if ringing {
        (
            "Incoming call from",
            "x-kde.call",
            2_u8,
            vec!["mute", "Mute call"],
        )
    } else {
        ("Missed call from", "x-kde.call.missed", 1_u8, Vec::new())
    };
    let mut hints = PropMap::new();
    hints.insert(
        "desktop-entry".into(),
        Variant(Box::new("org.caelestia.Connect".to_owned())),
    );
    hints.insert(
        "x-kdeconnect-source-device".into(),
        Variant(Box::new(title.clone())),
    );
    hints.insert("urgency".into(), Variant(Box::new(urgency)));
    hints.insert("category".into(), Variant(Box::new(category.to_owned())));
    hints.insert("resident".into(), Variant(Box::new(ringing)));
    if let Some(image) = thumbnail.as_deref().and_then(call_image) {
        hints.insert("image-data".into(), Variant(Box::new(image)));
    }
    let server = bus.with_proxy(DESKTOP, DESKTOP_PATH, Duration::from_millis(750));
    let (id,) = server.method_call(
        DESKTOP,
        "Notify",
        (
            "Caelestia Connect",
            replaces,
            "call-start",
            title,
            format!("{event} {}", escape(contact)),
            actions,
            hints,
            if ringing { 0_i32 } else { -1_i32 },
        ),
    )?;
    Ok(id)
}

fn serve(
    state: Weak<State>,
    runtime: Handle,
    mut popups: mpsc::Receiver<Popup>,
    alive: watch::Receiver<()>,
) -> Result<()> {
    let bus = LocalConnection::new_session()?;
    let events = Rc::new(RefCell::new(Vec::new()));
    for signal in ["ActionInvoked", "NotificationReplied"] {
        let actions = events.clone();
        let mut rule = MatchRule::new_signal(DESKTOP, signal);
        rule.sender = Some(DESKTOP.into());
        rule.path = Some(DESKTOP_PATH.into());
        bus.add_match(rule, move |(id, action): (u32, String), _, _| {
            if actions.borrow().len() < 64 {
                actions.borrow_mut().push(if signal == "ActionInvoked" {
                    DesktopAction::Click(id, action)
                } else {
                    DesktopAction::Reply(id, action)
                });
            }
            true
        })?;
    }
    let actions = events.clone();
    let mut rule = MatchRule::new_signal(DESKTOP, "NotificationClosed");
    rule.sender = Some(DESKTOP.into());
    rule.path = Some(DESKTOP_PATH.into());
    bus.add_match(rule, move |(id, _): (u32, u32), _, _| {
        if actions.borrow().len() < 64 {
            actions.borrow_mut().push(DesktopAction::Closed(id));
        }
        true
    })?;
    let actions = events.clone();
    let mut rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    rule.sender = Some("org.freedesktop.DBus".into());
    rule.path = Some("/org/freedesktop/DBus".into());
    bus.add_match(rule, move |(name, _, _): (String, String, String), _, _| {
        if name == DESKTOP {
            actions.borrow_mut().clear();
            actions.borrow_mut().push(DesktopAction::ServerChanged);
        }
        true
    })?;
    let mut shown = BTreeMap::<PopupKey, u32>::new();
    while alive.has_changed().is_ok() {
        let Some(state) = state.upgrade() else {
            break;
        };
        for event in events.take() {
            match event {
                DesktopAction::ServerChanged => shown.clear(),
                DesktopAction::Closed(id) => {
                    shown.retain(|_, shown| *shown != id);
                }
                DesktopAction::Click(id, name) => {
                    if let Some((key, _)) = shown.iter().find(|(_, shown)| **shown == id) {
                        match key {
                            PopupKey::Phone(key) => {
                                state.desktop_notification_action(&runtime, key, &name)
                            }
                            PopupKey::Call { device, serial } if name == "mute" => {
                                let _ = state.mute_call(device, Some(*serial));
                            }
                            _ => (),
                        }
                    }
                }
                DesktopAction::Reply(id, text) => {
                    if let Some((PopupKey::Phone(key), _)) =
                        shown.iter().find(|(_, shown)| **shown == id)
                    {
                        state.desktop_notification_reply(key, &text);
                    }
                }
            }
        }
        for _ in 0..8 {
            let Ok(popup) = popups.try_recv() else {
                break;
            };
            let key = popup.key();
            if !state.desktop_popup_exists(&key) {
                continue;
            }
            let replacing = shown.get(&key).copied().unwrap_or_default();
            if let Ok(id) = show(&bus, &popup, replacing) {
                if !matches!(key, PopupKey::MissedCall { .. }) {
                    shown.insert(key, id);
                }
            }
        }
        shown.retain(|key, id| {
            if state.desktop_popup_exists(key) {
                true
            } else {
                close(&bus, *id);
                false
            }
        });
        drop(state);
        bus.process(Duration::from_millis(125))?;
    }
    for id in shown.values() {
        close(&bus, *id);
    }
    Ok(())
}

pub async fn run(state: &Arc<State>, popups: mpsc::Receiver<Popup>) {
    let (lifetime, alive) = watch::channel(());
    let state = Arc::downgrade(state);
    let runtime = Handle::current();
    let _ = tokio::task::spawn_blocking(move || serve(state, runtime, popups, alive)).await;
    drop(lifetime);
    std::future::pending::<()>().await;
}
