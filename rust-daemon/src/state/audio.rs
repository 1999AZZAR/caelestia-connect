use super::{Links, Peer, State};
use crate::{audio, wire::Packet};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};

impl Peer {
    pub(super) fn supports_volume(&self, plugin: &str) -> bool {
        match plugin {
            audio::LOCAL => self.sends(audio::REQUEST) && self.accepts(audio::INFO),
            audio::REMOTE => self.sends(audio::INFO) && self.accepts(audio::REQUEST),
            _ => false,
        }
    }
}

impl Links {
    fn volume_allowed(&self, id: &str, serial: u64, plugin: &str) -> bool {
        !self.blocked.contains(id)
            && self
                .trust
                .get(id)
                .is_some_and(|entry| entry.enabled(plugin))
            && self
                .active
                .get(id)
                .is_some_and(|link| link.serial == serial && link.peer.supports_volume(plugin))
    }
}

impl State {
    pub(super) fn request_remote_sinks(&self, id: &str) {
        let links = self.links.lock().unwrap();
        if let Some(link) = links
            .active
            .get(id)
            .filter(|link| links.volume_allowed(id, link.serial, audio::REMOTE))
        {
            link.queue(Packet::new(audio::REQUEST, json!({"requestSinks": true})));
        }
    }

    pub fn receive_remote_sinks(&self, id: &str, serial: u64, body: Value) {
        let mut links = self.links.lock().unwrap();
        if !links.volume_allowed(id, serial, audio::REMOTE) {
            return;
        }
        let link = links.active.get_mut(id).expect("active volume link");
        if audio::update_remote(&mut link.remote_sinks, body) {
            self.changed();
        }
    }

    pub fn remote_volume_action(
        &self,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<bool> {
        let args = args.context("missing volume arguments")?;
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .context("missing sink name")?;
        let links = self.links.lock().unwrap();
        let link = links
            .active
            .get(id)
            .filter(|link| links.volume_allowed(id, link.serial, audio::REMOTE))
            .context("remote volume is unavailable")?;
        let sink = link.remote_sinks.get(name).context("unknown output")?;
        let body = match action {
            "set_volume" => {
                let volume = args
                    .get("volume")
                    .and_then(Value::as_u64)
                    .context("invalid volume")?;
                ensure!(
                    volume <= u64::from(sink.max_volume),
                    "volume exceeds the output's maximum"
                );
                json!({"name": name, "volume": volume})
            }
            "set_muted" => {
                let muted = args
                    .get("muted")
                    .and_then(Value::as_bool)
                    .context("invalid mute state")?;
                json!({"name": name, "muted": muted})
            }
            _ => bail!("unknown remote volume action"),
        };
        Ok(link.queue(Packet::new(audio::REQUEST, body)))
    }

    pub fn local_volume_targets(&self) -> audio::Targets {
        let links = self.links.lock().unwrap();
        links
            .active
            .iter()
            .filter(|(id, link)| links.volume_allowed(id, link.serial, audio::LOCAL))
            .map(|(id, link)| (id.clone(), link.serial))
            .collect()
    }

    pub fn local_volume_allowed(&self, id: &str, serial: u64) -> bool {
        self.links
            .lock()
            .unwrap()
            .volume_allowed(id, serial, audio::LOCAL)
    }

    pub fn receive_volume_request(&self, id: &str, serial: u64, body: Value) {
        if self.config.system_volume && self.local_volume_allowed(id, serial) {
            let _ = self.audio.try_send(audio::Request {
                device: id.to_owned(),
                serial,
                body,
            });
        }
    }

    pub fn send_volume(&self, id: &str, serial: u64, body: Value) -> bool {
        let links = self.links.lock().unwrap();
        links.volume_allowed(id, serial, audio::LOCAL)
            && links.active[id].queue(Packet::new(audio::INFO, body))
    }
}
