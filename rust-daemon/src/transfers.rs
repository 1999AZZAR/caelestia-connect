use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, SemaphorePermit, broadcast, watch};

const KEEP_FINISHED: usize = 20;
const MAX_ACTIVE: usize = 16;

struct Transfer {
    id: u64,
    device: String,
    device_name: String,
    filename: String,
    direction: &'static str,
    total: u64,
    bytes: u64,
    status: &'static str,
    error: String,
    path: String,
    started: Instant,
    last_emit: Instant,
    finished: Option<Instant>,
    cancel: watch::Sender<bool>,
}

impl Transfer {
    fn snapshot(&self) -> Value {
        let elapsed = self
            .finished
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
            .as_secs_f64();
        json!({
            "id": self.id, "device": self.device, "deviceName": self.device_name,
            "filename": self.filename, "direction": self.direction, "total": self.total, "bytes": self.bytes,
            "state": self.status, "error": self.error, "path": self.path,
            "speed": if elapsed > 0.0 { (self.bytes as f64 / elapsed) as u64 } else { 0 },
        })
    }
}

#[derive(Default)]
struct Entries {
    next_id: u64,
    items: BTreeMap<u64, Transfer>,
}

pub struct Transfers {
    entries: Mutex<Entries>,
    active: Semaphore,
    revision: watch::Sender<u64>,
    events: broadcast::Sender<Value>,
}

impl Transfers {
    pub fn new(revision: watch::Sender<u64>) -> Self {
        Self {
            entries: Mutex::default(),
            active: Semaphore::new(MAX_ACTIVE),
            revision,
            events: broadcast::channel(32).0,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    fn changed(&self) {
        self.revision.send_modify(|rev| *rev = rev.wrapping_add(1));
    }

    pub fn start(
        &self,
        device: &str,
        name: &str,
        filename: String,
        direction: &'static str,
        total: u64,
    ) -> Result<Job<'_>> {
        let permit = self.active.try_acquire()?;
        let mut entries = self.entries.lock().unwrap();
        entries.next_id += 1;
        let id = entries.next_id;
        let (cancel, cancelled) = watch::channel(false);
        let now = Instant::now();
        entries.items.insert(
            id,
            Transfer {
                id,
                device: device.to_owned(),
                device_name: name.to_owned(),
                filename,
                direction,
                total,
                bytes: 0,
                status: "pending",
                error: String::new(),
                path: String::new(),
                started: now,
                last_emit: now,
                finished: None,
                cancel,
            },
        );
        drop(entries);
        self.changed();
        Ok(Job {
            transfers: self,
            id,
            cancelled,
            _permit: permit,
        })
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.entries
            .lock()
            .unwrap()
            .items
            .values()
            .rev()
            .map(Transfer::snapshot)
            .collect()
    }

    pub fn cancel(&self, id: u64) -> bool {
        let entries = self.entries.lock().unwrap();
        if let Some(transfer) = entries.items.get(&id).filter(|t| t.finished.is_none()) {
            transfer.cancel.send_replace(true);
            true
        } else {
            false
        }
    }

    pub fn cancel_device(&self, id: &str) {
        let entries = self.entries.lock().unwrap();
        for transfer in entries
            .items
            .values()
            .filter(|t| t.device == id && t.finished.is_none())
        {
            transfer.cancel.send_replace(true);
        }
    }

    pub fn clear(&self) {
        self.entries
            .lock()
            .unwrap()
            .items
            .retain(|_, t| t.finished.is_none());
        self.changed();
    }
}

pub struct Job<'a> {
    transfers: &'a Transfers,
    id: u64,
    cancelled: watch::Receiver<bool>,
    _permit: SemaphorePermit<'a>,
}

impl Job<'_> {
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancelled.clone()
    }

    pub fn check(&self) -> Result<()> {
        ensure!(!*self.cancelled.borrow(), "transfer cancelled");
        Ok(())
    }

    pub fn progress(&self, bytes: u64) {
        let mut entries = self.transfers.entries.lock().unwrap();
        if let Some(transfer) = entries.items.get_mut(&self.id) {
            let now = Instant::now();
            let emit = transfer.status == "pending"
                || now.duration_since(transfer.last_emit) >= Duration::from_millis(250);
            transfer.bytes = bytes;
            transfer.status = "running";
            if emit {
                transfer.last_emit = now;
                self.transfers.changed();
            }
        }
    }

    pub fn finish(&self, result: Result<String>) {
        let mut entries = self.transfers.entries.lock().unwrap();
        let Some(transfer) = entries
            .items
            .get_mut(&self.id)
            .filter(|t| t.finished.is_none())
        else {
            return;
        };
        transfer.finished = Some(Instant::now());
        match result {
            Ok(path) => {
                transfer.status = "done";
                transfer.path = path;
                transfer.bytes = transfer.total;
            }
            Err(error) if !*self.cancelled.borrow() => {
                transfer.status = "failed";
                transfer.error = error.to_string();
            }
            Err(_) => transfer.status = "cancelled",
        }
        let event = transfer.snapshot();
        let expired: Vec<_> = entries
            .items
            .values()
            .rev()
            .filter(|t| t.finished.is_some())
            .skip(KEEP_FINISHED)
            .map(|t| t.id)
            .collect();
        for id in expired {
            entries.items.remove(&id);
        }
        drop(entries);
        let _ = self.transfers.events.send(event);
        self.transfers.changed();
    }
}

pub async fn cancelled(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow_and_update() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
}
