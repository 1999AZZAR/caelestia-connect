use super::{Link, Links, State};
use crate::{
    notifications::{Popup, PopupKey},
    wire::Packet,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};

pub const PLUGIN: &str = "telephony";
pub const EVENT: &str = "kdeconnect.telephony";
pub const MUTE: &str = "kdeconnect.telephony.request_mute";

impl Links {
    fn call_link(&mut self, id: &str, serial: Option<u64>) -> Option<&mut Link> {
        if self.blocked.contains(id) || !self.trust.get(id)?.enabled(PLUGIN) {
            return None;
        }
        self.active.get_mut(id).filter(|link| {
            serial.is_none_or(|serial| link.serial == serial) && link.peer.sends(EVENT)
        })
    }
}

impl State {
    pub fn receive_call(&self, id: &str, serial: u64, body: &Value) {
        let mut links = self.links.lock().unwrap();
        let Some(link) = links.call_link(id, Some(serial)) else {
            return;
        };
        if body.get("isCancel").and_then(Value::as_bool) == Some(true) {
            if link.status.remove(PLUGIN).is_some() {
                self.changed();
            }
            return;
        }
        let event = body
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(event, "ringing" | "talking" | "missedCall") {
            return;
        }
        let text = |key| {
            body.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .filter(|ch| !ch.is_control())
                .take(128)
                .collect::<String>()
        };
        let mut number = text("phoneNumber");
        let mut contact = text("contactName");
        if event == "talking" {
            if let Some(previous) = link.status.get(PLUGIN).and_then(|state| state.get("call")) {
                if number.is_empty() {
                    number = previous["number"].as_str().unwrap_or_default().to_owned();
                }
                if contact.is_empty() {
                    contact = previous["contact"].as_str().unwrap_or_default().to_owned();
                }
            }
        }
        if number.is_empty() {
            number = "unknown number".into();
        }
        if contact.is_empty() {
            contact.clone_from(&number);
        }
        let status =
            json!({"call": {"event": event, "number": number, "contact": contact, "photo": ""}});
        if link.status.get(PLUGIN) == Some(&status) {
            return;
        }
        link.status.insert(PLUGIN, status);
        let key = match event {
            "ringing" => Some(PopupKey::Call {
                device: id.to_owned(),
                serial,
            }),
            "missedCall" => Some(PopupKey::MissedCall {
                device: id.to_owned(),
                serial,
            }),
            _ => None,
        };
        if let Some(key) = key {
            let _ = self.popups.try_send(Popup::Call {
                key,
                title: link.peer.name.clone(),
                contact,
            });
        }
        self.changed();
    }

    pub fn call_event(&self, id: &str, serial: u64, event: &str) -> bool {
        self.links
            .lock()
            .unwrap()
            .call_link(id, Some(serial))
            .is_some_and(|link| {
                link.status
                    .get(PLUGIN)
                    .and_then(|state| state.get("call"))
                    .and_then(|call| call.get("event"))
                    .and_then(Value::as_str)
                    == Some(event)
            })
    }

    pub fn mute_call(&self, id: &str, serial: Option<u64>) -> Result<bool> {
        let mut links = self.links.lock().unwrap();
        let link = links
            .call_link(id, serial)
            .context("call control unavailable")?;
        if serial.is_some()
            && link
                .status
                .get(PLUGIN)
                .and_then(|state| state["call"]["event"].as_str())
                != Some("ringing")
        {
            return Ok(false);
        }
        Ok(link.peer.accepts(MUTE) && link.queue(Packet::new(MUTE, json!({"action": "mute"}))))
    }
}
