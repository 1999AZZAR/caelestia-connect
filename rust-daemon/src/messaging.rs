mod contacts;
mod sms;

use crate::{config::xdg_path, lan::authenticated_stream, state::State, wire::Packet};
use anyhow::{Context, Result, bail, ensure};
use contacts::Contacts;
use serde_json::{Map, Value, json};
use sms::{ATTACHMENT_LIMIT, Archive, attachment_name, recipient_objects};
use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{Arc, Weak, mpsc},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::{Semaphore, oneshot},
    time::timeout,
};

pub const SMS: &str = "sms";
pub const CONTACTS: &str = "contacts";
pub const SMS_MESSAGES: &str = "kdeconnect.sms.messages";
const SMS_ATTACHMENT: &str = "kdeconnect.sms.attachment_file";
const SMS_SEND: &str = "kdeconnect.sms.request";
pub const SMS_CONVERSATIONS: &str = "kdeconnect.sms.request_conversations";
const SMS_CONVERSATION: &str = "kdeconnect.sms.request_conversation";
const SMS_GET_ATTACHMENT: &str = "kdeconnect.sms.request_attachment";
pub const CONTACT_UIDS: &str = "kdeconnect.contacts.response_uids_timestamps";
const CONTACT_CARDS: &str = "kdeconnect.contacts.response_vcards";
pub const CONTACT_SYNC: &str = "kdeconnect.contacts.request_all_uids_timestamps";
const CONTACT_REQUEST: &str = "kdeconnect.contacts.request_vcards_by_uid";
pub const INCOMING: &[&str] = &[SMS_MESSAGES, SMS_ATTACHMENT, CONTACT_UIDS, CONTACT_CARDS];
pub const OUTGOING: &[&str] = &[
    SMS_SEND,
    SMS_CONVERSATIONS,
    SMS_CONVERSATION,
    SMS_GET_ATTACHMENT,
    CONTACT_SYNC,
    CONTACT_REQUEST,
];
pub const FEATURES: [(&str, &str, &str); 2] = [
    (SMS, "SMS", "Read and send text messages"),
    (
        CONTACTS,
        "Contacts synchronizer",
        "Synchronize the device's contacts",
    ),
];
pub const FRAME_LIMIT: usize = 8 * 1024 * 1024;
const CACHE_COUNT: usize = 3;
static ATTACHMENTS: Semaphore = Semaphore::const_new(4);

pub fn incoming_plugin(kind: &str) -> Option<&'static str> {
    match kind {
        SMS_MESSAGES | SMS_ATTACHMENT => Some(SMS),
        CONTACT_UIDS | CONTACT_CARDS => Some(CONTACTS),
        _ => None,
    }
}

pub fn outgoing_plugin(kind: &str) -> Option<&'static str> {
    match kind {
        SMS_SEND | SMS_CONVERSATIONS | SMS_CONVERSATION | SMS_GET_ATTACHMENT => Some(SMS),
        CONTACT_SYNC | CONTACT_REQUEST => Some(CONTACTS),
        _ => None,
    }
}

pub enum Request {
    Load {
        id: String,
        serial: u64,
    },
    Incoming {
        id: String,
        serial: u64,
        packet: Packet,
    },
    Action {
        id: String,
        serial: u64,
        plugin: String,
        action: String,
        args: Map<String, Value>,
        response: oneshot::Sender<Result<Value>>,
    },
    AttachmentDone {
        id: String,
        serial: u64,
    },
}

