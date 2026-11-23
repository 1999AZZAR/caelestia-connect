use super::super::{INFO, valid_art_header};
use crate::{
    share::{accept_payload, payload_listener},
    state::State,
    wire::{Packet, PayloadTransferInfo},
};
use anyhow::{Context, Result, ensure};
use percent_encoding::percent_decode_str;
use serde_json::json;
use std::{
    fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::PathBuf, process::Stdio, sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    runtime::Handle,
    sync::Semaphore,
    time::timeout,
};

const LIMIT: u64 = 5 * 1024 * 1024;
static TRANSFERS: Semaphore = Semaphore::const_new(4);

pub(super) fn file_path(url: &str) -> Option<PathBuf> {
    let raw = url.strip_prefix("file://")?;
    let raw = if raw.starts_with('/') {
        raw
    } else {
        raw.strip_prefix("localhost")?
    };
    let raw = raw.split(['?', '#']).next()?;
    let path = percent_decode_str(raw).decode_utf8().ok()?;
    (path.starts_with('/') && !path.contains('\0')).then(|| PathBuf::from(path.as_ref()))
}

async fn read_art(source: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    source.take(LIMIT + 1).read_to_end(&mut data).await?;
    ensure!(
        (12..=LIMIT as usize).contains(&data.len()),
        "invalid album art size"
    );
    ensure!(valid_art_header(&data), "unsupported album art format");
    Ok(data)
}

async fn load(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = file_path(url) {
        let file = tokio::task::spawn_blocking(move || {
            let path = path.canonicalize()?;
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
                .open(path)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.len() <= LIMIT,
                "invalid album art file"
            );
            Ok::<_, anyhow::Error>(file)
        })
        .await??;
        return read_art(tokio::fs::File::from_std(file)).await;
    }
    ensure!(
        url.starts_with("https://") || url.starts_with("http://"),
        "unsupported album art URL"
    );
    let mut curl = Command::new("curl")
        .args([
            "--disable",
            "--globoff",
            "--fail",
            "--silent",
            "--location",
            "--max-redirs",
            "3",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=http,https",
            "--max-time",
            "12",
            "--max-filesize",
            "5242880",
            "--user-agent",
            "CaelestiaConnect",
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let data = read_art(curl.stdout.take().context("curl output unavailable")?).await?;
    ensure!(curl.wait().await?.success(), "could not fetch album art");
    Ok(data)
}

pub(super) fn send(
    runtime: &Handle,
    state: &Arc<State>,
    id: String,
    serial: u64,
    name: String,
    url: String,
) {
    let Ok(permit) = TRANSFERS.try_acquire() else {
        return;
    };
    let state = Arc::clone(state);
    runtime.spawn(async move {
        let _permit = permit;
        let _ = send_payload(&state, &id, serial, &name, &url).await;
    });
}

async fn send_payload(state: &State, id: &str, serial: u64, name: &str, url: &str) -> Result<()> {
    let peer = state
        .local_media_payload_peer(id, serial)
        .context("desktop media controls unavailable")?;
    let data = timeout(Duration::from_secs(15), load(url)).await??;
    let listener = payload_listener(peer.local_address).await?;
    let mut packet = Packet::new(
        INFO,
        json!({"transferringAlbumArt": true, "player": name, "albumArtUrl": url}),
    );
    packet.payload_size = Some(data.len() as i64);
    packet.payload_transfer_info = Some(PayloadTransferInfo {
        port: listener.local_addr()?.port(),
    });
    state.send_on(id, Some(serial), packet).await?;
    let mut tls = accept_payload(state, id, peer.address, &peer.certificate, listener).await?;
    ensure!(
        state.local_media_allowed(id, serial),
        "desktop media controls unavailable"
    );
    timeout(Duration::from_secs(10), tls.write_all(&data)).await??;
    let _ = timeout(Duration::from_secs(5), tls.shutdown()).await;
    Ok(())
}
