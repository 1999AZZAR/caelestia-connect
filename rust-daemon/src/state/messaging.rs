use super::{Links, Peer, State};
use crate::{
    messaging as messages,
    wire::{self, Packet},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

impl Peer {
    pub(super) fn supports_messaging(&self, plugin: &str) -> bool {
        match plugin {
            messages::SMS => {
                self.sends(messages::SMS_MESSAGES) && self.accepts(messages::SMS_CONVERSATIONS)
            }
            messages::CONTACTS => {
                self.sends(messages::CONTACT_UIDS) && self.accepts(messages::CONTACT_SYNC)
            }
            _ => false,
        }
    }
}

impl Links {
    fn messaging_allowed(&self, id: &str, serial: u64, plugin: &str) -> bool {
        !self.blocked.contains(id)
            && self
                .trust
                .get(id)
                .is_some_and(|trusted| trusted.enabled(plugin))
            && self
                .active
                .get(id)
                .is_some_and(|link| link.serial == serial && link.peer.supports_messaging(plugin))
    }
}

impl State {
    pub fn network_frame_limit(&self, id: &str, serial: u64) -> usize {
        let links = self.links.lock().unwrap();
        if links.messaging_allowed(id, serial, messages::SMS)
            || links.messaging_allowed(id, serial, messages::CONTACTS)
        {
            messages::FRAME_LIMIT
        } else {
            wire::FRAME_LIMIT
        }
    }

    pub fn messaging_enabled(&self, id: &str, serial: u64, plugin: &str) -> bool {
        self.links
            .lock()
            .unwrap()
            .messaging_allowed(id, serial, plugin)
    }

    pub(super) async fn contact_name(&self, id: &str, serial: u64, number: &str) -> Option<String> {
        if !self.messaging_enabled(id, serial, messages::CONTACTS) {
            return None;
        }
        let (response, result) = oneshot::channel();
        self.messaging
            .try_send(messages::Request::ContactName {
                id: id.to_owned(),
                serial,
                number: number.to_owned(),
                response,
            })
            .ok()?;
        tokio::time::timeout(std::time::Duration::from_millis(250), result)
            .await
            .ok()?
            .ok()?
    }

    pub(super) fn start_messaging(&self, id: &str) {
        self.refresh_message_cache(id, true);
    }

    pub(super) fn refresh_message_cache(&self, id: &str, sync: bool) {
        let links = self.links.lock().unwrap();
        let Some(link) = links.active.get(id) else {
            return;
        };
        let mut supported = false;
        for (plugin, discovery) in [
            (messages::SMS, messages::SMS_CONVERSATIONS),
            (messages::CONTACTS, messages::CONTACT_SYNC),
        ] {
            if links.messaging_allowed(id, link.serial, plugin) {
                supported = true;
                if sync {
                    link.queue(Packet::new(discovery, json!({})));
                }
            }
        }
        if supported {
            let _ = self.messaging.try_send(messages::Request::Load {
                id: id.to_owned(),
                serial: link.serial,
            });
        }
    }

    pub fn receive_messaging(&self, id: &str, serial: u64, packet: Packet) {
        let Some(plugin) = messages::incoming_plugin(&packet.kind) else {
            return;
        };
        let links = self.links.lock().unwrap();
        if !links.messaging_allowed(id, serial, plugin)
            || !links
                .active
                .get(id)
                .is_some_and(|link| link.peer.sends(&packet.kind))
        {
            return;
        }
        let _ = self.messaging.try_send(messages::Request::Incoming {
            id: id.to_owned(),
            serial,
            packet,
        });
    }

    pub fn queue_messaging(&self, id: &str, serial: u64, packet: Packet) -> Result<()> {
        let plugin = messages::outgoing_plugin(&packet.kind).context("invalid messaging packet")?;
        let links = self.links.lock().unwrap();
        ensure!(
            links.messaging_allowed(id, serial, plugin),
            "feature unavailable for this device"
        );
        let link = links.active.get(id).context("device is disconnected")?;
        ensure!(
            link.peer.accepts(&packet.kind),
            "device does not support this request"
        );
        ensure!(link.queue(packet), "device is busy or disconnected");
        Ok(())
    }

    pub async fn messaging_action(
        &self,
        id: &str,
        plugin: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value> {
        let serial = {
            let links = self.links.lock().unwrap();
            let link = links.active.get(id).context("device is disconnected")?;
            ensure!(
                links.messaging_allowed(id, link.serial, plugin),
                "feature unavailable for this device"
            );
            link.serial
        };
        let (response, result) = oneshot::channel();
        self.messaging
            .try_send(messages::Request::Action {
                id: id.to_owned(),
                serial,
                plugin: plugin.to_owned(),
                action: action.to_owned(),
                args: args.cloned().unwrap_or_default(),
                response,
            })
            .context("messaging is busy or unavailable")?;
        tokio::time::timeout(std::time::Duration::from_secs(12), result)
            .await
            .context("messaging request timed out")?
            .context("messaging stopped")?
    }

    pub fn set_messaging_status(&self, id: &str, serial: u64, sms: Value, contacts: Value) {
        let mut links = self.links.lock().unwrap();
        let mut changed = false;
        for (plugin, status) in [(messages::SMS, sms), (messages::CONTACTS, contacts)] {
            if links.messaging_allowed(id, serial, plugin) {
                if let Some(link) = links.active.get_mut(id) {
                    if link.status.get(plugin) != Some(&status) {
                        link.status.insert(plugin, status);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            drop(links);
            self.changed();
        }
    }

    pub fn messaging_payload_peer(
        &self,
        id: &str,
        serial: u64,
    ) -> Option<(std::net::Ipv4Addr, Vec<u8>)> {
        let links = self.links.lock().unwrap();
        if !links.messaging_allowed(id, serial, messages::SMS) {
            return None;
        }
        let link = links.active.get(id)?;
        let trusted = links.trust.get(id)?;
        Some((link.peer.address, trusted.certificate.clone()))
    }

    pub fn messaging_attachment_finished(&self, id: &str, serial: u64) {
        let _ = self.messaging.try_send(messages::Request::AttachmentDone {
            id: id.to_owned(),
            serial,
        });
    }
}
