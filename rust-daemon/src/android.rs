mod adb;

use crate::state::State;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    process::Child,
    sync::{Mutex as AsyncMutex, Notify, watch},
    time::Instant,
};

pub const PLUGIN: &str = "android";
const NOT_CONNECTED: &str = "The phone is not connected over USB or wireless debugging.";
const CAMERA_ERROR: &str =
    "Phone camera failed. Android 12 or newer is needed, and the camera must be free.";
const CAMERA_RELEASE: Duration = Duration::from_secs(1);

static SCHEMA: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("android/settings.json"))
        .expect("valid Android feature settings")
});

#[derive(Clone, Default)]
pub struct Settings(Map<String, Value>);

impl Settings {
    pub fn read(saved: Option<&Value>) -> Self {
        let values = saved
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(key, value)| validate(key, value).is_ok())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self(values)
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.0.insert(key.to_owned(), value);
    }

    pub fn plugin(&self, enabled: bool) -> Value {
        let settings: Vec<_> = SCHEMA
            .iter()
            .filter(|option| option["key"] != "webcam")
            .collect();
        let values = settings
            .iter()
            .map(|option| {
                let key = option["key"].as_str().expect("setting name");
                (key.to_owned(), self.value(key).clone())
            })
            .collect::<Map<_, _>>();
        json!({"id":PLUGIN, "name":"Screen mirroring and camera",
            "description":"Mirror the screen and use the camera over USB or wireless debugging",
            "enabled":enabled, "loaded":enabled, "settings":settings, "values":values})
    }

    fn value(&self, key: &str) -> &Value {
        self.0.get(key).unwrap_or_else(|| {
            &SCHEMA
                .iter()
                .find(|option| option["key"] == key)
                .expect("defined setting")["default"]
        })
    }

    fn enabled(&self, key: &str) -> bool {
        self.value(key).as_bool().unwrap_or_default()
    }
    fn number(&self, key: &str) -> u16 {
        self.value(key).as_u64().unwrap_or_default() as u16
    }
    fn text(&self, key: &str) -> &str {
        self.value(key).as_str().unwrap_or_default()
    }
}

pub fn validate(key: &str, value: &Value) -> Result<()> {
    let option = SCHEMA
        .iter()
        .find(|option| option["key"] == key)
        .context("unknown Android feature setting")?;
    match option["type"].as_str() {
        Some("bool") => ensure!(value.is_boolean(), "expected an on/off value"),
        Some("choice") => ensure!(
            option["choices"]
                .as_array()
                .is_some_and(|choices| choices.iter().any(|choice| &choice[0] == value)),
            "unsupported Android feature option"
        ),
        Some("str") => {
            let text = value.as_str().context("expected text")?;
            ensure!(
                text.len() <= 4096 && !text.contains('\0'),
                "invalid scrcpy option"
            );
            if key == "custom_args" {
                custom_args(text)?;
            }
        }
        _ => unreachable!("known setting type"),
    }
    Ok(())
}

fn custom_args(text: &str) -> Result<Vec<String>> {
    let args = shlex::split(text).context("Invalid quoting in custom scrcpy options.")?;
    for arg in &args {
        let flag = arg.split('=').next().unwrap_or_default();
        ensure!(
            !matches!(
                flag,
                "--serial" | "--tcpip" | "--select-usb" | "--select-tcpip"
            ) && !arg.starts_with("-s")
                && !matches!(arg.as_str(), "-d" | "-e" | "--"),
            "Choose the Android device in Caelestia Connect."
        );
    }
    Ok(args)
}

pub fn supports(device_type: &str) -> bool {
    matches!(device_type, "phone" | "tablet")
}

pub struct Device {
    pub name: String,
    pub address: Option<Ipv4Addr>,
    pub options: Settings,
    pub phoneaudio: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct View {
    mirroring: bool,
    preview: bool,
    screen_on: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            mirroring: false,
            preview: false,
            screen_on: true,
        }
    }
}

#[derive(Default)]
pub struct Manager {
    sessions: Mutex<BTreeMap<String, Arc<Entry>>>,
    awake: Notify,
}

#[derive(Default)]
struct Entry {
    session: AsyncMutex<Session>,
    view: Mutex<View>,
}

#[derive(Default)]
struct Session {
    screen: Option<Screen>,
    camera: Option<Camera>,
    view: View,
}

