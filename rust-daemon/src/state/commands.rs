use super::{Link, Links, Peer, State};
use crate::{commands, config::Trusted, wire::Packet};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::sync::Arc;

impl Peer {
    pub(super) fn supports_commands(&self, plugin: &str) -> bool {
        match plugin {
            commands::LOCAL => self.accepts(commands::LIST) && self.sends(commands::REQUEST),
            commands::REMOTE => self.sends(commands::LIST) && self.accepts(commands::REQUEST),
            _ => false,
        }
    }
}

impl Links {
    fn command_link(
        &self,
        id: &str,
        plugin: &str,
        serial: Option<u64>,
    ) -> Option<(&Link, &Trusted)> {
        let trust = self
            .trust
            .get(id)
            .filter(|entry| entry.enabled(plugin) && !self.blocked.contains(id))?;
        let link = self.active.get(id).filter(|link| {
            serial.is_none_or(|serial| link.serial == serial) && link.peer.supports_commands(plugin)
        })?;
        Some((link, trust))
    }
}

impl State {
    pub(super) fn start_commands(&self, id: &str) {
        let links = self.links.lock().unwrap();
        if let Some((link, trust)) = links.command_link(id, commands::LOCAL, None) {
            link.queue(Packet::new(
                commands::LIST,
                commands::advertise(&trust.local_commands),
            ));
        }
        if let Some((link, _)) = links.command_link(id, commands::REMOTE, None) {
            link.queue(Packet::new(
                commands::REQUEST,
                json!({"requestCommandList": true}),
            ));
        }
    }

    pub fn local_commands_allowed(&self, id: &str, serial: u64) -> bool {
        self.links
            .lock()
            .unwrap()
            .command_link(id, commands::LOCAL, Some(serial))
            .is_some()
    }

    pub fn local_command_script(&self, id: &str, serial: u64, key: &str) -> Option<String> {
        let links = self.links.lock().unwrap();
        let (_, trust) = links.command_link(id, commands::LOCAL, Some(serial))?;
        trust
            .local_commands
            .get(key)
            .map(|command| command.command.clone())
    }

    pub fn send_command(&self, id: &str, serial: u64, kind: &str, body: Value) -> bool {
        let links = self.links.lock().unwrap();
        links
            .command_link(id, commands::LOCAL, Some(serial))
            .is_some_and(|(link, _)| link.peer.accepts(kind) && link.queue(Packet::new(kind, body)))
    }

    pub fn receive_command_request(self: &Arc<Self>, id: &str, serial: u64, body: &Value) {
        let links = self.links.lock().unwrap();
        let Some((link, trust)) = links.command_link(id, commands::LOCAL, Some(serial)) else {
            return;
        };
        if body.get("requestCommandList").and_then(Value::as_bool) == Some(true) {
            link.queue(Packet::new(
                commands::LIST,
                commands::advertise(&trust.local_commands),
            ));
        } else if let Some(key) = body.get("key").and_then(Value::as_str) {
            let known = trust.local_commands.contains_key(key);
            drop(links);
            if known {
                self.command_jobs.start(self, id, serial, key.to_owned());
            }
        } else if body.get("setup").and_then(Value::as_bool) == Some(true) {
            drop(links);
            let _ = crate::tray::open_window(&["--device", id, "--page", "settings"]);
        } else if body.get("stop").and_then(Value::as_bool) == Some(true) {
            drop(links);
            self.command_jobs.stop(id, serial);
        }
    }

    pub fn receive_command_state(&self, id: &str, serial: u64, packet: Packet) {
        let mut links = self.links.lock().unwrap();
        if links
            .command_link(id, commands::REMOTE, Some(serial))
            .is_none()
        {
            return;
        }
        let link = links.active.get_mut(id).expect("active commands link");
        if link.remote_commands.update(&packet.kind, &packet.body) {
            self.changed();
        }
    }

    pub fn remote_command_action(
        &self,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<bool> {
        let links = self.links.lock().unwrap();
        let (link, _) = links
            .command_link(id, commands::REMOTE, None)
            .context("remote commands are unavailable")?;
        let body = match action {
            "trigger" => {
                let key = args
                    .and_then(|args| args.get("key"))
                    .and_then(Value::as_str)
                    .context("missing command key")?;
                ensure!(link.remote_commands.contains(key), "unknown remote command");
                json!({"key": key})
            }
            "edit_commands" => {
                ensure!(
                    link.remote_commands.can_add(),
                    "the device does not accept command setup requests"
                );
                json!({"setup": true})
            }
            _ => bail!("unknown remote command action"),
        };
        Ok(link.queue(Packet::new(commands::REQUEST, body)))
    }

    pub fn edit_local_commands(
        &self,
        id: &str,
        action: &str,
        args: Option<&Map<String, Value>>,
    ) -> Result<Value> {
        let args = args.context("missing command arguments")?;
        let text = |key| {
            args.get(key)
                .and_then(Value::as_str)
                .with_context(|| format!("missing {key}"))
        };
        let mut links = self.links.lock().unwrap();
        let (_, trust) = links
            .command_link(id, commands::LOCAL, None)
            .context("local commands are unavailable")?;
        let mut commands = trust.local_commands.clone();
        let response = match action {
            "add_command" => {
                let key = commands::key()?;
                ensure!(!commands.contains_key(&key), "duplicate command ID");
                commands.insert(
                    key.clone(),
                    commands::Definition::new(text("name")?, text("command")?)?,
                );
                json!(key)
            }
            "edit_command" => {
                let Some(existing) = commands.get_mut(text("key")?) else {
                    return Ok(json!(false));
                };
                *existing = commands::Definition::new(text("name")?, text("command")?)?;
                json!(true)
            }
            "remove_command" => {
                if commands.remove(text("key")?).is_none() {
                    return Ok(json!(false));
                }
                json!(true)
            }
            _ => bail!("unknown local command action"),
        };
        commands::validate(&commands)?;
        links.trust.plugin_setting(
            id,
            commands::LOCAL,
            "commands",
            serde_json::to_value(commands)?,
        )?;
        if let Some((link, trust)) = links.command_link(id, commands::LOCAL, None) {
            link.queue(Packet::new(
                commands::LIST,
                commands::advertise(&trust.local_commands),
            ));
        }
        drop(links);
        self.changed();
        Ok(response)
    }
}
