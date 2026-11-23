use crate::state::State;
use anyhow::{Context, Result, ensure};
use dbus::{
    Path,
    arg::{PropMap, prop_cast},
    blocking::LocalConnection,
    message::MatchRule,
};
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

pub const PLUGIN: &str = "phoneaudio";
const BLUEZ: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";
const TRANSPORT: &str = "org.bluez.MediaTransport1";
const OBJECTS: &str = "org.freedesktop.DBus.ObjectManager";
const A2DP_SOURCE: &str = "0000110a-0000-1000-8000-00805f9b34fb";

#[derive(Clone, PartialEq, Eq)]
pub struct Phone {
    pub path: Path<'static>,
    address: String,
    name: String,
    pub on_computer: bool,
    pub playing: bool,
}

pub struct Request {
    pub id: String,
    pub serial: u64,
    pub path: Path<'static>,
    pub on_computer: Option<bool>,
    pub completion: oneshot::Sender<Result<bool>>,
}

pub fn find<'a>(phones: &'a [Phone], address: &str, name: &str) -> Option<&'a Phone> {
    if !address.is_empty() {
        if let Some(phone) = phones
            .iter()
            .find(|phone| phone.address.eq_ignore_ascii_case(address))
        {
            return Some(phone);
        }
    }
    let mut matched = phones.iter().filter(|phone| {
        phone.name.eq_ignore_ascii_case(name) || phone.name.to_lowercase() == name.to_lowercase()
    });
    let phone = matched.next()?;
    matched.next().is_none().then_some(phone)
}

fn devices(bus: &LocalConnection) -> Result<Vec<Phone>> {
    type Objects = HashMap<Path<'static>, HashMap<String, PropMap>>;
    let proxy = bus.with_proxy(BLUEZ, "/", Duration::from_secs(2));
    let (objects,): (Objects,) = proxy.method_call(OBJECTS, "GetManagedObjects", ())?;
    let mut transports = BTreeMap::new();
    for props in objects
        .values()
        .filter_map(|interfaces| interfaces.get(TRANSPORT))
    {
        let Some(device) = prop_cast::<Path<'static>>(props, "Device") else {
            continue;
        };
        let playing = prop_cast::<String>(props, "State").is_some_and(|state| state == "active");
        *transports.entry(device).or_insert(false) |= playing;
    }
    let mut phones: Vec<_> = objects
        .iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get(DEVICE)?;
            let paired = prop_cast::<bool>(props, "Paired").copied().unwrap_or(false);
            let audio = prop_cast::<Vec<String>>(props, "UUIDs").is_some_and(|uuids| {
                uuids
                    .iter()
                    .any(|uuid| uuid.eq_ignore_ascii_case(A2DP_SOURCE))
            });
            if !paired || !audio {
                return None;
            }
            let address = prop_cast::<String>(props, "Address")?.clone();
            let name = prop_cast::<String>(props, "Alias")
                .filter(|alias| !alias.is_empty())
                .or_else(|| prop_cast::<String>(props, "Name"))
                .cloned()
                .unwrap_or_default();
            Some(Phone {
                path: path.clone(),
                address,
                name,
                on_computer: transports.contains_key(path),
                playing: transports.get(path).copied().unwrap_or(false),
            })
        })
        .collect();
    phones.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(phones)
}

fn route(bus: &LocalConnection, path: &Path<'static>, on_computer: bool) -> Result<()> {
    let target = bus.with_proxy(BLUEZ, path.clone(), Duration::from_secs(8));
    let method = if on_computer {
        "ConnectProfile"
    } else {
        "DisconnectProfile"
    };
    Ok(target.method_call(DEVICE, method, (A2DP_SOURCE,))?)
}

fn subscribe(bus: &LocalConnection) -> Result<Rc<Cell<bool>>> {
    let refresh = Rc::new(Cell::new(true));
    let changes = refresh.clone();
    let mut signals = MatchRule::new();
    signals.msg_type = Some(dbus::MessageType::Signal);
    signals.sender = Some(BLUEZ.into());
    bus.add_match(signals, move |_: (), _, _| {
        changes.set(true);
        true
    })?;
    let service = refresh.clone();
    let mut owners = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    owners.sender = Some("org.freedesktop.DBus".into());
    bus.add_match(
        owners,
        move |(name, _, _): (String, String, String), _, _| {
            if name == BLUEZ {
                service.set(true);
            }
            true
        },
    )?;
    Ok(refresh)
}

fn handle(bus: &LocalConnection, state: &State, request: &Request) -> Result<bool> {
    let phone = state
        .phone_audio_target(&request.id, request.serial)
        .context("phone not found")?;
    ensure!(phone.path == request.path, "Bluetooth device changed");
    let on_computer = request.on_computer.unwrap_or(!phone.on_computer);
    if on_computer != phone.on_computer {
        route(bus, &phone.path, on_computer)?;
        state.set_phone_audio_devices(devices(bus).unwrap_or_default());
    }
    Ok(on_computer)
}

fn serve(
    state: Weak<State>,
    mut requests: mpsc::Receiver<Request>,
    active: watch::Receiver<()>,
) -> Result<()> {
    let bus = LocalConnection::new_system()?;
    let refresh = subscribe(&bus)?;
    let mut updated = None::<Instant>;
    let mut managed = BTreeMap::new();
    while active.has_changed().is_ok() {
        let Some(state) = state.upgrade() else {
            break;
        };
        if refresh.get() && updated.is_none_or(|last| last.elapsed() >= Duration::from_millis(300))
        {
            refresh.set(false);
            updated = Some(Instant::now());
            state.set_phone_audio_devices(devices(&bus).unwrap_or_default());
        }
        managed.retain(|path, (id, serial): &mut (String, u64)| {
            if state
                .phone_audio_target(id, *serial)
                .is_some_and(|phone| &phone.path == path)
            {
                true
            } else {
                let _ = route(&bus, path, false);
                false
            }
        });
        if let Ok(request) = requests.try_recv() {
            let before = state.phone_audio_target(&request.id, request.serial);
            let result = handle(&bus, &state, &request);
            if let Ok(on_computer) = result {
                if !on_computer {
                    managed.remove(&request.path);
                } else if before.is_some_and(|phone| !phone.on_computer) {
                    managed.insert(request.path, (request.id, request.serial));
                }
            }
            let _ = request.completion.send(result);
        }
        drop(state);
        bus.process(Duration::from_millis(200))?;
    }
    for path in managed.keys() {
        let _ = route(&bus, path, false);
    }
    Ok(())
}

pub async fn run(state: &Arc<State>, requests: mpsc::Receiver<Request>) {
    let (lifetime, active) = watch::channel(());
    let state = Arc::downgrade(state);
    let worker = tokio::task::spawn_blocking(move || serve(state, requests, active));
    if let Ok(Err(error)) = worker.await {
        eprintln!("Bluetooth audio unavailable: {error}");
    }
    drop(lifetime);
    std::future::pending::<()>().await;
}
