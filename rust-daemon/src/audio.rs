use crate::state::State;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdout, Command},
    sync::{mpsc, watch},
    time::{Instant, sleep_until, timeout},
};

pub const LOCAL: &str = "systemvolume";
pub const REMOTE: &str = "remotesystemvolume";
pub const INFO: &str = "kdeconnect.systemvolume";
pub const REQUEST: &str = "kdeconnect.systemvolume.request";
const NORMAL_VOLUME: u32 = 65536;
const MAX_SINKS: usize = 64;
const RETRY: Duration = Duration::from_secs(5);
const DEBOUNCE: Duration = Duration::from_millis(150);

pub type Targets = BTreeSet<(String, u64)>;
pub type Sinks = BTreeMap<String, Sink>;

pub struct Request {
    pub device: String,
    pub serial: u64,
    pub body: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sink {
    pub name: String,
    #[serde(default)]
    description: String,
    pub volume: u32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "normal_volume")]
    pub max_volume: u32,
}

fn normal_volume() -> u32 {
    NORMAL_VOLUME
}

fn name_valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)
}

pub fn available() -> bool {
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join("pactl").is_file()))
}

pub fn list(sinks: &Sinks) -> Value {
    json!({"sinkList": sinks.values().collect::<Vec<_>>()})
}

pub fn update_remote(sinks: &mut Sinks, mut body: Value) -> bool {
    if let Some(items) = body.get_mut("sinkList").and_then(Value::as_array_mut) {
        let next = items
            .iter_mut()
            .take(MAX_SINKS)
            .filter_map(|item| serde_json::from_value::<Sink>(item.take()).ok())
            .filter(|sink| name_valid(&sink.name))
            .map(|mut sink| {
                sink.description = sink.description.chars().take(256).collect();
                (sink.name.clone(), sink)
            })
            .collect();
        return if *sinks == next {
            false
        } else {
            *sinks = next;
            true
        };
    }
    let Some(sink) = body
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| sinks.get_mut(name))
    else {
        return false;
    };
    let (old_volume, old_muted, old_enabled) = (sink.volume, sink.muted, sink.enabled);
    if let Some(volume) = volume(&body) {
        sink.volume = volume;
    }
    if let Some(muted) = body.get("muted").and_then(Value::as_bool) {
        sink.muted = muted;
    }
    if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
        sink.enabled = enabled;
    }
    (old_volume, old_muted, old_enabled) != (sink.volume, sink.muted, sink.enabled)
}

pub fn volume(body: &Value) -> Option<u32> {
    volume_at(body, "volume")
}

fn pactl(args: &[&str]) -> Command {
    let mut command = Command::new("pactl");
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn read(args: &[&str], limit: u64) -> Result<Vec<u8>> {
    timeout(Duration::from_secs(2), async {
        let mut child = pactl(args).stdout(Stdio::piped()).spawn()?;
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .context("no pactl output")?
            .take(limit + 1)
            .read_to_end(&mut output)
            .await?;
        ensure!(
            output.len() as u64 <= limit && child.wait().await?.success(),
            "pactl command failed"
        );
        Ok(output)
    })
    .await?
}

async fn set(args: &[&str]) -> bool {
    timeout(
        Duration::from_secs(2),
        pactl(args).stdout(Stdio::null()).status(),
    )
    .await
    .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

async fn set_mute(name: &str, muted: bool) -> bool {
    set(&["set-sink-mute", name, if muted { "1" } else { "0" }]).await
}

async fn read_sinks() -> Result<Sinks> {
    let (sinks, default) = tokio::join!(
        read(&["-f", "json", "list", "sinks"], 512 * 1024),
        read(&["get-default-sink"], 1024)
    );
    let default = default.unwrap_or_default();
    let default = std::str::from_utf8(&default).unwrap_or_default().trim();
    let list: Vec<Value> = serde_json::from_slice(&sinks?)?;
    Ok(list
        .into_iter()
        .take(MAX_SINKS)
        .filter_map(|sink| {
            let name = sink.get("name")?.as_str()?.to_owned();
            if !name_valid(&name) || name.starts_with('-') || name.starts_with('@') {
                return None;
            }
            let description = sink
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(&name)
                .chars()
                .take(256)
                .collect();
            let (total, count) = sink
                .get("volume")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|channels| channels.values())
                .filter_map(|channel| volume_at(channel, "value"))
                .fold((0_u64, 0_u64), |(total, count), value| {
                    (total + u64::from(value), count + 1)
                });
            let volume = ((total + count / 2) / count.max(1)) as u32;
            Some((
                name.clone(),
                Sink {
                    enabled: name == default,
                    name,
                    description,
                    volume,
                    muted: sink.get("mute").and_then(Value::as_bool).unwrap_or(false),
                    max_volume: NORMAL_VOLUME,
                },
            ))
        })
        .collect())
}

fn volume_at(body: &Value, key: &str) -> Option<u32> {
    body.get(key)?.as_u64()?.try_into().ok()
}

async fn apply(state: &State, sinks: &Sinks, request: &Request) {
    let authorized = || state.local_volume_allowed(&request.device, request.serial);
    let Some(name) = request
        .body
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| sinks.contains_key(*name))
    else {
        return;
    };
    let requested_volume = volume(&request.body);
    let explicit_mute = request.body.get("muted").and_then(Value::as_bool);
    let mut mute = explicit_mute;
    if let Some(value) = requested_volume.filter(|_| authorized()) {
        if set(&[
            "set-sink-volume",
            name,
            &value.min(NORMAL_VOLUME).to_string(),
        ])
        .await
        {
            mute = Some(explicit_mute.unwrap_or(false));
        }
    }
    if let Some(muted) = mute.filter(|_| authorized()) {
        set_mute(name, muted).await;
    }
    if request.body.get("enabled").and_then(Value::as_bool) == Some(true) && authorized() {
        set(&["set-default-sink", name]).await;
    }
}

