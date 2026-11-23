use crate::state::State;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader},
    process::Command,
    sync::oneshot,
};

pub const LOCAL: &str = "runcommand";
pub const REMOTE: &str = "remotecommands";
pub const LIST: &str = "kdeconnect.runcommand";
pub const REQUEST: &str = "kdeconnect.runcommand.request";
pub const OUTPUT: &str = "kdeconnect.runcommand.output";
const MAX_COMMANDS: usize = 64;
const MAX_SCRIPT: usize = 4096;
const MAX_NAME: usize = 256;
const MAX_LIST_BYTES: usize = 52 * 1024;
const OUTPUT_BYTES: u64 = 48 * 1024;
const MAX_JOBS: usize = 4;
const MAX_DURATION: Duration = Duration::from_secs(120);

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Definition {
    pub name: String,
    pub command: String,
}

pub type Definitions = BTreeMap<String, Definition>;

impl Definition {
    pub fn new(name: &str, command: &str) -> Result<Self> {
        ensure!(
            !name.trim().is_empty()
                && name.len() <= MAX_NAME
                && !name.chars().any(char::is_control),
            "invalid command name"
        );
        ensure!(
            !command.trim().is_empty() && command.len() <= MAX_SCRIPT && !command.contains('\0'),
            "invalid command script"
        );
        Ok(Self {
            name: name.trim().to_owned(),
            command: command.to_owned(),
        })
    }
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 128 && !key.chars().any(char::is_control)
}

pub fn defaults() -> Definitions {
    [
        ("lock", "Lock screen", "loginctl lock-session"),
        ("suspend", "Suspend", "systemctl suspend"),
        (
            "mute",
            "Mute / unmute",
            "wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle",
        ),
        ("screenshot", "Screenshot", "caelestia screenshot"),
    ]
    .into_iter()
    .map(|(key, name, command)| {
        (
            key.to_owned(),
            Definition {
                name: name.to_owned(),
                command: command.to_owned(),
            },
        )
    })
    .collect()
}

pub fn load(saved: Option<&Value>) -> Definitions {
    let Some(saved) = saved else {
        return defaults();
    };
    let mut defs = Definitions::new();
    if let Some(entries) = saved.as_object() {
        for (key, value) in entries
            .iter()
            .filter(|(key, _)| valid_key(key))
            .take(MAX_COMMANDS)
        {
            let Some(name) = value.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(command) = value.get("command").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(definition) = Definition::new(name, command) {
                defs.insert(key.clone(), definition);
            }
        }
    }
    defs
}

pub fn advertise(commands: &Definitions) -> Value {
    inventory(commands).unwrap_or_else(|_| json!({"commandList": "{}", "canAddCommand": true}))
}

pub fn validate(commands: &Definitions) -> Result<()> {
    inventory(commands).map(drop)
}

fn inventory(commands: &Definitions) -> Result<Value> {
    ensure!(commands.len() <= MAX_COMMANDS, "too many commands");
    let body = json!({"commandList": serde_json::to_string(commands)?, "canAddCommand": true});
    ensure!(
        serde_json::to_vec(&body)?.len() <= MAX_LIST_BYTES,
        "command list exceeds its size limit"
    );
    Ok(body)
}

pub fn definitions_snapshot(commands: &Definitions) -> Vec<Value> {
    commands
        .iter()
        .map(|(key, def)| json!({"key": key, "name": def.name, "command": def.command}))
        .collect()
}

#[derive(Default)]
pub struct Remote {
    commands: Definitions,
    can_add: bool,
    output: VecDeque<String>,
}

impl Remote {
    pub fn update(&mut self, kind: &str, body: &Value) -> bool {
        if kind == OUTPUT {
            let mut changed = false;
            if body.get("commandStarted").and_then(Value::as_bool) == Some(true) {
                self.output.clear();
                self.push(&format!(
                    "$ {}",
                    body.get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ));
                changed = true;
            }
            for key in ["stdout", "stderr"] {
                for line in body
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(200)
                {
                    self.push(line);
                    changed = true;
                }
            }
            if body.get("commandFinished").and_then(Value::as_bool) == Some(true) {
                self.push(&format!(
                    "[exit code {}]",
                    body.get("exitCode")
                        .and_then(Value::as_i64)
                        .unwrap_or_default()
                ));
                changed = true;
            }
            return changed;
        }
        let Some(raw) = body.get("commandList") else {
            return false;
        };
        let decoded = raw
            .as_str()
            .filter(|text| text.len() <= MAX_LIST_BYTES)
            .and_then(|text| serde_json::from_str(text).ok());
        let mut next = Definitions::new();
        if let Some(entries) = decoded.as_ref().unwrap_or(raw).as_object() {
            for (key, row) in entries
                .iter()
                .filter(|(key, row)| valid_key(key) && row.is_object())
                .take(MAX_COMMANDS)
            {
                let name = row
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(key);
                let name = name
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_NAME)
                    .collect();
                let command = row
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .chars()
                    .take(MAX_SCRIPT)
                    .collect();
                next.insert(key.clone(), Definition { name, command });
            }
        }
        let can_add = body
            .get("canAddCommand")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        if self.commands == next && self.can_add == can_add {
            return false;
        }
        self.commands = next;
        self.can_add = can_add;
        true
    }

    fn push(&mut self, line: &str) {
        if self.output.len() == 200 {
            self.output.pop_front();
        }
        self.output
            .push_back(line.chars().filter(|ch| *ch != '\0').take(2048).collect());
    }

    pub fn contains(&self, key: &str) -> bool {
        self.commands.contains_key(key)
    }

    pub fn can_add(&self) -> bool {
        self.can_add
    }

    pub fn snapshot(&self) -> Value {
        json!({"commands": definitions_snapshot(&self.commands), "canAddCommand": self.can_add, "output": self.output})
    }
}