struct Screen {
    serial: String,
    child: Child,
    restore: bool,
    check_at: Instant,
}

struct Camera {
    serial: String,
    child: Child,
    facing: String,
    mirror: bool,
    sensor: u16,
    rotation: u16,
    angle: u16,
    check_at: Instant,
}

fn tools_available() -> bool {
    adb::installed("adb") && adb::installed("scrcpy")
}

impl Entry {
    fn publish(&self, state: &State, session: &Session) {
        let mut shown = self.view.lock().unwrap();
        if *shown != session.view {
            *shown = session.view;
            state.changed();
        }
    }
}

impl Manager {
    pub fn snapshot(&self, id: &str) -> Value {
        let shown = self
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .map_or_else(View::default, |entry| *entry.view.lock().unwrap());
        json!({"tools":tools_available(), "mirroring":shown.mirroring, "screenOn":shown.screen_on,
            "preview":shown.preview})
    }

    pub async fn call(
        &self,
        state: &State,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value> {
        let phone = state.android_device(id)?;
        let parsed = Arguments(args);
        if action == "scan" {
            if !tools_available() {
                return Ok(json!({"serial":"", "devices":[], "unauthorized":false}));
            }
            return Ok(resolve(&phone, "")
                .await
                .map(|scan| json!(scan))
                .unwrap_or_else(|error| json!({"error": error.to_string()})));
        }
        let entry = self
            .sessions
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone();
        let mut session = entry.session.lock().await;
        let result = session.call(state, id, action, &parsed).await;
        entry.publish(state, &session);
        self.awake.notify_one();
        Ok(result.unwrap_or_else(|error| json!({"error": error.to_string()})))
    }

    pub async fn run(&self, state: &State, mut shutdown: watch::Receiver<()>) {
        loop {
            let active = self.sessions.lock().unwrap().values().any(|entry| {
                let shown = entry.view.lock().unwrap();
                shown.mirroring || shown.preview
            });
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = self.awake.notified() => (),
                _ = tokio::time::sleep(Duration::from_secs(1)), if active => (),
            }
            {
                let sessions = self.sessions.lock().unwrap().clone();
                for (id, entry) in sessions {
                    let Ok(mut session) = entry.session.try_lock() else {
                        continue;
                    };
                    if !session.view.mirroring && !session.view.preview {
                        continue;
                    }
                    match state.android_device(&id) {
                        Ok(phone) => session.tick(&phone).await,
                        Err(_) => session.stop().await,
                    }
                    entry.publish(state, &session);
                }
            }
            self.sessions.lock().unwrap().retain(|_, entry| {
                let shown = entry.view.lock().unwrap();
                Arc::strong_count(entry) > 1 || *shown != View::default()
            });
        }
        let sessions = self.sessions.lock().unwrap().clone();
        for entry in sessions.values() {
            entry.session.lock().await.stop().await;
        }
    }
}

struct Arguments<'a>(Option<&'a Map<String, Value>>);

impl Arguments<'_> {
    fn value(&self, key: &str) -> Option<&Value> {
        self.0?.get(key).filter(|value| !value.is_null())
    }

    fn text(&self, key: &str) -> Result<&str> {
        self.value(key)
            .map(|value| value.as_str().context("expected text"))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    fn boolean(&self, key: &str) -> Result<Option<bool>> {
        self.value(key)
            .map(|value| value.as_bool().context("expected an on/off value"))
            .transpose()
    }
}

async fn resolve(phone: &Device, entered: &str) -> Result<adb::Scan> {
    if !entered.is_empty() {
        let mut scan = adb::devices().await?;
        ensure!(
            scan.devices.iter().any(|serial| serial == entered),
            "Choose an authorized Android debugging device."
        );
        scan.serial = entered.to_owned();
        return Ok(scan);
    }
    tokio::time::timeout(
        Duration::from_secs(9),
        adb::find(&phone.name, phone.address),
    )
    .await
    .context("Android device scan timed out")?
}

async fn serial(phone: &Device, entered: &str) -> Result<String> {
    let scan = resolve(phone, entered).await?;
    ensure!(!scan.serial.is_empty(), NOT_CONNECTED);
    Ok(scan.serial)
}

