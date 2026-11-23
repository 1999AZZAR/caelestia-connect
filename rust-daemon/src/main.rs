mod audio;
mod clipboard;
mod config;
mod desktop;
mod idle;
mod ipc;
mod lan;
mod mdns;
mod media;
mod messaging;
mod notifications;
mod phone_audio;
mod share;
mod state;
mod transfers;
mod tray;
mod wire;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use dbus::blocking::{Connection, stdintf::org_freedesktop_dbus::RequestNameReply};
use std::{env, net::Ipv4Addr, path::PathBuf, sync::Arc};
use tokio::{
    net::UdpSocket,
    signal::{
        self,
        unix::{SignalKind, signal as unix_signal},
    },
    sync::{mpsc, watch},
};

#[derive(Parser)]
#[command(version, about = "Experimental Caelestia Connect LAN daemon")]
struct Args {
    /// Profile directory for identity, saved pairings, and settings
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Device name to advertise for this run
    #[arg(long)]
    name: Option<String>,

    /// Device name to use when the profile has no saved name
    #[arg(long, conflicts_with = "name")]
    default_name: Option<String>,

    /// IPv4 interface address for the discovery and connection listeners
    #[arg(long, default_value = "0.0.0.0")]
    bind: Ipv4Addr,

    /// Local UDP discovery port (0 selects a free port)
    #[arg(long, default_value_t = wire::DISCOVERY_PORT)]
    discovery_port: u16,

    /// Accept direct TCP connections without sending or receiving discovery packets
    #[arg(long)]
    no_discovery: bool,

    /// Run without registering a desktop tray icon
    #[arg(long)]
    no_tray: bool,
}

fn take_bus_name() -> Result<Connection> {
    let name = env::var("CAELESTIA_CONNECT_BUS_NAME")
        .unwrap_or_else(|_| "org.caelestia.Connect.Daemon".to_owned());
    dbus::strings::BusName::new(name.as_str())
        .map_err(anyhow::Error::msg)
        .context("invalid daemon D-Bus name")?;
    let bus = Connection::new_session().context("connect to the session D-Bus")?;
    let owner = bus
        .request_name(name.as_str(), false, false, true)
        .context("claim daemon D-Bus name")?;
    ensure!(
        matches!(
            owner,
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner
        ),
        "another Caelestia Connect daemon already owns {name}"
    );
    Ok(bus)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _bus = take_bus_name()?;
    let path = match args.config_dir {
        Some(path) => path,
        None => config::xdg_path("XDG_CONFIG_HOME", ".config")?.join("caelestia-connect"),
    };
    let (mut config, trust) = config::Config::load(&path)?;
    let fallback = args.name.is_none();
    if let Some(name) = args.name.or(args.default_name) {
        let name = wire::device_name(&name);
        ensure!(!name.is_empty(), "device name is empty");
        config.set_startup_name(name, fallback);
    }
    let tcp = lan::listen(args.bind).await?;
    let port = tcp.local_addr()?.port();
    let udp = if args.no_discovery {
        None
    } else {
        let udp = UdpSocket::bind((args.bind, args.discovery_port))
            .await
            .context("bind UDP discovery")?;
        udp.set_broadcast(true)?;
        Some(udp)
    };
    let mut terminate = unix_signal(SignalKind::terminate())?;
    let (ipc, socket, uid) = ipc::listen()?;
    let (local_media, media_requests) = mpsc::channel(32);
    let (phone_audio, audio_requests) = mpsc::channel(8);
    let (audio, volume_requests) = mpsc::channel(32);
    let (messaging, message_requests) = std::sync::mpsc::sync_channel(16);
    let (notifications, popups) = mpsc::channel(32);
    let state = Arc::new(state::State::new(
        config,
        trust,
        port,
        socket.directory().to_path_buf(),
        state::Workers {
            local_media,
            phone_audio,
            messaging,
            popups: notifications,
            audio,
        },
    ));
    let (refresh, scan) = mpsc::channel(1);
    eprintln!(
        "Experimental Caelestia Connect daemon listening on {}:{port}",
        args.bind
    );
    let (background_alive, audio_shutdown) = watch::channel(());
    let idle_service = Arc::clone(&state);
    let idle_shutdown = audio_shutdown.clone();
    let idle_worker = tokio::spawn(async move { idle::run(&idle_service, idle_shutdown).await });
    let service = Arc::clone(&state);
    let audio_worker = tokio::spawn(async move {
        if service.config.system_volume {
            audio::run(&service, volume_requests, audio_shutdown).await;
        }
    });
    let result = tokio::select! {
        result = lan::run(state.clone(), tcp, udp, args.bind, scan) => result,
        result = ipc::run(ipc, uid, state.clone(), refresh) => result,
        _ = state.expire_pairings() => Ok(()),
        _ = state.report_battery() => Ok(()),
        _ = state.run_clipboard() => Ok(()),
        _ = state.expire_idle_media() => Ok(()),
        _ = media::run_exports(&state) => Ok(()),
        _ = media::local::run(&state, media_requests) => Ok(()),
        _ = phone_audio::run(&state, audio_requests) => Ok(()),
        _ = messaging::run(&state, message_requests) => Ok(()),
        _ = notifications::run(&state, popups) => Ok(()),
        _ = state.resync_notifications() => Ok(()),
        _ = tray::run(&state), if !args.no_tray => Ok(()),
        result = signal::ctrl_c() => result.context("listen for SIGINT"),
        _ = terminate.recv() => Ok(()),
    };
    drop(background_alive);
    tokio::try_join!(audio_worker, idle_worker)?;
    result
}
