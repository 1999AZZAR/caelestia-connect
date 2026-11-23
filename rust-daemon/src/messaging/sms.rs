use super::{contacts::Contacts, number, private_dir};
use crate::{config::save_json, wire};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const ARCHIVE_LIMIT: u64 = 64 * 1024 * 1024;
pub(super) const ATTACHMENT_LIMIT: u64 = 8 * 1024 * 1024;
const OUTGOING_ATTACHMENT_LIMIT: u64 = 4 * 1024 * 1024;
type Thread = BTreeMap<String, Value>;

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
pub(super) struct Archive {
    threads: BTreeMap<String, Thread>,
}

fn text(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

fn date(message: &Value) -> i64 {
    message.get("date").and_then(number).unwrap_or_default()
}

fn sent(message: &Value) -> bool {
    message.get("type").and_then(number) == Some(2)
}

fn read(message: &Value) -> bool {
    message
        .get("read")
        .map(|value| match value {
            Value::Bool(value) => *value,
            Value::Number(_) => number(value) != Some(0),
            Value::String(value) => !value.is_empty() && value != "0",
            _ => false,
        })
        .unwrap_or(true)
}

fn addresses(message: &Value) -> Vec<&str> {
    message
        .get("addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|address| address.get("address")?.as_str())
        .collect()
}

fn field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => String::new(),
    }
}

pub(super) fn attachment_name(name: &str) -> Option<String> {
    // Same legacy cache key for ordinary attachment identifiers, with traversal components rejected.
    let filename = name.rsplit(['/', '\\']).next()?;
    let safe: String = filename
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "_.-".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();
    (!safe.is_empty() && safe.len() <= 240 && safe != "." && safe != "..").then_some(safe)
}

