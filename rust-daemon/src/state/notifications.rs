use super::{Link, Links, Peer, State};
use crate::{
    config::Trusted,
    notifications::{self, Key, PLUGIN, Popup, PopupKey},
    wire::Packet,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration};
use tokio::runtime::Handle;

impl Peer {
    pub(super) fn supports_notification_sync(&self) -> bool {
        self.sends(notifications::INFO) && self.accepts(notifications::REQUEST)
    }
}

impl Links {
    fn notification_link(
        &mut self,
        id: &str,
        serial: Option<u64>,
    ) -> Option<(&mut Link, &Trusted)> {
        let trusted = self
            .trust
            .get(id)
            .filter(|entry| !self.blocked.contains(id) && entry.enabled(PLUGIN))?;
        let link = self.active.get_mut(id).filter(|link| {
            serial.is_none_or(|serial| serial == link.serial)
                && link.peer.supports_notification_sync()
        })?;
        Some((link, trusted))
    }
}

fn request_sync(link: &mut Link) -> bool {
    if !link.queue(Packet::new(
        notifications::REQUEST,
        json!({"request": true}),
    )) {
        return false;
    }
    link.notifications.begin_sync();
    true
}

impl State {
    pub(super) fn start_notifications(&self, id: &str) {
        let mut links = self.links.lock().unwrap();
        if let Some((link, _)) = links.notification_link(id, None) {
            request_sync(link);
        }
    }

    pub async fn resync_notifications(&self) {
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        loop {
            timer.tick().await;
            let mut links = self.links.lock().unwrap();
            let eligible: Vec<_> = links
                .active
                .keys()
                .filter(|id| {
                    links
                        .trust
                        .get(id)
                        .is_some_and(|entry| entry.enabled(PLUGIN))
                        && !links.blocked.contains(*id)
                })
                .cloned()
                .collect();
            let mut changed = false;
            for id in eligible {
                if let Some((link, _)) = links.notification_link(&id, None) {
                    changed |= link.notifications.expire();
                    if link.notifications.needs_sync() {
                        request_sync(link);
                    }
                }
            }
            if changed {
                drop(links);
                self.changed();
            }
        }
    }

    pub fn receive_notification(self: &Arc<Self>, id: &str, serial: u64, packet: Packet) {
        let mut links = self.links.lock().unwrap();
        let Some((link, trusted)) = links.notification_link(id, Some(serial)) else {
            return;
        };
        let (changed, item) = link.notifications.receive(&packet.body);
        let mut icon = None;
        if let Some(item) = item {
            let key = Key {
                device: id.to_owned(),
                serial,
                id: item.id.clone(),
            };
            if item.icon.is_none()
                && !item.payload_hash.is_empty()
                && packet.payload_transfer_info.is_some()
            {
                icon = Some((key.clone(), item.payload_hash.clone()));
            }
            if changed
                && !item.silent
                && trusted.notification_popups
                && (item.dismissable || !link.media.matches_app(&item.app))
            {
                let _ = self.popups.try_send(Popup::Phone {
                    key,
                    item: Box::new(item),
                    source: link.peer.name.clone(),
                });
            }
        }
        drop(links);
        if changed {
            self.changed();
        }
        if let Some((key, hash)) = icon {
            notifications::download_icon(self, key, hash, &packet);
        }
    }

    pub fn notification_payload_peer(&self, key: &Key) -> Option<(Ipv4Addr, Vec<u8>)> {
        let mut links = self.links.lock().unwrap();
        let (link, trusted) = links.notification_link(&key.device, Some(key.serial))?;
        link.notifications
            .items
            .contains_key(&key.id)
            .then(|| (link.peer.address, trusted.certificate.clone()))
    }

    pub fn set_notification_icon(&self, key: &Key, hash: &str, path: PathBuf) {
        let mut links = self.links.lock().unwrap();
        let Some((link, trusted)) = links.notification_link(&key.device, Some(key.serial)) else {
            return;
        };
        let Some(item) = link
            .notifications
            .items
            .get_mut(&key.id)
            .filter(|item| item.payload_hash == hash && item.icon.as_ref() != Some(&path))
        else {
            return;
        };
        item.icon = Some(path);
        if trusted.notification_popups
            && !item.silent
            && (item.dismissable || !link.media.matches_app(&item.app))
        {
            let _ = self.popups.try_send(Popup::Phone {
                key: key.clone(),
                item: Box::new(item.clone()),
                source: link.peer.name.clone(),
            });
        }
        drop(links);
        self.changed();
    }