fn number(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

struct Paths {
    data: PathBuf,
    cache: PathBuf,
}

impl Paths {
    fn desktop() -> Result<Self> {
        if let Some(root) = env::var_os("CAELESTIA_CONNECT_PREVIEW_DATA") {
            let root = PathBuf::from(root);
            ensure!(
                root.is_absolute(),
                "preview data directory must be absolute"
            );
            return Ok(Self {
                data: root.join("data"),
                cache: root.join("cache"),
            });
        }
        Ok(Self {
            data: xdg_path("XDG_DATA_HOME", ".local/share")?.join("caelestia-connect"),
            cache: xdg_path("XDG_CACHE_HOME", ".cache")?.join("caelestia-connect"),
        })
    }
}

struct Cache {
    sms: Option<Archive>,
    contacts: Contacts,
    sms_path: PathBuf,
    contacts_path: PathBuf,
    attachments: PathBuf,
    requested: BTreeMap<String, (u64, Instant)>,
    version: u64,
    sms_status: Value,
    last_used: Instant,
    save_at: Option<Instant>,
}

impl Cache {
    fn load(paths: &Paths, id: &str) -> Self {
        let sms_path = paths.data.join("sms").join(format!("{id}.json"));
        let contacts_path = paths.data.join("contacts").join(id);
        let sms = Archive::load(&sms_path)
            .inspect_err(|error| eprintln!("Could not read saved SMS history: {error}"))
            .ok();
        let version = crate::wire::timestamp_ms();
        let sms_status = sms
            .as_ref()
            .map(|archive| archive.status(version))
            .unwrap_or_else(|| json!({"version": version, "threads": 0, "unread": 0}));
        Self {
            sms,
            contacts: Contacts::load(&contacts_path),
            sms_path,
            contacts_path,
            attachments: paths.cache.join("sms-attachments").join(id),
            requested: BTreeMap::new(),
            version,
            sms_status,
            last_used: Instant::now(),
            save_at: None,
        }
    }

    fn archive(&self) -> Result<&Archive> {
        self.sms
            .as_ref()
            .context("could not read saved SMS history")
    }

    fn changed(&mut self, persist: bool) {
        self.version = self.version.saturating_add(1);
        if persist {
            if let Some(archive) = &self.sms {
                self.sms_status = archive.status(self.version);
            }
            self.save_at
                .get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
        }
        self.sms_status["version"] = json!(self.version);
    }

    fn flush(&mut self) -> Result<()> {
        if self.save_at.is_some() {
            self.archive()?.save(&self.sms_path)?;
            self.save_at = None;
        }
        Ok(())
    }

    fn publish(&self, state: &State, id: &str, serial: u64) {
        state.set_messaging_status(id, serial, self.sms_status.clone(), self.contacts.status());
    }

    fn incoming(&mut self, peer: &Session<'_>, runtime: &Handle, packet: Packet) -> Result<()> {
        match packet.kind.as_str() {
            SMS_MESSAGES => {
                let archive = self
                    .sms
                    .as_mut()
                    .context("could not read saved SMS history")?;
                if archive.merge(&packet.body) {
                    self.changed(true);
                }
            }
            SMS_ATTACHMENT => {
                if let Some(name) = packet
                    .body
                    .get("filename")
                    .and_then(Value::as_str)
                    .and_then(attachment_name)
                {
                    if self.requested.get(&name).is_some_and(|(serial, sent)| {
                        *serial == peer.serial && sent.elapsed() < Duration::from_secs(60)
                    }) && receive_attachment(
                        runtime,
                        peer,
                        &packet,
                        self.attachments.join(&name),
                    ) {
                        self.requested.remove(&name);
                    }
                }
            }
            CONTACT_UIDS | CONTACT_CARDS => {
                let (changed, response) = self.contacts.receive(&self.contacts_path, &packet)?;
                if changed {
                    self.changed(false);
                }
                if let Some(response) = response {
                    peer.state.queue_messaging(peer.id, peer.serial, response)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
}

struct Session<'a> {
    state: &'a Arc<State>,
    id: &'a str,
    serial: u64,
}

impl Session<'_> {
    fn send(&self, kind: &str, body: Value) -> Result<()> {
        self.state
            .queue_messaging(self.id, self.serial, Packet::new(kind, body))
    }

    fn enabled(&self, plugin: &str) -> bool {
        self.state.messaging_enabled(self.id, self.serial, plugin)
    }
}

fn conversation_body(thread: i64, start: Option<i64>, count: Option<i64>) -> Value {
    let mut body = json!({"threadID": thread});
    if let Some(start) = start.filter(|start| *start > 0) {
        body["rangeStartTimestamp"] = json!(start);
    }
    if let Some(count) = count.filter(|count| *count > 0) {
        body["numberToRequest"] = json!(count.min(1000));
    }
    body
}

fn action(
    cache: &mut Cache,
    peer: &Session<'_>,
    plugin: &str,
    action: &str,
    args: &Map<String, Value>,
) -> Result<Value> {
    ensure!(peer.enabled(plugin), "feature unavailable for this device");
    match (plugin, action) {
        (CONTACTS, "sync") => peer.send(CONTACT_SYNC, json!({}))?,
        (CONTACTS, "list_contacts") => return Ok(cache.contacts.list()),
        (SMS, "request_conversations") => peer.send(SMS_CONVERSATIONS, json!({}))?,
        (SMS, "request_conversation") => {
            let thread = args
                .get("thread_id")
                .and_then(number)
                .filter(|thread| *thread >= 0)
                .context("invalid SMS thread")?;
            peer.send(
                SMS_CONVERSATION,
                conversation_body(
                    thread,
                    args.get("start").and_then(number),
                    args.get("count").map_or(Some(100), number),
                ),
            )?;
        }
        (SMS, "conversations") => {
            return Ok(cache
                .archive()?
                .conversations(&cache.contacts, peer.enabled(CONTACTS)));
        }
        (SMS, "messages") => {
            let thread = args
                .get("thread_id")
                .and_then(number)
                .context("invalid SMS thread")?
                .to_string();
            return Ok(cache.archive()?.messages(
                &thread,
                &cache.attachments,
                &cache.contacts,
                peer.enabled(CONTACTS),
            ));
        }
        (SMS, "get_attachment") => {
            let part = args
                .get("part_id")
                .and_then(number)
                .filter(|part| *part >= 0)
                .context("invalid attachment part")?;
            let uid = args
                .get("unique_id")
                .and_then(Value::as_str)
                .context("missing attachment identifier")?;
            let name = attachment_name(uid).context("invalid attachment identifier")?;
            let path = cache.attachments.join(&name);
            if path.is_file() {
                return Ok(json!(path));
            }
            cache.requested.retain(|_, (serial, sent)| {
                *serial == peer.serial && sent.elapsed() < Duration::from_secs(60)
            });
            if !cache.requested.contains_key(&name) {
                ensure!(
                    cache.requested.len() < 64,
                    "too many pending MMS attachments"
                );
                peer.send(
                    SMS_GET_ATTACHMENT,
                    json!({"part_id": part, "unique_identifier": uid}),
                )?;
                cache.requested.insert(name, (peer.serial, Instant::now()));
            }
            return Ok(json!(""));
        }
        (SMS, "send_sms") => return send_sms(cache, peer, args),
        _ => bail!("unknown messaging action"),
    }
    Ok(json!(true))
}

fn send_sms(cache: &mut Cache, peer: &Session<'_>, args: &Map<String, Value>) -> Result<Value> {
    let raw = args
        .get("addresses")
        .and_then(Value::as_array)
        .context("expected recipient addresses")?;
    let mut recipients = Vec::new();
    for address in raw {
        let address = address
            .as_str()
            .context("invalid recipient address")?
            .trim();
        if !address.is_empty() {
            ensure!(
                address.len() <= 256 && !address.chars().any(char::is_control),
                "invalid recipient address"
            );
            recipients.push(address.to_owned());
        }
    }
    let message = args
        .get("message")
        .map(|message| message.as_str().context("invalid message text"))
        .transpose()?
        .unwrap_or_default();
    ensure!(
        message.len() <= 64 * 1024 && recipients.len() <= 64,
        "message or recipient list is too long"
    );
    let paths = args
        .get("attachments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    ensure!(paths.len() <= 10, "too many MMS attachments");
    if recipients.is_empty() || (message.is_empty() && paths.is_empty()) {
        return Ok(json!(false));
    }
    let sub_id = args.get("sub_id").and_then(number).filter(|id| *id >= 0);
    let mut body = json!({"version": 2, "addresses": recipient_objects(&recipients)});
    if !message.is_empty() {
        body["messageBody"] = json!(message);
    }
    if let Some(sub_id) = sub_id {
        body["subID"] = json!(sub_id);
    }
    if !paths.is_empty() {
        body["attachments"] = json!(sms::load_attachments(paths)?);
    }
    peer.send(SMS_SEND, body)?;
    if let Some(thread) = cache
        .sms
        .as_mut()
        .and_then(|archive| archive.add_pending(&recipients, message, sub_id))
    {
        cache.changed(true);
        let _ = peer.send(SMS_CONVERSATION, conversation_body(thread, None, Some(10)));
    }
    Ok(json!(true))
}

fn receive_attachment(
    runtime: &Handle,
    peer: &Session<'_>,
    packet: &Packet,
    path: PathBuf,
) -> bool {
    let Some(port) = packet
        .payload_transfer_info
        .as_ref()
        .map(|info| info.port)
        .filter(|port| *port != 0)
    else {
        return false;
    };
    let Some(size) = packet
        .payload_size
        .and_then(|size| u64::try_from(size).ok())
        .filter(|size| *size <= ATTACHMENT_LIMIT)
    else {
        return false;
    };
    let Some((address, certificate)) = peer.state.messaging_payload_peer(peer.id, peer.serial)
    else {
        return false;
    };
    let Ok(permit) = ATTACHMENTS.try_acquire() else {
        return false;
    };
    let id = peer.id.to_owned();
    let serial = peer.serial;
    let state = Arc::clone(peer.state);
    runtime.spawn(async move {
        let _permit = permit;
        let received = timeout(Duration::from_secs(30), async {
            let socket = TcpStream::connect((address, port)).await?;
            let mut tls =
                authenticated_stream(&state.config.tls, socket, &id, Some(certificate), false)
                    .await?;
            let directory = path.parent().context("missing attachment directory")?;
            private_dir(directory)?;
            let tempfile = tempfile::NamedTempFile::new_in(directory)?;
            let mut output = tokio::fs::File::from_std(tempfile.as_file().try_clone()?);
            ensure!(
                tokio::io::copy(&mut (&mut tls).take(size), &mut output).await? == size,
                "incomplete MMS attachment"
            );
            output.flush().await?;
            ensure!(
                state.messaging_enabled(&id, serial, SMS),
                "messaging connection closed"
            );
            tempfile.persist(path).map_err(|error| error.error)?;
            Ok::<_, anyhow::Error>(())
        })
        .await;
        if matches!(received, Ok(Ok(()))) {
            state.messaging_attachment_finished(&id, serial);
        }
    });
    true
}

fn cache<'a>(
    entries: &'a mut BTreeMap<String, Cache>,
    paths: &Paths,
    id: &str,
) -> Result<&'a mut Cache> {
    if entries.len() >= CACHE_COUNT && !entries.contains_key(id) {
        if let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, cache)| cache.last_used)
            .map(|(id, _)| id.clone())
        {
            entries.get_mut(&oldest).unwrap().flush()?;
            entries.remove(&oldest);
        }
    }
    let cache = entries
        .entry(id.to_owned())
        .or_insert_with(|| Cache::load(paths, id));
    cache.last_used = Instant::now();
    Ok(cache)
}

