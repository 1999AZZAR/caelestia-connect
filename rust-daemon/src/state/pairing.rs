use super::{Link, Links, State, Transmission};
use crate::wire::{self, PAIR, Packet};
use anyhow::{Context, Result, ensure};
use openssl::sha::Sha256;
use serde_json::{Value, json};
use std::{future::pending, time::Duration};
use tokio::time::{Instant, sleep_until};

const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CLOCK_SKEW: u64 = 1800;

pub enum PairCommand {
    Request,
    Accept,
    Reject,
    Unpair,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Sent,
    Received,
}

pub(super) struct Pending {
    direction: Direction,
    until: Instant,
    timestamp: u64,
    pub(super) verification_key: String,
}

impl Pending {
    fn new(direction: Direction, desktop: &[u8], link: &Link, timestamp: u64) -> Result<Self> {
        let peer = link.peer.certificate.public_key()?.public_key_to_der()?;
        let (first, second) = if desktop < peer.as_slice() {
            (peer.as_slice(), desktop)
        } else {
            (desktop, peer.as_slice())
        };
        let mut hash = Sha256::new();
        hash.update(first);
        hash.update(second);
        hash.update(timestamp.to_string().as_bytes());
        let verification_key = hash.finish()[..4]
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        Ok(Self {
            direction,
            until: Instant::now() + PAIR_TIMEOUT,
            timestamp,
            verification_key,
        })
    }

    pub(super) fn is_current(&self) -> bool {
        self.until > Instant::now()
    }

    pub(super) fn state(&self) -> &'static str {
        match self.direction {
            Direction::Sent => "requested",
            Direction::Received => "requestedByPeer",
        }
    }

    pub(super) fn matches_sent_request(&self, timestamp: u64) -> bool {
        self.direction == Direction::Sent && self.timestamp == timestamp && self.is_current()
    }
}

fn transmission(pair: bool, timestamp: Option<u64>) -> Transmission {
    let mut body = json!({"pair": pair});
    if let Some(timestamp) = timestamp {
        body["timestamp"] = json!(timestamp);
    }
    Transmission {
        packet: Packet::new(PAIR, body),
        completed: None,
    }
}

fn send(link: &Link, pair: bool, timestamp: Option<u64>) -> Result<()> {
    link.outbound
        .try_send(transmission(pair, timestamp))
        .context("device is busy or disconnected")
}

fn commit(links: &mut Links, id: &str) -> Result<()> {
    let link = links.active.get(id).context("device is disconnected")?;
    let peer = &link.peer;
    let certificate = String::from_utf8(peer.certificate.to_pem()?)?;
    links.trust.trust(id, json!({
        "name": peer.name, "type": peer.device_type, "protocolVersion": peer.version,
        "certificate": certificate, "lastAddress": peer.address.to_string(), "lastPort": peer.port,
    }))?;
    let link = links.active.get_mut(id).unwrap();
    link.pending = None;
    link.pair_error = "";
    links
        .last_seen
        .insert(id.to_owned(), wire::timestamp_ms() / 1000);
    Ok(())
}

fn forget(links: &mut Links, id: &str) -> Result<bool> {
    let forgotten = links.trust.forget(id);
    if forgotten.is_err() {
        links.blocked.insert(id.to_owned());
    } else {
        links.blocked.remove(id);
    }
    links.last_seen.remove(id);
    links.reconnect_batteries.remove(id);
    forgotten
}

fn finish(link: &mut Link, error: &'static str) {
    link.pending = None;
    link.pair_error = error;
}

impl State {
    pub fn pair_command(&self, id: &str, command: PairCommand) -> Result<bool> {
        let mut links = self.links.lock().unwrap();
        let was_paired = links.trust.get(id).is_some();
        let result = self.apply_command(&mut links, id, command);
        let mut newly_paired = false;
        if links.trust.get(id).is_none() || links.blocked.contains(id) {
            if let Some(link) = links.active.get_mut(id) {
                link.media = Default::default();
            }
            self.transfers.cancel_device(id);
        } else if !was_paired {
            links.request_battery(id);
            links.start_media(id);
            newly_paired = true;
        }
        drop(links);
        if newly_paired {
            self.start_messaging(id);
            self.start_notifications(id);
            self.request_remote_sinks(id);
            self.start_commands(id);
        }
        self.changed();
        result
    }