struct Events {
    _child: Child,
    stream: ChildStdout,
    line: Vec<u8>,
}

impl Events {
    fn open() -> Result<Self> {
        let mut child = pactl(&["subscribe"]).stdout(Stdio::piped()).spawn()?;
        let stream = child.stdout.take().context("no pactl event stream")?;
        Ok(Self {
            _child: child,
            stream,
            line: Vec::new(),
        })
    }
}

async fn audio_event(events: &mut Option<Events>) -> Option<bool> {
    let Some(events) = events else {
        return std::future::pending().await;
    };
    let mut chunk = [0; 2048];
    let size = events
        .stream
        .read(&mut chunk)
        .await
        .ok()
        .filter(|size| *size != 0)?;
    let mut changed = false;
    for part in chunk[..size].split_inclusive(|byte| *byte == b'\n') {
        let len = part.len().min(256_usize.saturating_sub(events.line.len()));
        events.line.extend_from_slice(&part[..len]);
        if part.last() == Some(&b'\n') {
            changed |= events.line.windows(9).any(|word| word == b" on sink ")
                || events.line.windows(11).any(|word| word == b" on server ");
            events.line.clear();
        }
    }
    Some(changed)
}

fn broadcast(state: &State, targets: &Targets, old: &Sinks, next: &Sinks) {
    let topology_changed = old.keys().ne(next.keys())
        || next.iter().any(|(name, sink)| {
            old.get(name)
                .is_some_and(|previous| previous.description != sink.description)
        });
    if topology_changed {
        let body = list(next);
        for (id, serial) in targets {
            state.send_volume(id, *serial, body.clone());
        }
        return;
    }
    for (name, sink) in next {
        if old.get(name) != Some(sink) {
            let body = json!({"name": name, "volume": sink.volume, "muted": sink.muted, "enabled": sink.enabled});
            for (id, serial) in targets {
                state.send_volume(id, *serial, body.clone());
            }
        }
    }
}

#[derive(Default)]
struct CallMute {
    restore: Option<bool>,
    muted: BTreeSet<String>,
}

impl CallMute {
    async fn update(&mut self, request: Option<bool>, sinks: &mut Sinks) {
        if let Some(restore) = request {
            let first = self.restore.is_none();
            self.restore = Some(self.restore.unwrap_or(true) && restore);
            if first {
                for sink in sinks.values_mut().filter(|sink| !sink.muted) {
                    if set_mute(&sink.name, true).await {
                        self.muted.insert(sink.name.clone());
                        sink.muted = true;
                    }
                }
            }
        } else if let Some(restore) = self.restore.take() {
            let changed = std::mem::take(&mut self.muted);
            if restore {
                for name in changed {
                    if set_mute(&name, false).await {
                        if let Some(sink) = sinks.get_mut(&name) {
                            sink.muted = false;
                        }
                    }
                }
            }
        }
    }
}

pub async fn run(
    state: &State,
    mut requests: mpsc::Receiver<Request>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut revision = state.subscribe();
    let mut targets = Targets::new();
    let mut announced = Targets::new();
    let mut due = None;
    let mut retry = Instant::now();
    let mut sinks = Sinks::new();
    let mut events = None;
    let mut calls = CallMute::default();
    loop {
        let wanted = state.local_volume_targets();
        let mute = state.call_audio().mute;
        if wanted != targets || (mute.is_some() && calls.restore.is_none()) {
            due = Some(Instant::now());
        }
        targets = wanted;
        let watching = !targets.is_empty() || mute.is_some();
        if !watching {
            events = None;
            announced.clear();
            due = None;
        }
        if watching && events.is_none() && Instant::now() >= retry {
            events = Events::open().ok();
            retry = Instant::now() + RETRY;
        }
        if mute.is_none() && calls.restore.is_some() {
            calls.update(None, &mut sinks).await;
            if !targets.is_empty() {
                due = Some(Instant::now());
            }
        } else if let (Some(before), Some(restore)) = (calls.restore, mute) {
            calls.restore = Some(before && restore);
        }
        let refresh = async {
            if let Some(due) = due {
                sleep_until(due).await;
            } else {
                std::future::pending().await
            }
        };
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = revision.changed() => (),
            _ = sleep_until(retry), if watching && events.is_none() => { due = Some(Instant::now()); },
            event = audio_event(&mut events) => match event {
                Some(true) => { due.get_or_insert_with(|| Instant::now() + DEBOUNCE); }
                Some(false) => (),
                None => {
                    events = None;
                    retry = Instant::now() + RETRY;
                }
            },
            _ = refresh => {
                due = None;
                if let Ok(mut next) = read_sinks().await {
                    calls.update(state.call_audio().mute, &mut next).await;
                    announced.retain(|target| targets.contains(target));
                    broadcast(state, &announced, &sinks, &next);
                    for (id, serial) in targets.difference(&announced).cloned().collect::<Vec<_>>() {
                        if state.send_volume(&id, serial, list(&next)) {
                            announced.insert((id, serial));
                        }
                    }
                    sinks = next;
                }
            }
            Some(request) = requests.recv() => {
                if state.local_volume_allowed(&request.device, request.serial) {
                    if request.body.get("requestSinks").is_some() {
                        announced.remove(&(request.device, request.serial));
                    } else {
                        apply(state, &sinks, &request).await;
                    }
                    due = Some(Instant::now());
                }
            }
        }
    }
    calls.update(None, &mut sinks).await;
}
