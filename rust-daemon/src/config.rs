use crate::{
    clipboard::{Clipboard, TEXT},
    desktop::{BATTERY, CONNECTIVITY, RING},
    media,
    wire::{self, PING, SHARE, VERSION},
};
use anyhow::{Context, Result, ensure};
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rand::rand_bytes,
    ssl::{SslContext, SslContextBuilder, SslMethod, SslOptions, SslSessionCacheMode, SslVersion},
    x509::{X509, X509NameBuilder, X509Ref},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{ErrorKind, Write},
    net::Ipv4Addr,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Clone, Deserialize, serde::Serialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    name: String,
    custom_devices: Vec<String>,
    incoming_path: String,
    #[serde(skip)]
    runtime_name: Option<String>,
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustRecord {
    certificate: String,
    name: Option<String>,
    #[serde(rename = "type")]
    device_type: Option<String>,
    protocol_version: Option<u32>,
    last_address: Option<String>,
    last_port: Option<u16>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeviceSettings {
    plugins: BTreeMap<String, bool>,
    #[serde(rename = "pluginSettings")]
    plugin_settings: BTreeMap<String, Value>,
}

pub struct Trusted {
    record: Value,
    pub certificate: Vec<u8>,
    pub fingerprint: String,
    pub name: String,
    pub device_type: String,
    pub version: u32,
    pub address: Option<Ipv4Addr>,
    pub port: u16,
    plugins: BTreeMap<String, bool>,
    pub share_directory: String,
    pub share_open_urls: bool,
    pub media_hide_idle: bool,
    pub media_export: bool,
    pub phone_audio_address: String,
    pub notification_popups: bool,
    pub notification_copy_codes: bool,
    pub pause_music: crate::state::telephony::PauseSettings,
    pub local_commands: crate::commands::Definitions,
}

impl Trusted {
    pub fn enabled(&self, plugin: &str) -> bool {
        self.plugins
            .get(plugin)
            .copied()
            .unwrap_or(plugin != crate::idle::PLUGIN)
    }
}

pub struct Config {
    pub id: String,
    pub fingerprint: String,
    pub public_key: Vec<u8>,
    pub clipboard: Option<Clipboard>,
    pub system_volume: bool,
    directory: PathBuf,
    default_name: String,
    settings: Mutex<Settings>,
    downloads: PathBuf,
    pub tls: SslContext,
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(home) = env::var_os("HOME") {
        if path == "~" {
            return PathBuf::from(home);
        }
        if let Some(relative) = path.strip_prefix("~/") {
            return PathBuf::from(home).join(relative);
        }
    }
    PathBuf::from(path)
}

fn default_downloads() -> PathBuf {
    let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    std::process::Command::new("xdg-user-dir")
        .arg("DOWNLOAD")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| PathBuf::from(value.trim()))
        .filter(|dir| dir.is_absolute() && *dir != home)
        .unwrap_or_else(|| home.join("Downloads"))
}

pub fn xdg_path(variable: &str, home_default: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable).filter(|value| !value.is_empty()) {
        ensure!(
            Path::new(&path).is_absolute(),
            "{variable} must be absolute"
        );
        return Ok(path.into());
    }
    Ok(PathBuf::from(env::var_os("HOME").context("HOME is unset")?).join(home_default))
}

fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(data) => {
            serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

pub fn save_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let parent = path.parent().context("missing configuration directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

pub fn certificate_id(cert: &X509Ref) -> Result<String> {
    let mut names = cert.subject_name().entries_by_nid(Nid::COMMONNAME);
    let name = names
        .next()
        .context("certificate has no common name")?
        .data()
        .to_string()?;
    ensure!(
        names.next().is_none() && wire::valid_id(&name),
        "invalid certificate device ID"
    );
    Ok(name)
}

pub fn fingerprint(cert: &X509Ref) -> Result<String> {
    Ok(cert
        .digest(MessageDigest::sha256())?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":"))
}

fn read_identity_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn save_new_identity_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("missing config directory")?)?;
    file.write_all(data)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn load_identity(path: &Path, has_pairings: bool) -> Result<(X509, PKey<Private>)> {
    let cert_path = path.join("certificate.pem");
    let key_path = path.join("privateKey.pem");
    let key_data = read_identity_file(&key_path)?;
    if let Some(cert_data) = read_identity_file(&cert_path)? {
        let key_data =
            key_data.context("privateKey.pem is missing for the existing certificate")?;
        return Ok((
            X509::from_pem(&cert_data)?,
            PKey::private_key_from_pem(&key_data)?,
        ));
    }
    ensure!(
        !has_pairings,
        "certificate.pem is missing for the existing pairings"
    );
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    require_private_config(path)?;
    let key = match &key_data {
        Some(data) => PKey::private_key_from_pem(data)?,
        None => {
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
            PKey::from_ec_key(EcKey::generate(&group)?)?
        }
    };
    let mut id_bytes = [0; 16];
    rand_bytes(&mut id_bytes)?;
    let id: String = id_bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut serial_bytes = [0; 16];
    rand_bytes(&mut serial_bytes)?;
    serial_bytes[0] |= 1;
    let serial = BigNum::from_slice(&serial_bytes)?.to_asn1_integer()?;
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_nid(Nid::COMMONNAME, &id)?;
    name.append_entry_by_nid(Nid::ORGANIZATIONNAME, "KDE")?;
    name.append_entry_by_nid(Nid::ORGANIZATIONALUNITNAME, "KDE Connect")?;
    let name = name.build();
    let mut certificate = X509::builder()?;
    certificate.set_version(2)?;
    certificate.set_serial_number(&serial)?;
    certificate.set_subject_name(&name)?;
    certificate.set_issuer_name(&name)?;
    certificate.set_pubkey(&key)?;
    let from = Asn1Time::from_unix((wire::timestamp_ms() / 1000).saturating_sub(86400) as i64)?;
    let until = Asn1Time::days_from_now(3650)?;
    certificate.set_not_before(&from)?;
    certificate.set_not_after(&until)?;
    certificate.sign(&key, MessageDigest::sha512())?;
    let certificate = certificate.build();
    if key_data.is_none() {
        save_new_identity_file(&key_path, &key.private_key_to_pem_pkcs8()?)
            .context("save privateKey.pem")?;
    }
    save_new_identity_file(&cert_path, &certificate.to_pem()?).context("save certificate.pem")?;
    Ok((certificate, key))
}