fn handle(
    request: Request,
    state: &Arc<State>,
    runtime: &Handle,
    paths: &Paths,
    entries: &mut BTreeMap<String, Cache>,
) {
    let (id, serial) = match &request {
        Request::Load { id, serial }
        | Request::Incoming { id, serial, .. }
        | Request::Action { id, serial, .. }
        | Request::AttachmentDone { id, serial } => (id.as_str(), *serial),
    };
    let peer = Session { state, id, serial };
    let enabled = match &request {
        Request::Incoming { packet, .. } => {
            incoming_plugin(&packet.kind).is_some_and(|plugin| peer.enabled(plugin))
        }
        Request::Action {
            plugin, response, ..
        } => !response.is_closed() && peer.enabled(plugin),
        Request::AttachmentDone { .. } => peer.enabled(SMS),
        Request::Load { .. } => peer.enabled(SMS) || peer.enabled(CONTACTS),
    };
    let result = if enabled {
        cache(entries, paths, id)
    } else {
        Err(anyhow::anyhow!("feature unavailable for this device"))
    };
    match request {
        Request::Action {
            id,
            plugin,
            action: verb,
            args,
            response,
            ..
        } => {
            let peer = Session {
                state,
                id: &id,
                serial,
            };
            let answer = result.and_then(|cache| {
                let answer = action(cache, &peer, &plugin, &verb, &args);
                cache.publish(state, &id, serial);
                answer
            });
            let _ = response.send(answer);
        }
        Request::Incoming { id, packet, .. } => {
            if let Ok(cache) = result {
                let peer = Session {
                    state,
                    id: &id,
                    serial,
                };
                if let Err(error) = cache.incoming(&peer, runtime, packet) {
                    eprintln!("Could not process messaging packet: {error}");
                }
                cache.publish(state, &id, serial);
            }
        }
        Request::AttachmentDone { id, .. } => {
            if let Ok(cache) = result {
                cache.changed(false);
                cache.publish(state, &id, serial);
            }
        }
        Request::Load { id, .. } => {
            if let Ok(cache) = result {
                cache.changed(false);
                cache.publish(state, &id, serial);
            }
        }
    }
}

