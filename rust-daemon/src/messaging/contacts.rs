use super::{CONTACT_CARDS, CONTACT_REQUEST, CONTACT_UIDS, number, private_dir};
use crate::wire::Packet;
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
};

const CARD_LIMIT: u64 = 512 * 1024;

#[derive(Default, PartialEq, Eq)]
struct Card {
    name: String,
    phones: Vec<String>,
    emails: Vec<String>,
    timestamp: i64,
}

#[derive(Default)]
pub(super) struct Contacts {
    cards: BTreeMap<String, Card>,
    numbers: BTreeMap<String, String>,
}

fn safe_uid(uid: &str) -> Option<String> {
    let safe: String = uid
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "_.-".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.starts_with('.') {
        return None;
    }
    if safe.len() > 200 {
        Some(
            openssl::sha::sha256(uid.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    } else {
        Some(safe)
    }
}

fn normalize(number: &str) -> String {
    let digits: String = number.chars().filter(char::is_ascii_digit).collect();
    digits[digits.len().saturating_sub(8)..].to_owned()
}

fn parse(text: &str) -> Card {
    let mut card = Card::default();
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(continuation) = line.strip_prefix([' ', '\t']) {
            if let Some(previous) = lines.last_mut() {
                previous.push_str(continuation);
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let property = key.split(';').next().unwrap_or_default();
        let property = property
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let value = value
            .replace("\\,", ",")
            .replace("\\;", ";")
            .replace("\\n", " ");
        let value = value.trim();
        match property.as_str() {
            "FN" if !value.is_empty() => card.name = value.to_owned(),
            "N" if card.name.is_empty() => {
                let mut parts: Vec<_> = value
                    .split(';')
                    .filter(|part| !part.is_empty())
                    .take(2)
                    .collect();
                parts.reverse();
                card.name = parts.join(" ");
            }
            "TEL" if !value.is_empty() => card
                .phones
                .push(value.strip_prefix("tel:").unwrap_or(value).to_owned()),
            "EMAIL" if !value.is_empty() => card.emails.push(value.to_owned()),
            "X-KDECONNECT-TIMESTAMP" => card.timestamp = value.parse().unwrap_or(0),
            _ => (),
        }
    }
    card
}

impl Contacts {
    pub(super) fn load(directory: &Path) -> Self {
        let mut contacts = Self::default();
        if let Ok(entries) = fs::read_dir(directory) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_none_or(|extension| extension != "vcf")
                    || !entry.file_type().is_ok_and(|kind| kind.is_file())
                    || !entry
                        .metadata()
                        .is_ok_and(|metadata| metadata.len() <= CARD_LIMIT)
                {
                    continue;
                }
                if let (Some(uid), Ok(text)) = (
                    path.file_stem().and_then(|uid| uid.to_str()),
                    fs::read(&path),
                ) {
                    contacts
                        .cards
                        .insert(uid.to_owned(), parse(&String::from_utf8_lossy(&text)));
                }
            }
        }
        contacts.index();
        contacts
    }

    fn index(&mut self) {
        self.numbers.clear();
        for card in self.cards.values().filter(|card| !card.name.is_empty()) {
            for phone in &card.phones {
                let number = normalize(phone);
                if !number.is_empty() {
                    self.numbers
                        .entry(number)
                        .or_insert_with(|| card.name.clone());
                }
            }
        }
    }

    pub(super) fn lookup<'a>(&'a self, number: &'a str) -> &'a str {
        self.name(number).unwrap_or(number)
    }

    pub(super) fn name(&self, number: &str) -> Option<&str> {
        self.numbers.get(&normalize(number)).map(String::as_str)
    }

    pub(super) fn status(&self) -> Value {
        json!({"count": self.cards.len()})
    }

    pub(super) fn list(&self) -> Value {
        let mut list: Vec<_> = self.cards.iter().collect();
        list.sort_by_cached_key(|(_, card)| card.name.to_lowercase());
        json!(list.into_iter().map(|(uid, card)| {
            json!({"uid": uid, "name": card.name, "phones": card.phones, "emails": card.emails})
        }).collect::<Vec<_>>())
    }

    pub(super) fn receive(
        &mut self,
        directory: &Path,
        packet: &Packet,
    ) -> Result<(bool, Option<Packet>)> {
        let Some(uids) = packet.body.get("uids").and_then(Value::as_array) else {
            return Ok((false, None));
        };
        let uids: Vec<_> = uids
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|uid| safe_uid(uid).map(|safe| (uid, safe)))
            .collect();
        let mut changed = false;
        let response = match packet.kind.as_str() {
            CONTACT_UIDS => {
                let known: BTreeSet<_> = uids.iter().map(|(_, safe)| safe.as_str()).collect();
                let stale: Vec<_> = self
                    .cards
                    .keys()
                    .filter(|uid| !known.contains(uid.as_str()))
                    .cloned()
                    .collect();
                for uid in stale {
                    match fs::remove_file(directory.join(format!("{uid}.vcf"))) {
                        Ok(()) => (),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                        Err(_) => continue,
                    }
                    changed |= self.cards.remove(&uid).is_some();
                }
                let wanted: Vec<_> = uids
                    .iter()
                    .filter_map(|(uid, safe)| {
                        let timestamp = packet.body.get(*uid).and_then(number).unwrap_or_default();
                        self.cards
                            .get(safe)
                            .is_none_or(|card| card.timestamp != timestamp)
                            .then_some(*uid)
                    })
                    .collect();
                (!wanted.is_empty()).then(|| Packet::new(CONTACT_REQUEST, json!({"uids": wanted})))
            }
            CONTACT_CARDS => {
                if !uids.is_empty() {
                    private_dir(directory)?;
                }
                for (uid, safe) in uids {
                    let Some(vcard) = packet
                        .body
                        .get(uid)
                        .and_then(Value::as_str)
                        .filter(|card| !card.is_empty() && card.len() as u64 <= CARD_LIMIT)
                    else {
                        continue;
                    };
                    let mut file = tempfile::NamedTempFile::new_in(directory)?;
                    file.write_all(vcard.as_bytes())?;
                    file.persist(directory.join(format!("{safe}.vcf")))
                        .map_err(|error| error.error)?;
                    let card = parse(vcard);
                    changed |= self.cards.get(&safe) != Some(&card);
                    self.cards.insert(safe, card);
                }
                None
            }
            _ => None,
        };
        if changed {
            self.index();
        }
        Ok((changed, response))
    }
}