impl Archive {
    pub(super) fn load(path: &Path) -> Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            file.metadata()?.len() <= ARCHIVE_LIMIT,
            "SMS archive exceeds 64 MiB"
        );
        Ok(serde_json::from_reader(std::io::BufReader::new(
            file.take(ARCHIVE_LIMIT),
        ))?)
    }

    pub(super) fn save(&self, path: &Path) -> Result<()> {
        private_dir(path.parent().context("missing archive directory")?)?;
        save_json(path, self)
    }

    pub(super) fn merge(&mut self, body: &Value) -> bool {
        let mut changed = false;
        for msg in body
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(thread_id) = msg
                .get("thread_id")
                .and_then(number)
                .filter(|thread| *thread >= 0)
            else {
                continue;
            };
            let thread = self.threads.entry(thread_id.to_string()).or_default();
            if sent(msg) {
                let before = thread.len();
                thread.retain(|key, previous| {
                    !(key.starts_with("pending-") && previous.get("body") == msg.get("body"))
                });
                changed |= before != thread.len();
            }
            let key = format!(
                "{}-{}-{}",
                field(msg.get("_id")),
                field(msg.get("date")),
                field(msg.get("type"))
            );
            if thread.get(&key) != Some(msg) {
                thread.insert(key, msg.clone());
                changed = true;
            }
        }
        changed
    }

    pub(super) fn status(&self, version: u64) -> Value {
        let unread = self
            .threads
            .values()
            .filter_map(|thread| thread.values().max_by_key(|message| date(message)))
            .filter(|message| !sent(message) && !read(message))
            .count();
        json!({"version": version, "threads": self.threads.len(), "unread": unread})
    }

    pub(super) fn conversations(&self, contacts: &Contacts, name_contacts: bool) -> Value {
        let mut threads: Vec<_> = self
            .threads
            .iter()
            .filter_map(|(id, thread)| {
                thread
                    .values()
                    .max_by_key(|message| date(message))
                    .map(|message| (id, thread.len(), message))
            })
            .collect();
        threads.sort_by_key(|(_, _, message)| std::cmp::Reverse(date(message)));
        json!(threads.into_iter().map(|(id, count, message)| {
            let addresses = addresses(message);
            let names: Vec<_> = addresses.iter().map(|address| {
                if name_contacts { contacts.lookup(address) } else { *address }
            }).collect();
            let title = if names.is_empty() { "Unknown".to_owned() } else { names.join(", ") };
            let mut snippet = text(message.get("body"));
            if snippet.is_empty() && message.get("attachments").and_then(Value::as_array).is_some_and(|files| !files.is_empty()) {
                snippet = "Attachment";
            }
            json!({"threadId": id, "addresses": addresses, "names": names, "title": title, "snippet": snippet,
                "date": date(message), "read": read(message) || sent(message), "count": count, "subId": message.get("sub_id")})
        }).collect::<Vec<_>>())
    }

    pub(super) fn messages(
        &self,
        id: &str,
        directory: &Path,
        contacts: &Contacts,
        name_contacts: bool,
    ) -> Value {
        let mut messages: Vec<_> = self
            .threads
            .get(id)
            .into_iter()
            .flat_map(|thread| thread.values())
            .collect();
        messages.sort_by_key(|message| date(message));
        json!(messages.into_iter().map(|message| {
            let address = addresses(message).into_iter().next().unwrap_or_default();
            let name = if name_contacts { contacts.lookup(address) } else { address };
            let attachments: Vec<_> = message.get("attachments").and_then(Value::as_array).into_iter().flatten()
                .filter(|attachment| attachment.is_object()).map(|attachment| {
                    let unique_id = text(attachment.get("unique_identifier"));
                    let path = attachment_name(unique_id).map(|name| directory.join(name))
                        .filter(|path| path.is_file()).map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
                    json!({"partId": attachment.get("part_id"), "mimeType": text(attachment.get("mime_type")),
                        "uniqueId": unique_id, "thumbnail": text(attachment.get("encoded_thumbnail")), "path": path})
                }).collect();
            json!({"body": text(message.get("body")), "date": date(message), "outgoing": sent(message),
                "sender": address, "senderName": name, "read": read(message), "attachments": attachments})
        }).collect::<Vec<_>>())
    }

    pub(super) fn add_pending(
        &mut self,
        recipients: &[String],
        message: &str,
        sub_id: Option<i64>,
    ) -> Option<i64> {
        let mut wanted: Vec<_> = recipients.iter().map(String::as_str).collect();
        wanted.sort_unstable();
        let (id, thread) = self.threads.iter_mut().find(|(_, thread)| {
            thread.values().any(|msg| {
                let mut current = addresses(msg);
                current.sort_unstable();
                current == wanted
            })
        })?;
        let thread_id = id.parse().ok()?;
        let now = wire::timestamp_ms();
        let root_key = format!("pending-{now}");
        let mut key = root_key.clone();
        let mut suffix = 1;
        while thread.contains_key(&key) {
            key = format!("{root_key}-{suffix}");
            suffix += 1;
        }
        thread.insert(
            key,
            json!({"body": message, "date": now, "thread_id": thread_id, "type": 2, "read": true,
            "sub_id": sub_id, "addresses": recipient_objects(recipients)}),
        );
        Some(thread_id)
    }
}

pub(super) fn recipient_objects(recipients: &[String]) -> Vec<Value> {
    recipients
        .iter()
        .map(|address| json!({"address": address}))
        .collect()
}

fn decode_path(path: &str) -> Result<PathBuf> {
    let path = if let Some(uri) = path.strip_prefix("file://") {
        percent_encoding::percent_decode_str(uri)
            .decode_utf8()?
            .into_owned()
    } else {
        path.to_owned()
    };
    let path = PathBuf::from(path);
    ensure!(path.is_absolute(), "use an absolute attachment path");
    Ok(path)
}

fn mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "amr" => "audio/amr",
        "pdf" => "application/pdf",
        "vcf" => "text/vcard",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

pub(super) fn load_attachments(paths: &[Value]) -> Result<Vec<Value>> {
    let mut fields = Vec::new();
    let mut remaining = OUTGOING_ATTACHMENT_LIMIT;
    for path in paths {
        let path = decode_path(path.as_str().context("invalid attachment path")?)?;
        let local = fs::canonicalize(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(local)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= remaining,
            "MMS attachments exceed 4 MiB or are not regular files"
        );
        let mut bytes = Vec::new();
        file.take(remaining + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= remaining,
            "MMS attachments exceed 4 MiB"
        );
        remaining -= bytes.len() as u64;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid attachment name")?;
        fields.push(json!({"fileName": name, "base64EncodedFile": openssl::base64::encode_block(&bytes), "mimeType": mime(&path)}));
    }
    Ok(fields)
}