fn mirror_options(phone: &Device, audio: Option<bool>, off: bool) -> Result<Vec<String>> {
    let options = &phone.options;
    let mut argv = vec![format!("--window-title={}", phone.name)];
    if !audio.unwrap_or(phone.phoneaudio && options.enabled("audio")) {
        argv.push("--no-audio".into());
    }
    if off {
        argv.push("--turn-screen-off".into());
    }
    if options.enabled("stay_awake") {
        argv.push("--stay-awake".into());
    }
    for (option, flag) in [("max_size", "--max-size"), ("max_fps", "--max-fps")] {
        let number = options.number(option);
        if number != 0 {
            argv.push(format!("{flag}={number}"));
        }
    }
    let bit_rate = options.text("video_bit_rate");
    if bit_rate != "8M" {
        argv.push(format!("--video-bit-rate={bit_rate}"));
    }
    let display = options.text("new_display").trim();
    if !display.is_empty() {
        argv.push(format!("--new-display={display}"));
    }
    argv.extend(custom_args(options.text("custom_args"))?);
    Ok(argv)
}

fn orientation(facing: &str, sensor: u16, rotation: u16, extra: u16) -> u16 {
    (sensor
        + if facing == "front" {
            360 - rotation
        } else {
            rotation
        }
        + extra)
        % 360
}

async fn camera_child(
    phone: &Device,
    serial: &str,
    facing: &str,
    angle: u16,
    mirror: bool,
) -> Result<Child> {
    let display = if mirror && facing == "front" {
        // scrcpy flips before rotating. Reverse the rotation to mirror the upright view.
        format!("flip{}", (360 - angle) % 360)
    } else {
        angle.to_string()
    };
    let opts = [
        "--video-source=camera".into(),
        format!("--camera-facing={facing}"),
        "--no-audio".into(),
        "--max-size=1280".into(),
        format!("--display-orientation={display}"),
        format!("--window-title={}", phone.name),
    ];
    let mut child = adb::scrcpy(serial, &opts).context(CAMERA_ERROR)?;
    adb::started(&mut child).await.context(CAMERA_ERROR)?;
    Ok(child)
}

impl Session {
    async fn call(
        &mut self,
        state: &State,
        id: &str,
        action: &str,
        args: &Arguments<'_>,
    ) -> Result<Value> {
        let phone = state.android_device(id)?;
        match action {
            "mirror" => {
                if self.screen.is_some() {
                    return Ok(json!({}));
                }
                let serial = serial(&phone, args.text("serial")?).await?;
                let off = args
                    .boolean("screen_off")?
                    .unwrap_or(phone.options.enabled("turn_screen_off"));
                let power = adb::power(&serial).await;
                state.android_device(id)?;
                let argv = mirror_options(&phone, args.boolean("audio")?, off)?;
                let mut child =
                    adb::scrcpy(&serial, &argv).context("Could not start screen mirroring.")?;
                if adb::started(&mut child).await.is_err() {
                    if off && power != Some(false) {
                        let _ = adb::set_power(&serial, true).await;
                    }
                    bail!(
                        "Screen mirroring failed. Check the Android debugging connection and try again."
                    );
                }
                let screen_on = adb::power(&serial).await.unwrap_or(!off);
                if let Err(error) = state.android_device(id) {
                    adb::stop(child).await;
                    if off && power != Some(false) {
                        let _ = adb::set_power(&serial, true).await;
                    }
                    return Err(error);
                }
                self.view.screen_on = screen_on;
                self.view.mirroring = true;
                self.screen = Some(Screen {
                    serial,
                    child,
                    restore: off && power != Some(false),
                    check_at: Instant::now(),
                });
                Ok(json!({}))
            }
            "stop_mirror" => {
                self.stop_screen().await;
                Ok(Value::Null)
            }
            "toggle_screen" | "set_screen" => {
                let chosen = match &self.screen {
                    Some(screen) => screen.serial.clone(),
                    None => serial(&phone, "").await?,
                };
                let actual = adb::power(&chosen).await.unwrap_or(self.view.screen_on);
                let on = if action == "toggle_screen" {
                    !actual
                } else {
                    args.boolean("on")?.context("missing screen power state")?
                };
                state.android_device(id)?;
                self.view.screen_on = actual;
                if on != actual {
                    adb::set_power(&chosen, on)
                        .await
                        .context("Could not change the phone's screen power.")?;
                    self.view.screen_on = on;
                    if let Some(screen) = &mut self.screen {
                        screen.restore = !on;
                    }
                }
                Ok(json!({"screenOn": self.view.screen_on}))
            }
            "camera" => {
                let mirror = args.boolean("mirror")?.unwrap_or(false);
                let entered = args.text("facing")?;
                if !entered.is_empty() {
                    state.set_plugin_setting(id, PLUGIN, "facing", json!(entered))?;
                }
                let phone = state.android_device(id)?;
                let serial = serial(&phone, args.text("serial")?).await?;
                if let Some(camera) = self.camera.take() {
                    self.view.preview = false;
                    adb::stop(camera.child).await;
                    tokio::time::sleep(CAMERA_RELEASE).await;
                }
                let facing = phone.options.text("facing").to_owned();
                let (sensor, rotation) =
                    tokio::join!(adb::sensor(&serial, &facing), adb::rotation(&serial));
                let rotation = rotation.unwrap_or_default();
                let angle =
                    orientation(&facing, sensor, rotation, phone.options.number("rotation"));
                state.android_device(id)?;
                let child = match camera_child(&phone, &serial, &facing, angle, mirror).await {
                    Ok(child) => child,
                    Err(_) => {
                        tokio::time::sleep(CAMERA_RELEASE).await;
                        state.android_device(id)?;
                        camera_child(&phone, &serial, &facing, angle, mirror).await?
                    }
                };
                if let Err(error) = state.android_device(id) {
                    adb::stop(child).await;
                    return Err(error);
                }
                self.camera = Some(Camera {
                    serial,
                    child,
                    facing,
                    mirror,
                    sensor,
                    rotation,
                    angle,
                    check_at: Instant::now(),
                });
                self.view.preview = true;
                Ok(json!({}))
            }
            "stop_camera" => {
                self.stop_preview().await;
                Ok(Value::Null)
            }
            _ => bail!("unknown Android feature action"),
        }
    }

