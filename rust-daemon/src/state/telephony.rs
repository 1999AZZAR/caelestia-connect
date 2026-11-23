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
pub const PAUSE: &str = "pausemusic";

fn call_text(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(128)
        .collect()
}

const PAUSE_OPTIONS: [(&str, &str, bool); 4] = [
    (
        "conditionTalking",
        "Only when talking (not while ringing)",
        false,
    ),
    ("actionPause", "Pause media players", true),
    ("actionMute", "Mute system sound", false),
    (
        "actionResume",
        "Resume automatically when the call ends",
        true,
    ),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    Ringing,
    Talking,
}

#[derive(Clone, Copy)]
pub struct PauseSettings([bool; 4]);

impl PauseSettings {
    pub fn read(saved: Option<&Value>) -> Self {
        Self(PAUSE_OPTIONS.map(|(key, _, default)| {
            saved
                .and_then(|entry| entry[key].as_bool())
                .unwrap_or(default)
        }))
    }

    pub fn update(&mut self, key: &str, value: bool) {
        if let Some(index) = PAUSE_OPTIONS.iter().position(|(name, _, _)| key == *name) {
            self.0[index] = value;
        }
    }

    pub(super) fn schema(self, enabled: bool) -> Value {
        let definitions: Vec<_> = PAUSE_OPTIONS.map(|(key, label, default)| {
            json!({"key": key, "type": "bool", "label": label, "default": default})
        }).into();
        let values = PAUSE_OPTIONS
            .into_iter()
            .zip(self.0)
            .map(|((key, _, _), value)| (key.to_owned(), json!(value)))
            .collect::<serde_json::Map<_, _>>();
        json!({"id": PAUSE, "name": "Pause media during calls", "description": "Pause music/videos during a phone call",
            "enabled": enabled, "loaded": enabled, "settings": definitions, "values": values})
    }
}

#[derive(Default)]
pub struct CallAudio {
    pub pause: Option<bool>,
    pub mute: Option<bool>,
}

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
    pub fn call_audio(&self) -> CallAudio {
        let links = self.links.lock().unwrap();
        let mut audio = CallAudio::default();
        for (id, link) in &links.active {
            let Some(activity) = link.call_activity else {
                continue;
            };
            let Some(entry) = links
                .trust
                .get(id)
                .filter(|entry| entry.enabled(PAUSE) && !links.blocked.contains(id))
            else {
                continue;
            };
            let [only_talking, pause, mute, resume] = entry.pause_music.0;
            if only_talking && activity != Activity::Talking {
                continue;
            }
            if pause {
                audio.pause = Some(audio.pause.unwrap_or(true) && resume);
            }
            if mute {
                audio.mute = Some(audio.mute.unwrap_or(true) && resume);
            }
        }
        audio
    }

    pub async fn receive_call(&self, id: &str, serial: u64, body: &Value) {
        let event = body["event"].as_str().unwrap_or_default();
        let canceled = body["isCancel"].as_bool() == Some(true);
        let mut number = call_text(body["phoneNumber"].as_str().unwrap_or_default());
        let mut contact = call_text(body["contactName"].as_str().unwrap_or_default());
        let lookup = matches!(event, "ringing" | "missedCall")
            && !canceled
            && !number.is_empty()
            && contact.is_empty()
            && self
                .links
                .lock()
                .unwrap()
                .call_link(id, Some(serial))
                .is_some();
        if lookup && let Some(name) = self.contact_name(id, serial, &number).await {
            contact = call_text(&name);
        }

        let mut links = self.links.lock().unwrap();
        let Some(trusted) = links.trust.get(id).filter(|_| !links.blocked.contains(id)) else {
            return;
        };
        let telephony = trusted.enabled(PLUGIN);
        let pause = trusted.enabled(PAUSE);
        let Some(link) = links
            .active
            .get_mut(id)
            .filter(|link| link.serial == serial && link.peer.sends(EVENT))
        else {
            return;
        };
        if pause {
            let activity = match event {
                "ringing" => Some(Activity::Ringing),
                "talking" => Some(Activity::Talking),
                _ => None,
            };
            let ended = canceled && (activity.is_none() || activity == link.call_activity)
                || event == "missedCall" && link.call_activity == Some(Activity::Ringing);
            let next = if ended {
                None
            } else if !canceled && activity.is_some() {
                activity
            } else {
                link.call_activity
            };
            if next != link.call_activity {
                link.call_activity = next;
                self.changed();
            }
        }
        if !telephony {
            return;
        }
        if canceled {
            if link.status.remove(PLUGIN).is_some() {
                self.changed();
            }
            return;
        }
        if !matches!(event, "ringing" | "talking" | "missedCall") {
            return;
        }
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