    fn apply_command(&self, links: &mut Links, id: &str, command: PairCommand) -> Result<bool> {
        if matches!(command, PairCommand::Unpair) {
            let removed = forget(links, id)?;
            if let Some(link) = links.active.get_mut(id) {
                let attempted = link.pending.is_some();
                finish(link, "");
                link.status.clear();
                link.remote_sinks.clear();
                link.call_activity = None;
                link.remote_commands = Default::default();
                link.notifications = Default::default();
                if (removed || attempted) && send(link, false, None).is_err() {
                    links.active.remove(id);
                }
                return Ok(removed || attempted);
            }
            return Ok(removed);
        }

        ensure!(
            !links.blocked.contains(id),
            "could not persist withdrawn pairing"
        );
        if links.trust.get(id).is_some() {
            return Ok(matches!(command, PairCommand::Accept));
        }
        let link = links.active.get_mut(id).context("device is disconnected")?;
        if link
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.is_current())
        {
            finish(link, "Timed out");
            let _ = send(link, false, None);
            return Ok(false);
        }
        let direction = link.pending.as_ref().map(|pending| pending.direction);
        match command {
            PairCommand::Request if direction.is_none() => {
                let timestamp = wire::timestamp_ms() / 1000;
                let pending =
                    Pending::new(Direction::Sent, &self.config.public_key, link, timestamp)?;
                send(link, true, Some(timestamp))?;
                link.pending = Some(pending);
                link.pair_error = "";
                Ok(true)
            }
            PairCommand::Request | PairCommand::Accept
                if direction == Some(Direction::Received) =>
            {
                let slot = link
                    .outbound
                    .clone()
                    .try_reserve_owned()
                    .context("device is busy or disconnected")?;
                // Persist trust before acknowledging the request to the peer.
                commit(links, id)?;
                slot.send(transmission(true, None));
                Ok(true)
            }
            PairCommand::Reject if direction.is_some() => {
                send(link, false, None)?;
                finish(link, "Canceled by user");
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn peer_pair(&self, id: &str, serial: u64, body: &Value) -> Result<()> {
        let mut links = self.links.lock().unwrap();
        if links
            .active
            .get(id)
            .is_none_or(|link| link.serial != serial)
        {
            return Ok(());
        }
        let was_paired = links.trust.get(id).is_some();
        let result = self.apply_peer_pair(&mut links, id, body);
        let mut newly_paired = false;
        if links.trust.get(id).is_none() || links.blocked.contains(id) {
            if let Some(link) = links.active.get_mut(id) {
                link.media = Default::default();
            }
            self.transfers.cancel_device(id);
        } else if !was_paired {
            links.request_battery(id);
            links.start_media(id);
            newly_paired = true;
        }
        drop(links);
        if newly_paired {
            self.start_messaging(id);
            self.start_notifications(id);
            self.request_remote_sinks(id);
            self.start_commands(id);
        }
        self.changed();
        result
    }

    fn apply_peer_pair(&self, links: &mut Links, id: &str, body: &Value) -> Result<()> {
        let Some(wants_pair) = body.get("pair").and_then(Value::as_bool) else {
            return Ok(());
        };
        let paired = links.trust.get(id).is_some();
        let link = links.active.get_mut(id).unwrap();
        if link
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.is_current())
        {
            finish(link, "Timed out");
            let _ = send(link, false, None);
            return Ok(());
        }
        let direction = link.pending.as_ref().map(|pending| pending.direction);
        if !wants_pair {
            link.status.clear();
            link.remote_sinks.clear();
            link.call_activity = None;
            link.remote_commands = Default::default();
            link.notifications = Default::default();
            finish(
                link,
                if direction.is_some() {
                    "Canceled by the other device"
                } else {
                    ""
                },
            );
            forget(links, id)?;
            return Ok(());
        }

        let Some(value) = body.get("timestamp") else {
            if direction == Some(Direction::Sent) {
                commit(links, id)?;
            }
            return Ok(());
        };
        let timestamp = value.as_u64().or_else(|| value.as_str()?.parse().ok());
        let now = wire::timestamp_ms() / 1000;
        let Some(timestamp) =
            timestamp.filter(|timestamp| timestamp.abs_diff(now) <= MAX_CLOCK_SKEW)
        else {
            if !paired && direction.is_none() {
                finish(link, "Device clocks are out of sync");
                let _ = send(link, false, None);
            }
            return Ok(());
        };
        if direction == Some(Direction::Received)
            || (direction == Some(Direction::Sent) && self.config.id.as_str() < id)
        {
            return Ok(());
        }
        let pending = Pending::new(
            Direction::Received,
            &self.config.public_key,
            link,
            timestamp,
        )?;
        if paired {
            forget(links, id)?;
        }
        let link = links.active.get_mut(id).unwrap();
        link.pending = Some(pending);
        link.pair_error = "";
        Ok(())
    }

    pub async fn expire_pairings(&self) {
        let mut changes = self.subscribe();
        loop {
            let deadline = {
                let links = self.links.lock().unwrap();
                links
                    .active
                    .values()
                    .filter_map(|link| link.pending.as_ref().map(|p| p.until))
                    .min()
            };
            tokio::select! {
                _ = changes.changed() => {}
                _ = async {
                    match deadline {
                        Some(deadline) => sleep_until(deadline).await,
                        None => pending::<()>().await,
                    }
                } => {
                    let mut links = self.links.lock().unwrap();
                    for link in links.active.values_mut() {
                        if link.pending.as_ref().is_some_and(|pending| !pending.is_current()) {
                            finish(link, "Timed out");
                            let _ = send(link, false, None);
                        }
                    }
                    drop(links);
                    self.changed();
                }
            }
        }
    }
}
