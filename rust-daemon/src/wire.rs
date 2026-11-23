use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};

pub const VERSION: u32 = 8;
pub const DISCOVERY_PORT: u16 = 1716;
pub const PORTS: std::ops::RangeInclusive<u16> = 1716..=1764;
pub const IDENTITY: &str = "kdeconnect.identity";
pub const PING: &str = "kdeconnect.ping";
pub const SHARE: &str = "kdeconnect.share.request";
pub const PAIR: &str = "kdeconnect.pair";
pub const IDENTITY_LIMIT: usize = 8192;
pub const FRAME_LIMIT: usize = 64 * 1024;

pub fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Packet {
    id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_size: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_transfer_info: Option<PayloadTransferInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct PayloadTransferInfo {
    pub port: u16,
}

impl Packet {
    pub fn new(kind: &str, body: Value) -> Self {
        Self {
            id: timestamp_ms(),
            kind: kind.to_owned(),
            body,
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    pub fn identity(self) -> Result<Identity> {
        ensure!(self.kind == IDENTITY, "expected an identity packet");
        let identity: Identity = serde_json::from_value(self.body)?;
        ensure!(valid_id(&identity.device_id), "invalid device ID");
        ensure!(
            identity.protocol_version >= VERSION,
            "unsupported protocol version"
        );
        Ok(identity)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub device_id: String,
    pub protocol_version: u32,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub device_type: String,
    #[serde(default)]
    pub tcp_port: u16,
    #[serde(default)]
    pub target_device_id: String,
    #[serde(default)]
    pub target_protocol_version: Value,
    #[serde(default)]
    pub incoming_capabilities: Vec<String>,
    #[serde(default)]
    pub outgoing_capabilities: Vec<String>,
}

impl Identity {
    pub fn is_targeted_to(&self, id: &str) -> bool {
        (self.target_device_id.is_empty() || self.target_device_id == id)
            && match &self.target_protocol_version {
                Value::Null => true,
                Value::Number(number) => number.as_u64() == Some(VERSION.into()),
                Value::String(text) => text.parse::<u32>().ok() == Some(VERSION),
                _ => false,
            }
    }
}

pub fn valid_id(id: &str) -> bool {
    (32..=38).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub fn device_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !"\"',;:.!?()[]<>".contains(*c))
        .take(32)
        .collect::<String>()
        .trim()
        .to_owned()
}

// A buffered read here could consume bytes from the TLS handshake.
pub async fn plaintext_identity(stream: &mut (impl AsyncRead + Unpin)) -> Result<Identity> {
    let mut buffer = Vec::new();
    loop {
        let byte = stream.read_u8().await?;
        if byte == b'\n' {
            return serde_json::from_slice::<Packet>(&buffer)?.identity();
        }
        ensure!(buffer.len() < IDENTITY_LIMIT, "identity too large");
        buffer.push(byte);
    }
}

// Keep partial data in the caller's buffer when select! cancels a read.
pub async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    buffer: &mut Vec<u8>,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            ensure!(buffer.is_empty(), "incomplete frame");
            return Ok(None);
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.unwrap_or(chunk.len());
        ensure!(buffer.len() + length <= limit, "frame too large");
        buffer.extend_from_slice(&chunk[..length]);
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(std::mem::take(buffer)));
        }
    }
}

pub fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(value)?;
    frame.push(b'\n');
    Ok(frame)
}

pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> Result<()> {
    writer
        .write_all(&encode(value)?)
        .await
        .context("write frame")
}

pub fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().context("expected an object")
}