fn serve(state: Weak<State>, runtime: Handle, paths: Paths, requests: mpsc::Receiver<Request>) {
    let mut caches = BTreeMap::new();
    loop {
        match requests.recv_timeout(Duration::from_millis(400)) {
            Ok(request) => {
                let Some(state) = state.upgrade() else {
                    break;
                };
                handle(request, &state, &runtime, &paths, &mut caches);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => (),
        }
        for cache in caches
            .values_mut()
            .filter(|cache| cache.save_at.is_some_and(|due| Instant::now() >= due))
        {
            if let Err(error) = cache.flush() {
                eprintln!("Could not save SMS history: {error}");
                cache.save_at = Some(Instant::now() + Duration::from_secs(5));
            }
        }
    }
    for cache in caches.values_mut() {
        if let Err(error) = cache.flush() {
            eprintln!("Could not save SMS history: {error}");
        }
    }
}

pub async fn run(state: &Arc<State>, requests: mpsc::Receiver<Request>) {
    match Paths::desktop() {
        Ok(paths) => {
            let state = Arc::downgrade(state);
            let runtime = Handle::current();
            let _ =
                tokio::task::spawn_blocking(move || serve(state, runtime, paths, requests)).await;
        }
        Err(error) => eprintln!("Messaging unavailable: {error}"),
    }
    std::future::pending::<()>().await;
}