fn require_private_config(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.is_dir() && metadata.mode() & 0o077 == 0,
        "Caelestia configuration directory must be private: {}",
        path.display()
    );
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<(Self, TrustStore)> {
        let trust = TrustStore::load(path)?;
        let (certificate, key) = load_identity(path, !trust.entries.is_empty())?;
        let now = Asn1Time::days_from_now(0)?;
        ensure!(
            certificate.not_before() <= now && certificate.not_after() >= now,
            "desktop certificate expired or not yet valid"
        );
        let mut tls = SslContextBuilder::new(SslMethod::tls())?;
        tls.set_min_proto_version(Some(SslVersion::TLS1_2))?;
        tls.set_options(SslOptions::NO_TICKET | SslOptions::NO_RENEGOTIATION);
        tls.set_session_cache_mode(SslSessionCacheMode::OFF);
        tls.set_certificate(&certificate)?;
        tls.set_private_key(&key)?;
        tls.check_private_key()?;

        let settings: Settings = read_json(&path.join("settings.json"))?;
        let mut default_name =
            wire::device_name(&fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default());
        if default_name.is_empty() {
            default_name = "Linux".to_owned();
        }
        Ok((
            Self {
                id: certificate_id(&certificate)?,
                fingerprint: fingerprint(&certificate)?,
                directory: path.to_path_buf(),
                default_name,
                settings: Mutex::new(settings),
                public_key: certificate.public_key()?.public_key_to_der()?,
                tls: tls.build(),
                downloads: default_downloads(),
                clipboard: Clipboard::detect(),
                system_volume: crate::audio::available(),
            },
            trust,
        ))
    }

    pub fn identity(&self, full: bool) -> Value {
        let mut body = json!({
            "deviceId": self.id, "deviceName": self.name(), "deviceType": "desktop", "protocolVersion": VERSION,
        });
        if full {
            body["incomingCapabilities"] = json!([
                PING,
                SHARE,
                BATTERY,
                CONNECTIVITY,
                crate::audio::INFO,
                crate::commands::LIST,
                crate::commands::REQUEST,
                crate::commands::OUTPUT,
                media::INFO,
                media::REQUEST
            ]);
            body["outgoingCapabilities"] = json!([
                PING,
                SHARE,
                BATTERY,
                RING,
                media::REQUEST,
                media::INFO,
                crate::audio::REQUEST
            ]);
            body["outgoingCapabilities"].as_array_mut().unwrap().extend(
                [
                    crate::commands::LIST,
                    crate::commands::REQUEST,
                    crate::commands::OUTPUT,
                ]
                .map(|kind| json!(kind)),
            );
            if self.system_volume {
                body["incomingCapabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(crate::audio::REQUEST));
                body["outgoingCapabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(crate::audio::INFO));
            }
            body["incomingCapabilities"]
                .as_array_mut()
                .unwrap()
                .extend(crate::messaging::INCOMING.iter().map(|kind| json!(kind)));
            body["outgoingCapabilities"]
                .as_array_mut()
                .unwrap()
                .extend(crate::messaging::OUTGOING.iter().map(|kind| json!(kind)));
            body["incomingCapabilities"]
                .as_array_mut()
                .unwrap()
                .push(json!(crate::notifications::INFO));
            body["outgoingCapabilities"].as_array_mut().unwrap().extend(
                crate::notifications::OUTGOING
                    .iter()
                    .map(|kind| json!(kind)),
            );
            body["incomingCapabilities"]
                .as_array_mut()
                .unwrap()
                .push(json!(crate::state::telephony::EVENT));
            body["outgoingCapabilities"]
                .as_array_mut()
                .unwrap()
                .push(json!(crate::state::telephony::MUTE));
            if self.clipboard.is_some() {
                body["incomingCapabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(TEXT));
                body["outgoingCapabilities"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(TEXT));
            }
        }
        body
    }

    pub fn set_startup_name(&mut self, name: String, fallback: bool) {
        if fallback {
            self.default_name = name;
        } else {
            self.settings.get_mut().unwrap().runtime_name = Some(name);
        }
    }

    pub fn name(&self) -> String {
        let settings = self.settings.lock().unwrap();
        let name = wire::device_name(settings.runtime_name.as_deref().unwrap_or(&settings.name));
        if name.is_empty() {
            self.default_name.clone()
        } else {
            name
        }
    }

    pub fn set_name(&self, name: &str) -> Result<String> {
        let name = wire::device_name(name);
        ensure!(!name.is_empty(), "invalid device name");
        self.update(|settings| {
            settings.name.clone_from(&name);
            settings.runtime_name = None;
        })?;
        Ok(name)
    }

    pub fn set_incoming_path(&self, path: &str) -> Result<()> {
        validate_incoming_path(path)?;
        self.update(|settings| settings.incoming_path = path.to_owned())
    }

    pub fn set_custom_address(&self, address: Ipv4Addr, add: bool) -> Result<()> {
        let address = address.to_string();
        self.update(|settings| {
            if add {
                if !settings.custom_devices.contains(&address) {
                    settings.custom_devices.push(address);
                }
            } else {
                settings.custom_devices.retain(|entry| *entry != address);
            }
        })
    }

    fn update(&self, edit: impl FnOnce(&mut Settings)) -> Result<()> {
        require_private_config(&self.directory)?;
        let mut settings = self.settings.lock().unwrap();
        let mut next = settings.clone();
        edit(&mut next);
        save_json(&self.directory.join("settings.json"), &next).context("save settings")?;
        *settings = next;
        Ok(())
    }

    pub fn custom_addresses(&self) -> Vec<Ipv4Addr> {
        self.settings
            .lock()
            .unwrap()
            .custom_devices
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect()
    }

    pub fn incoming_path(&self, override_path: &str) -> PathBuf {
        if !override_path.is_empty() {
            return expand_home(override_path);
        }
        let settings = self.settings.lock().unwrap();
        let path = &settings.incoming_path;
        if path.is_empty() {
            return self.downloads.clone();
        }
        expand_home(path)
    }

    pub fn settings_state(&self) -> Value {
        let incoming = self.incoming_path("");
        let name = self.name();
        json!({"name": name, "customDevices": self.settings.lock().unwrap().custom_devices, "incomingPath": incoming})
    }
}

pub fn validate_incoming_path(path: &str) -> Result<()> {
    ensure!(
        path.len() <= 4096 && !path.contains('\0'),
        "invalid directory path"
    );
    ensure!(
        path.is_empty() || expand_home(path).is_absolute(),
        "use an absolute directory path"
    );
    Ok(())
}

pub struct TrustStore {
    directory: PathBuf,
    entries: BTreeMap<String, Trusted>,
}

impl TrustStore {
    fn load(directory: &Path) -> Result<Self> {
        let mut store = Self {
            directory: directory.to_path_buf(),
            entries: BTreeMap::new(),
        };
        for (id, record) in
            read_json::<BTreeMap<String, Value>>(&directory.join("trusted_devices.json"))?
        {
            store
                .entries
                .insert(id.clone(), store.parse_record(&id, record)?);
        }
        Ok(store)
    }

    fn parse_record(&self, id: &str, record: Value) -> Result<Trusted> {
        let fields: TrustRecord = serde_json::from_value(record.clone())?;
        let cert = X509::from_pem(fields.certificate.as_bytes())
            .with_context(|| format!("parse trusted certificate for {id}"))?;
        ensure!(
            certificate_id(&cert)? == id,
            "trusted certificate common name does not match its device ID"
        );
        let device: DeviceSettings =
            read_json(&self.directory.join("devices").join(id).join("config.json"))?;
        Ok(Trusted {
            record,
            local_commands: crate::commands::load(
                device
                    .plugin_settings
                    .get(crate::commands::LOCAL)
                    .and_then(|settings| settings.get("commands")),
            ),
            pause_music: crate::state::telephony::PauseSettings::read(
                device.plugin_settings.get(crate::state::telephony::PAUSE),
            ),
            certificate: cert.to_der()?,
            fingerprint: fingerprint(&cert)?,
            name: fields
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| id.to_owned()),
            device_type: fields.device_type.unwrap_or_else(|| "unknown".to_owned()),
            version: fields.protocol_version.unwrap_or(VERSION),
            address: fields.last_address.and_then(|address| address.parse().ok()),
            port: fields
                .last_port
                .filter(|port| wire::PORTS.contains(port))
                .unwrap_or(1716),
            plugins: device.plugins,
            share_directory: device
                .plugin_settings
                .get("share")
                .and_then(|s| s.get("incomingPath"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            share_open_urls: device
                .plugin_settings
                .get("share")
                .and_then(|s| s.get("openUrls"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            media_hide_idle: device
                .plugin_settings
                .get(media::PLUGIN)
                .and_then(|settings| settings.get("hideIdle"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            media_export: device
                .plugin_settings
                .get(media::PLUGIN)
                .and_then(|settings| settings.get("exportMpris"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            notification_popups: device
                .plugin_settings
                .get(crate::notifications::PLUGIN)
                .and_then(|settings| settings.get("showPopups"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            notification_copy_codes: device
                .plugin_settings
                .get(crate::notifications::PLUGIN)
                .and_then(|settings| settings.get("copyAuthCodes"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            phone_audio_address: device
                .plugin_settings
                .get(crate::phone_audio::PLUGIN)
                .and_then(|settings| settings.get("address"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }

    pub fn get(&self, id: &str) -> Option<&Trusted> {
        self.entries.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Trusted)> {
        self.entries.iter()
    }

    pub fn plugin_setting(
        &mut self,
        id: &str,
        plugin: &str,
        key: &str,
        value: Value,
    ) -> Result<()> {
        self.update_device(id, |settings| {
            let plugin_settings = settings
                .entry("pluginSettings")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("invalid plugin settings")?;
            plugin_settings
                .entry(plugin)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("invalid feature settings")?
                .insert(key.to_owned(), value.clone());
            Ok(())
        })?;
        let entry = self.entries.get_mut(id).context("device is not paired")?;
        match (plugin, key) {
            ("share", "incomingPath") => {
                entry.share_directory = value.as_str().unwrap_or_default().to_owned()
            }
            ("share", "openUrls") => entry.share_open_urls = value.as_bool().unwrap_or(false),
            (media::PLUGIN, "hideIdle") => entry.media_hide_idle = value.as_bool().unwrap_or(true),
            (media::PLUGIN, "exportMpris") => entry.media_export = value.as_bool().unwrap_or(true),
            (crate::notifications::PLUGIN, "showPopups") => {
                entry.notification_popups = value.as_bool().unwrap_or(true)
            }
            (crate::notifications::PLUGIN, "copyAuthCodes") => {
                entry.notification_copy_codes = value.as_bool().unwrap_or(true)
            }
            (crate::state::telephony::PAUSE, key) => entry
                .pause_music
                .update(key, value.as_bool().unwrap_or_default()),
            (crate::commands::LOCAL, "commands") => {
                entry.local_commands = crate::commands::load(Some(&value))
            }
            _ => unreachable!("unsupported feature setting"),
        }
        Ok(())
    }

    pub fn set_enabled(&mut self, id: &str, plugin: &str, enabled: bool) -> Result<()> {
        self.update_device(id, |settings| {
            settings
                .entry("plugins")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("invalid plugin preferences")?
                .insert(plugin.to_owned(), json!(enabled));
            Ok(())
        })?;
        self.entries
            .get_mut(id)
            .context("device is not paired")?
            .plugins
            .insert(plugin.to_owned(), enabled);
        Ok(())
    }

    fn update_device(
        &self,
        id: &str,
        edit: impl FnOnce(&mut serde_json::Map<String, Value>) -> Result<()>,
    ) -> Result<()> {
        ensure!(self.entries.contains_key(id), "device is not paired");
        require_private_config(&self.directory)?;
        let directory = self.directory.join("devices").join(id);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let path = directory.join("config.json");
        let mut settings: Value = read_json(&path)?;
        if settings.is_null() {
            settings = json!({});
        }
        edit(
            settings
                .as_object_mut()
                .context("invalid device configuration")?,
        )?;
        save_json(&path, &settings).context("save device preferences")
    }

    pub fn trust(&mut self, id: &str, record: Value) -> Result<()> {
        let entry = self.parse_record(id, record)?;
        self.save(id, Some(&entry.record))?;
        self.entries.insert(id.to_owned(), entry);
        Ok(())
    }

    pub fn forget(&mut self, id: &str) -> Result<bool> {
        if !self.entries.contains_key(id) {
            return Ok(false);
        }
        self.save(id, None)?;
        self.entries.remove(id);
        Ok(true)
    }

    fn save(&self, id: &str, record: Option<&Value>) -> Result<()> {
        require_private_config(&self.directory)?;
        let mut records: BTreeMap<&str, &Value> = self
            .entries
            .iter()
            .filter(|(key, _)| key.as_str() != id)
            .map(|(key, entry)| (key.as_str(), &entry.record))
            .collect();
        if let Some(record) = record {
            records.insert(id, record);
        }
        let mut file =
            tempfile::NamedTempFile::new_in(&self.directory).context("create pairing store")?;
        serde_json::to_writer_pretty(&mut file, &records)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("trusted_devices.json"))
            .map_err(|error| error.error)
            .context("replace pairing store")?;
        Ok(())
    }
}