    pub fn desktop_popup_exists(&self, key: &PopupKey) -> bool {
        match key {
            PopupKey::Phone(key) => self.notification_popup_exists(key),
            PopupKey::Call { device, serial } => self.call_event(device, *serial, "ringing"),
            PopupKey::MissedCall { device, serial } => {
                self.call_event(device, *serial, "missedCall")
            }
        }
    }

    fn notification_popup_exists(&self, key: &Key) -> bool {
        let mut links = self.links.lock().unwrap();
        links
            .notification_link(&key.device, Some(key.serial))
            .is_some_and(|(link, trusted)| {
                trusted.notification_popups
                    && link.notifications.items.get(&key.id).is_some_and(|item| {
                        !item.silent && (item.dismissable || !link.media.matches_app(&item.app))
                    })
            })
    }

    pub fn notification_action(
        &self,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<bool> {
        self.notification_action_on(id, None, action, args)
    }

    fn notification_action_on(
        &self,
        id: &str,
        serial: Option<u64>,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<bool> {
        let mut links = self.links.lock().unwrap();
        let (link, trusted) = links
            .notification_link(id, serial)
            .context("notification sync unavailable for this device")?;
        let argument = |key| {
            args.and_then(|args| args.get(key))
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let notif_id = argument("id");
        let changed = match action {
            "refresh" => return Ok(request_sync(link)),
            "reply" => {
                let message = argument("message");
                ensure!(
                    !message.trim().is_empty() && message.len() <= 4096,
                    "invalid reply"
                );
                let Some(item) = link
                    .notifications
                    .items
                    .get(notif_id)
                    .filter(|item| !item.reply_id.is_empty())
                else {
                    return Ok(false);
                };
                return Ok(link.peer.accepts(notifications::REPLY)
                    && link.queue(Packet::new(
                        notifications::REPLY,
                        json!({"requestReplyId": item.reply_id, "message": message}),
                    )));
            }
            "invoke_action" => {
                let title = argument("action");
                let allowed = link
                    .notifications
                    .items
                    .get(notif_id)
                    .is_some_and(|item| item.actions.iter().any(|action| action == title));
                if !allowed
                    || !link.peer.accepts(notifications::ACTION)
                    || !link.queue(Packet::new(
                        notifications::ACTION,
                        json!({"key": notif_id, "action": title}),
                    ))
                {
                    return Ok(false);
                }
                if trusted.notification_copy_codes {
                    if let Some(code) = notifications::verification_code(title) {
                        self.copy_text(&code);
                    }
                }
                return Ok(true);
            }
            "dismiss" | "dismiss_all" => {
                let eligible: Vec<_> = link
                    .notifications
                    .items
                    .values()
                    .filter(|item| {
                        item.dismissable && (action == "dismiss_all" || item.id == notif_id)
                    })
                    .map(|item| item.id.clone())
                    .collect();
                let mut changed = false;
                for id in eligible {
                    if !link.queue(Packet::new(notifications::REQUEST, json!({"cancel": id}))) {
                        break;
                    }
                    link.notifications.items.remove(&id);
                    changed = true;
                }
                changed
            }
            _ => bail!("unknown notification action"),
        };
        if changed {
            drop(links);
            self.changed();
        }
        Ok(changed)
    }

    pub fn desktop_notification_action(&self, runtime: &Handle, key: &Key, action: &str) {
        if !self.notification_popup_exists(key) {
            return;
        }
        if action == "default" || action == "reply" {
            let key = key.clone();
            let reply = action == "reply";
            runtime.spawn(async move {
                let args = if reply {
                    vec!["--reply", key.device.as_str(), key.id.as_str()]
                } else {
                    vec!["--device", key.device.as_str(), "--page", "notifications"]
                };
                let _ = crate::tray::open_window(&args);
            });
            return;
        }
        if action == "dismiss" {
            let _ = self.notification_action_on(
                &key.device,
                Some(key.serial),
                "dismiss",
                json!({"id": key.id}).as_object(),
            );
        } else if let Some(title) = action.strip_prefix("action:") {
            let _ = self.notification_action_on(
                &key.device,
                Some(key.serial),
                "invoke_action",
                json!({"id": key.id, "action": title}).as_object(),
            );
        }
    }

    pub fn desktop_notification_reply(&self, key: &Key, text: &str) {
        if self.notification_popup_exists(key) {
            let _ = self.notification_action_on(
                &key.device,
                Some(key.serial),
                "reply",
                json!({"id": key.id, "message": text}).as_object(),
            );
        }
    }
}