    async fn tick(&mut self, phone: &Device) {
        if let Some(screen) = &mut self.screen {
            if !matches!(screen.child.try_wait(), Ok(None)) {
                self.stop_screen().await;
            } else if Instant::now() >= screen.check_at {
                if let Some(on) = adb::power(&screen.serial).await {
                    if on != self.view.screen_on {
                        screen.restore = !on;
                    }
                    self.view.screen_on = on;
                }
                screen.check_at = Instant::now() + Duration::from_secs(5);
            }
        }
        let Some(mut camera) = self.camera.take() else {
            return;
        };
        if !matches!(camera.child.try_wait(), Ok(None)) {
            adb::stop(camera.child).await;
            self.view.preview = false;
            return;
        }
        if Instant::now() >= camera.check_at {
            let facing = phone.options.text("facing");
            let facing_changed = facing != camera.facing;
            if facing_changed {
                camera.sensor = adb::sensor(&camera.serial, facing).await;
            }
            camera.rotation = adb::rotation(&camera.serial)
                .await
                .unwrap_or(camera.rotation);
            let angle = orientation(
                facing,
                camera.sensor,
                camera.rotation,
                phone.options.number("rotation"),
            );
            if facing_changed || angle != camera.angle {
                adb::stop(camera.child).await;
                tokio::time::sleep(CAMERA_RELEASE).await;
                match camera_child(phone, &camera.serial, facing, angle, camera.mirror).await {
                    Ok(next) => camera.child = next,
                    Err(_) => {
                        self.view.preview = false;
                        return;
                    }
                }
                camera.facing = facing.to_owned();
                camera.angle = angle;
            }
            camera.check_at = Instant::now() + Duration::from_secs(2);
        }
        self.camera = Some(camera);
    }

    async fn stop_screen(&mut self) {
        if let Some(screen) = self.screen.take() {
            adb::stop(screen.child).await;
            if screen.restore && adb::set_power(&screen.serial, true).await.is_ok() {
                self.view.screen_on = true;
            }
        }
        self.view.mirroring = false;
    }

    async fn stop_preview(&mut self) {
        if let Some(camera) = self.camera.take() {
            adb::stop(camera.child).await;
        }
        self.view.preview = false;
    }

    async fn stop(&mut self) {
        self.stop_screen().await;
        self.stop_preview().await;
    }
}
