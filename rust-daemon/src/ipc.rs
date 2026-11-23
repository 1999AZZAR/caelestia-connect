use crate::{
    state::{PairCommand, State},
    wire,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::ErrorKind,
    net::Ipv4Addr,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::timeout,
};

pub struct SocketPath {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketPath {
    pub fn directory(&self) -> &Path {
        self.path.parent().expect("socket has a parent directory")
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn listen() -> Result<(UnixListener, SocketPath, u32)> {
    let runtime = PathBuf::from(
        env::var_os("CAELESTIA_CONNECT_IPC_RUNTIME_DIR")
            .or_else(|| env::var_os("XDG_RUNTIME_DIR"))
            .context("XDG_RUNTIME_DIR must be set")?,
    );
    ensure!(
        runtime.is_absolute(),
        "IPC runtime directory must be absolute"
    );
    let metadata = fs::metadata(&runtime)?;
    ensure!(
        metadata.is_dir() && metadata.mode() & 0o077 == 0,
        "IPC runtime directory must be private"
    );
    let dir = runtime.join("caelestia-connect");
    match fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => (),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let directory = fs::symlink_metadata(&dir)?;
    ensure!(
        directory.is_dir() && directory.mode() & 0o077 == 0 && directory.uid() == metadata.uid(),
        "daemon IPC directory must be private and owned by the runtime directory's user",
    );
    bind(&dir, directory.uid())
}

fn bind(dir: &Path, uid: u32) -> Result<(UnixListener, SocketPath, u32)> {
    let path = dir.join("ipc.sock");
    let listener = match UnixListener::bind(&path) {
        Err(error) if error.kind() == ErrorKind::AddrInUse && remove_stale(&path, uid)? => {
            UnixListener::bind(&path)
        }
        result => result,
    }
    .with_context(|| {
        format!(
            "bind {} (check for a running daemon or use a fresh private XDG_RUNTIME_DIR)",
            path.display()
        )
    })?;
    let identity = fs::symlink_metadata(&path)?;
    let guard = SocketPath {
        path,
        device: identity.dev(),
        inode: identity.ino(),
    };
    fs::set_permissions(&guard.path, fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard, uid))
}

fn remove_stale(path: &Path, uid: u32) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Ok(false);
    }
    let probe = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    probe.set_nonblocking(true)?;
    if !matches!(
        probe.connect(&socket2::SockAddr::unix(path)?),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused
    ) {
        return Ok(false);
    }
    let current = fs::symlink_metadata(path)?;
    if (metadata.dev(), metadata.ino()) != (current.dev(), current.ino()) {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

pub async fn run(
    listener: UnixListener,
    uid: u32,
    state: Arc<State>,
    refresh: mpsc::Sender<Option<Ipv4Addr>>,
) -> Result<()> {
    let clients = Arc::new(Semaphore::new(32));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (stream, _) = connection?;
                if !stream.peer_cred().is_ok_and(|peer| peer.uid() == uid) {
                    continue;
                }
                let Ok(permit) = clients.clone().try_acquire_owned() else { continue; };
                let state = state.clone();
                let refresh = refresh.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let _ = client(stream, &state, &refresh).await;
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => result.context("IPC worker failed")?,
        }
    }
}

async fn client(
    stream: UnixStream,
    state: &Arc<State>,
    refresh: &mpsc::Sender<Option<Ipv4Addr>>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut buffer = Vec::new();
    let mut changes = state.subscribe();
    let mut transfers = state.transfers.subscribe();
    let mut subscribed = false;
    loop {
        let response = tokio::select! {
            result = wire::read_frame(&mut reader, &mut buffer, wire::FRAME_LIMIT) => {
                let Some(frame) = result? else { return Ok(()); };
                match serde_json::from_slice::<Value>(&frame) {
                    Ok(request) => {
                        let id = request.get("id").cloned().unwrap_or(Value::Null);
                        let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
                        let params = request.get("params").filter(|value| !value.is_null()).cloned().unwrap_or(json!({}));
                        if method == "subscribe" && params.is_object() {
                            subscribed = true;
                            changes.borrow_and_update();
                        }
                        let result = dispatch(state, refresh, method, &params).await;
                        if id.is_null() { continue; }
                        match result {
                            Ok(value) => json!({"id": id, "result": value}),
                            Err(error) => json!({"id": id, "error": error.to_string()}),
                        }
                    }
                    Err(_) => json!({"error": "invalid json"}),
                }
            }
            _ = changes.changed(), if subscribed => json!({"event": "state", "data": state.snapshot()}),
            transfer = transfers.recv(), if subscribed => {
                match transfer {
                    Ok(data) => json!({"event": "transfer", "data": data}),
                    Err(_) => continue,
                }
            }
        };
        timeout(
            Duration::from_secs(5),
            wire::write_frame(&mut write, &response),
        )
        .await??;
    }
}

