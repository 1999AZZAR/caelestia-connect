use crate::state::State;
use anyhow::Result;
use dbus::{
    Message, Path,
    blocking::LocalConnection,
    channel::{MatchingReceiver, Sender},
    message::MatchRule,
};
use dbus_crossroads::Crossroads;
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
};

const ITEM: &str = "org.kde.StatusNotifierItem";
const OBJECT: &str = "/StatusNotifierItem";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const APP: &str = "Caelestia Connect";

#[derive(Clone)]
pub struct Presentation {
    status: &'static str,
    icon: &'static str,
    description: String,
}

impl Presentation {
    pub fn new(status: &'static str, icon: &'static str, description: String) -> Self {
        let description = description
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        Self {
            status,
            icon,
            description,
        }
    }
}

struct TrayItem {
    presentation: Presentation,
    activate: mpsc::Sender<()>,
}

fn serve(mut changes: watch::Receiver<Presentation>, activate: mpsc::Sender<()>) -> Result<()> {
    let bus = LocalConnection::new_session()?;
    let path = Path::from(OBJECT);
    let mut service = Crossroads::new();
    let item = service.register(ITEM, |builder| {
        for (key, value) in [
            ("Category", "ApplicationStatus"),
            ("Id", "caelestia-connect"),
            ("Title", APP),
            ("IconThemePath", ""),
        ] {
            builder
                .property(key)
                .get(move |_, _: &mut TrayItem| Ok(value.to_owned()));
        }
        builder
            .property("Status")
            .get(|_, item: &mut TrayItem| Ok(item.presentation.status.to_owned()));
        builder
            .property("IconName")
            .get(|_, item: &mut TrayItem| Ok(item.presentation.icon.to_owned()));
        builder.property("ToolTip").get(|_, item: &mut TrayItem| {
            Ok((
                item.presentation.icon.to_owned(),
                Vec::<(i32, i32, Vec<u8>)>::new(),
                APP.to_owned(),
                item.presentation.description.clone(),
            ))
        });
        builder
            .property("ItemIsMenu")
            .get(|_, _: &mut TrayItem| Ok(false));
        builder
            .property("WindowId")
            .get(|_, _: &mut TrayItem| Ok(0_u32));
        builder
            .property("Menu")
            .get(|_, _: &mut TrayItem| Ok(Path::from("/NO_DBUSMENU")));
        for method in ["Activate", "SecondaryActivate", "ContextMenu"] {
            builder.method(
                method,
                ("x", "y"),
                (),
                |_, item: &mut TrayItem, _: (i32, i32)| {
                    let _ = item.activate.try_send(());
                    Ok(())
                },
            );
        }
        builder.method(
            "Scroll",
            ("delta", "orientation"),
            (),
            |_, _: &mut TrayItem, _: (i32, String)| Ok(()),
        );
        builder.signal::<(String,), _>("NewStatus", ("status",));
        builder.signal::<(), _>("NewIcon", ());
        builder.signal::<(), _>("NewToolTip", ());
    });
    service.insert(
        path.clone(),
        &[item],
        TrayItem {
            presentation: changes.borrow_and_update().clone(),
            activate,
        },
    );
    let mut method = MatchRule::new_method_call();
    method.path = Some(path.clone());
    let service = Rc::new(RefCell::new(service));
    let handlers = service.clone();
    bus.start_receive(
        method,
        Box::new(move |message, bus| {
            let _ = handlers.borrow_mut().handle_message(message, bus);
            true
        }),
    );

    let register = Rc::new(Cell::new(true));
    let watcher = register.clone();
    let mut owners = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    owners.sender = Some("org.freedesktop.DBus".into());
    bus.add_match(
        owners,
        move |(name, _, owner): (String, String, String), _, _| {
            if name == WATCHER && !owner.is_empty() {
                watcher.set(true);
            }
            true
        },
    )?;

    while let Ok(changed) = changes.has_changed() {
        if register.replace(false) {
            let watcher = bus.with_proxy(
                WATCHER,
                "/StatusNotifierWatcher",
                Duration::from_millis(900),
            );
            let _: Result<(), _> =
                watcher.method_call(WATCHER, "RegisterStatusNotifierItem", (OBJECT,));
        }
        if changed {
            let next = changes.borrow_and_update().clone();
            let mut service = service.borrow_mut();
            let tray: &mut TrayItem = service.data_mut(&path).expect("tray is registered");
            if tray.presentation.status != next.status {
                let _ = bus.send(signal("NewStatus").append1(next.status));
            }
            if tray.presentation.icon != next.icon {
                let _ = bus.send(signal("NewIcon"));
            }
            if tray.presentation.description != next.description {
                let _ = bus.send(signal("NewToolTip"));
            }
            tray.presentation = next;
        }
        bus.process(Duration::from_millis(150))?;
    }
    Ok(())
}

fn signal(name: &str) -> Message {
    Message::new_signal(OBJECT, ITEM, name).expect("valid tray signal")
}

pub fn open_window(args: &[&str]) -> std::io::Result<()> {
    let installed = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".local/bin/caelestia-connect"))
        .filter(|path| path.is_file());
    let mut child = Command::new(installed.unwrap_or_else(|| PathBuf::from("caelestia-connect")))
        .args(args)
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

pub async fn run(state: &Arc<State>) {
    let mut revision = state.subscribe();
    let (updates, presentation) = watch::channel(state.tray_status());
    let (activation, mut activate) = mpsc::channel(1);
    let mut worker = tokio::task::spawn_blocking(move || serve(presentation, activation));
    let mut last_click = None::<Instant>;
    loop {
        tokio::select! {
            _ = revision.changed() => { updates.send_replace(state.tray_status()); }
            Some(()) = activate.recv() => {
                if last_click.is_none_or(|previous| previous.elapsed() >= Duration::from_millis(400)) {
                    last_click = Some(Instant::now());
                    if let Err(error) = open_window(&[]) { eprintln!("Could not open Caelestia Connect: {error}"); }
                }
            }
            result = &mut worker => {
                if let Ok(Err(error)) = result { eprintln!("System tray unavailable: {error}"); }
                break;
            }
        }
    }
    std::future::pending::<()>().await;
}