struct Job {
    device: String,
    serial: u64,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
struct ActiveJobs {
    next: u64,
    entries: BTreeMap<u64, Job>,
}

#[derive(Default)]
pub struct Jobs(Mutex<ActiveJobs>);

impl Jobs {
    pub fn start(&self, state: &Arc<State>, device: &str, serial: u64, key: String) {
        let mut jobs = self.0.lock().unwrap();
        if jobs.entries.len() >= MAX_JOBS {
            return;
        }
        let id = jobs.next;
        jobs.next = id.wrapping_add(1);
        let (cancel, stop) = oneshot::channel();
        jobs.entries.insert(
            id,
            Job {
                device: device.to_owned(),
                serial,
                cancel: Some(cancel),
            },
        );
        drop(jobs);
        let state = Arc::clone(state);
        let device = device.to_owned();
        tokio::spawn(async move {
            let session = Execution {
                state: &state,
                device: &device,
                serial,
                id,
            };
            session.run(key, stop).await;
            state.command_jobs.finished(id);
        });
    }

    pub fn stop(&self, device: &str, serial: u64) {
        let mut jobs = self.0.lock().unwrap();
        for job in jobs
            .entries
            .values_mut()
            .filter(|job| job.device == device && job.serial == serial)
        {
            if let Some(cancel) = job.cancel.take() {
                let _ = cancel.send(());
            }
        }
    }

    fn finished(&self, id: u64) {
        self.0.lock().unwrap().entries.remove(&id);
    }
}

struct ProcessGroup(Option<libc::pid_t>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The child starts a process group with its own PID as the group ID.
        if let Some(id) = self.0 {
            unsafe { libc::kill(-id, libc::SIGKILL) };
        }
    }
}

struct Execution<'a> {
    state: &'a State,
    device: &'a str,
    serial: u64,
    id: u64,
}

impl Execution<'_> {
    fn send(&self, mut body: Value) {
        body["id"] = json!(self.id);
        self.state
            .send_command(self.device, self.serial, OUTPUT, body);
    }

    async fn output(&self, stream: impl AsyncRead + Unpin, key: &str) {
        let mut stream = BufReader::new(stream.take(OUTPUT_BYTES));
        let mut buf = [0_u8; 4096];
        let mut pending = Vec::new();
        while let Ok(len) = stream.read(&mut buf).await {
            if len == 0 {
                break;
            }
            pending.extend(&buf[..len]);
            let mut lines = Vec::new();
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                lines.push(String::from_utf8_lossy(&pending[..end]).into_owned());
                pending.drain(..=end);
            }
            if pending.len() >= buf.len() {
                lines.push(String::from_utf8_lossy(&pending).into_owned());
                pending.clear();
            }
            for lines in lines.chunks(32) {
                self.send_lines(key, lines);
            }
        }
        if !pending.is_empty() {
            self.send_lines(key, &[String::from_utf8_lossy(&pending).into_owned()]);
        }
    }

    fn send_lines(&self, key: &str, lines: &[String]) {
        let mut body = json!({"commandOutput": true, "stdout": [], "stderr": []});
        body[key] = json!(lines);
        self.send(body);
    }

    async fn run(&self, key: String, mut stopped: oneshot::Receiver<()>) {
        let mut revision = self.state.subscribe();
        if !matches!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
            return;
        }
        let Some(script) = self
            .state
            .local_command_script(self.device, self.serial, &key)
        else {
            return;
        };
        let mut process = Command::new("/bin/sh");
        process
            .arg("-c")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        if let Some(home) = std::env::var_os("HOME") {
            process.current_dir(home);
        }
        self.send(json!({"commandStarted": true, "command": script}));
        let code = match process.spawn() {
            Ok(mut child) => {
                let mut group = ProcessGroup(child.id().map(|pid| pid as libc::pid_t));
                let stdout = child.stdout.take().expect("command stdout is piped");
                let stderr = child.stderr.take().expect("command stderr is piped");
                let exited = async {
                    tokio::join!(self.output(stdout, "stdout"), self.output(stderr, "stderr"));
                    child
                        .wait()
                        .await
                        .ok()
                        .and_then(|status| status.code())
                        .unwrap_or(125)
                };
                let revoked = async {
                    while revision.changed().await.is_ok() {
                        if !self.state.local_commands_allowed(self.device, self.serial) {
                            break;
                        }
                    }
                };
                let code = tokio::select! {
                    code = exited => { group.0 = None; code }
                    _ = stopped => 130,
                    _ = revoked => 130,
                    _ = tokio::time::sleep(MAX_DURATION) => 124,
                };
                drop(group);
                if child.id().is_some() {
                    let _ = child.wait().await;
                }
                code
            }
            Err(_) => 127,
        };
        self.send(json!({"commandFinished": true, "success": code == 0, "exitCode": code}));
    }
}

pub fn key() -> Result<String> {
    let mut bytes = [0; 6];
    openssl::rand::rand_bytes(&mut bytes).context("cannot allocate command ID")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