async fn dispatch(
    state: &Arc<State>,
    refresh: &mpsc::Sender<Option<Ipv4Addr>>,
    method: &str,
    params: &Value,
) -> Result<Value> {
    let params = wire::object(params)?;
    match method {
        "subscribe" | "state" => Ok(state.snapshot()),
        "ping" => Ok(json!("pong")),
        "refresh" => match refresh.try_send(None) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(None)) => Ok(json!(true)),
            Err(_) => bail!("discovery stopped"),
        },
        "settings.setName" => Ok(json!(
            state.set_name(
                params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("missing device name")?
            )?
        )),
        "settings.setIncomingPath" => {
            let path = params
                .get("path")
                .filter(|value| !value.is_null())
                .map(|value| value.as_str().context("expected a directory path"))
                .transpose()?
                .unwrap_or_default();
            state.set_incoming_path(path)?;
            Ok(json!(true))
        }
        "devices.add" | "devices.remove" => {
            let address: Ipv4Addr = params
                .get("address")
                .and_then(Value::as_str)
                .context("enter an IPv4 address")?
                .trim()
                .parse()
                .context("enter a valid IPv4 address")?;
            ensure!(
                !address.is_unspecified() && !address.is_multicast() && !address.is_broadcast(),
                "enter a device address"
            );
            let add = method == "devices.add";
            state.set_custom_address(address, add)?;
            if add {
                refresh
                    .send(Some(address))
                    .await
                    .context("discovery stopped")?;
            }
            Ok(json!(true))
        }
        "transfer.cancel" => {
            let id = params
                .get("id")
                .and_then(Value::as_u64)
                .context("invalid transfer ID")?;
            Ok(json!(state.transfers.cancel(id)))
        }
        "transfers.clear" => {
            state.transfers.clear();
            Ok(json!(true))
        }
        "device.setPluginSetting" => {
            let plugin = params
                .get("plugin")
                .and_then(Value::as_str)
                .context("missing plugin")?;
            let device = params
                .get("device")
                .and_then(Value::as_str)
                .filter(|id| wire::valid_id(id))
                .context("invalid device ID")?;
            let key = params
                .get("key")
                .and_then(Value::as_str)
                .context("missing setting name")?;
            state.set_plugin_setting(
                device,
                plugin,
                key,
                params
                    .get("value")
                    .cloned()
                    .context("missing setting value")?,
            )?;
            Ok(json!(true))
        }
        "device.setPluginEnabled" => {
            let device = params
                .get("device")
                .and_then(Value::as_str)
                .filter(|id| wire::valid_id(id))
                .context("invalid device ID")?;
            let plugin = params
                .get("plugin")
                .and_then(Value::as_str)
                .context("missing plugin")?;
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .context("expected an on/off value")?;
            state.set_plugin_enabled(device, plugin, enabled)?;
            Ok(json!(true))
        }
        "device.call" => {
            let device = params
                .get("device")
                .and_then(Value::as_str)
                .filter(|id| wire::valid_id(id))
                .context("invalid device ID")?;
            let args = params
                .get("args")
                .filter(|value| !value.is_null())
                .map(wire::object)
                .transpose()?;
            let arg = |name: &str| -> Result<Option<&str>> {
                args.and_then(|args| args.get(name))
                    .map(|value| value.as_str().context("expected text"))
                    .transpose()
            };
            match (
                params.get("plugin").and_then(Value::as_str),
                params.get("action").and_then(Value::as_str),
            ) {
                (Some("ping"), Some("ping")) => {
                    state
                        .ping(device, arg("message")?.unwrap_or_default())
                        .await?
                }
                (Some("clipboard"), Some("send_clipboard")) => {
                    return Ok(json!(state.send_clipboard(device).await?));
                }
                (Some(crate::state::telephony::PLUGIN), Some("mute")) => {
                    return Ok(json!(state.mute_call(device, None)?));
                }
                (Some("findmyphone"), Some("ring")) => {
                    state
                        .send(device, wire::Packet::new(crate::desktop::RING, json!({})))
                        .await?;
                }
                (Some("share"), Some("share_url")) => {
                    state
                        .share(device, true, arg("url")?.context("missing link")?)
                        .await?
                }
                (Some("share"), Some("share_text")) => {
                    state
                        .share(device, false, arg("text")?.context("missing text")?)
                        .await?
                }
                (Some("share"), Some("share_files")) => {
                    let args = args.context("missing file list")?;
                    let paths: Vec<String> = serde_json::from_value(
                        args.get("paths").cloned().context("missing file list")?,
                    )?;
                    let open = args.get("open").and_then(Value::as_bool).unwrap_or(false);
                    return Ok(json!(
                        crate::share::send_files(state, device, paths, open).await?
                    ));
                }
                (Some("share"), Some("open_folder")) => {
                    let directory = state.share_peer(device, None)?.directory;
                    tokio::fs::create_dir_all(&directory).await?;
                    std::process::Command::new("xdg-open")
                        .arg(directory)
                        .spawn()?;
                }
                (Some(crate::media::PLUGIN), Some(action)) => {
                    return Ok(json!(state.media_action(device, action, args)?));
                }
                (Some(crate::notifications::PLUGIN), Some(action)) => {
                    return Ok(json!(state.notification_action(device, action, args)?));
                }
                (
                    Some(plugin @ (crate::messaging::SMS | crate::messaging::CONTACTS)),
                    Some(action),
                ) => {
                    return state.messaging_action(device, plugin, action, args).await;
                }
                (Some(crate::phone_audio::PLUGIN), Some("toggle" | "set_on_computer")) => {
                    let on_computer = if params.get("action").and_then(Value::as_str)
                        == Some("set_on_computer")
                    {
                        Some(
                            args.and_then(|args| args.get("on_computer"))
                                .and_then(Value::as_bool)
                                .context("expected an audio output preference")?,
                        )
                    } else {
                        None
                    };
                    return state.route_phone_audio(device, on_computer).await;
                }
                _ => bail!("device action is unavailable in the experimental Rust daemon"),
            }
            Ok(json!(true))
        }
        "device.pair" | "device.accept" | "device.reject" | "device.unpair" => {
            let device = params
                .get("device")
                .and_then(Value::as_str)
                .filter(|id| wire::valid_id(id))
                .context("invalid device ID")?;
            let command = match method {
                "device.pair" => PairCommand::Request,
                "device.accept" => PairCommand::Accept,
                "device.reject" => PairCommand::Reject,
                _ => PairCommand::Unpair,
            };
            Ok(json!(state.pair_command(device, command)?))
        }
        _ => bail!("method is unavailable in the experimental Rust daemon"),
    }
}
