use crate::{
    config::expand_home,
    lan::authenticated_stream,
    state::State,
    transfers::{Job, cancelled},
    wire::{self, Packet, PayloadTransferInfo, SHARE},
};
use anyhow::{Context, Result, bail, ensure};
use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use std::{
    fs::{self, File, FileTimes, OpenOptions},
    io::ErrorKind,
    net::Ipv4Addr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    fs::File as AsyncFile,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const PAYLOAD_PORTS: std::ops::RangeInclusive<u16> = 1739..=1764;
const CHUNK: usize = 64 * 1024;
const MAX_BATCH_FILES: usize = 4096;
const WEB_SCHEMES: &[&str] = &["http://", "https://", "mailto:"];

pub struct SharePeer {
    pub serial: u64,
    pub name: String,
    pub address: Ipv4Addr,
    pub local_address: Ipv4Addr,
    pub certificate: Vec<u8>,
    pub directory: PathBuf,
    pub open_urls: bool,
}

pub fn destination(root: &Path, name: &str) -> PathBuf {
    PathBuf::from(root.to_string_lossy().replace("%1", &clean_name(name)))
}

fn clean_name(name: &str) -> String {
    let component = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let mut output = String::new();
    for letter in component.chars().filter(|letter| !letter.is_control()) {
        if output.len() + letter.len_utf8() > 230 {
            break;
        }
        output.push(letter);
    }
    if output.is_empty() || output == "." || output == ".." {
        format!("file-{}", wire::timestamp_ms())
    } else {
        output
    }
}

fn local_file_path(path: &str) -> Option<PathBuf> {
    if let Some(uri) = path.strip_prefix("file://") {
        let path = uri.strip_prefix("localhost").unwrap_or(uri);
        if !path.starts_with('/') {
            return None;
        }
        let decoded = percent_decode_str(path).decode_utf8().ok()?;
        return Some(PathBuf::from(decoded.as_ref()));
    }
    Some(expand_home(path))
}

struct Upload {
    path: PathBuf,
    length: u64,
    modified: u64,
}

fn collect_uploads(paths: Vec<String>) -> Result<Vec<Upload>> {
    let mut pending: Vec<_> = paths
        .iter()
        .rev()
        .filter_map(|path| local_file_path(path))
        .collect();
    let mut uploads = Vec::new();
    while let Some(path) = pending.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if let Ok(children) = path.read_dir() {
                let mut files: Vec<_> = children
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect();
                files.sort_unstable();
                pending.extend(files.into_iter().rev());
            }
        } else if metadata.is_file() && open_source(&path).is_ok() {
            ensure!(
                uploads.len() < MAX_BATCH_FILES,
                "too many files in the transfer batch"
            );
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis() as u64);
            uploads.push(Upload {
                path,
                length: metadata.len(),
                modified,
            });
        }
    }
    Ok(uploads)
}

fn open_source(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "source is not a regular file");
    Ok(file)
}

pub async fn send_files(
    state: &Arc<State>,
    id: &str,
    paths: Vec<String>,
    open: bool,
) -> Result<usize> {
    let peer = state.share_peer(id, None)?;
    let uploads = tokio::task::spawn_blocking(move || collect_uploads(paths)).await??;
    let count = uploads.len();
    if count == 0 {
        return Ok(0);
    }
    let total = uploads
        .iter()
        .try_fold(0_u64, |total, file| total.checked_add(file.length))
        .context("file batch is too large")?;
    let state = Arc::clone(state);
    let id = id.to_owned();
    tokio::spawn(async move {
        for file in uploads {
            if state.share_peer(&id, Some(peer.serial)).is_err() {
                break;
            }
            let filename = clean_name(
                file.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
            );
            let Ok(job) =
                state
                    .transfers
                    .start(&id, &peer.name, filename.clone(), "upload", file.length)
            else {
                break;
            };
            let body = json!({"filename": filename, "lastModified": file.modified, "creationTime": file.modified,
                "open": open, "numberOfFiles": count, "totalPayloadSize": total});
            let result = tokio::select! {
                biased;
                _ = cancelled(job.cancellation()) => Err(anyhow::anyhow!("transfer cancelled")),
                result = upload(&state, &id, &peer, file, body, &job) => result.map(|()| String::new()),
            };
            let stop_batch = job.check().is_err();
            job.finish(result);
            if stop_batch {
                break;
            }
        }
    });
    Ok(count)
}

pub async fn payload_listener(address: Ipv4Addr) -> Result<TcpListener> {
    for port in PAYLOAD_PORTS {
        match TcpListener::bind((address, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("no free payload port")
}

async fn upload(
    state: &State,
    id: &str,
    peer: &SharePeer,
    file: Upload,
    body: Value,
    job: &Job<'_>,
) -> Result<()> {
    let mut source =
        AsyncFile::from_std(tokio::task::spawn_blocking(move || open_source(&file.path)).await??);
    ensure!(
        source.metadata().await?.len() == file.length,
        "source size changed"
    );
    let listener = payload_listener(peer.local_address).await?;
    let mut packet = Packet::new(SHARE, body);
    packet.payload_size = Some(i64::try_from(file.length).context("file is too large")?);
    packet.payload_transfer_info = Some(PayloadTransferInfo {
        port: listener.local_addr()?.port(),
    });
    state.send_on(id, Some(peer.serial), packet).await?;
    let mut tls = accept_payload(state, id, peer.address, &peer.certificate, listener).await?;
    copy_payload(&mut source, &mut tls, Some(file.length), job).await?;
    let _ = timeout(CONNECT_TIMEOUT, tls.shutdown()).await;
    Ok(())
}

pub async fn accept_payload(
    state: &State,
    id: &str,
    address: Ipv4Addr,
    certificate: &[u8],
    listener: TcpListener,
) -> Result<tokio_openssl::SslStream<TcpStream>> {
    timeout(Duration::from_secs(30), async {
        loop {
            let (stream, remote) = listener.accept().await?;
            if remote.ip() == address {
                let secure = authenticated_stream(
                    &state.config.tls,
                    stream,
                    id,
                    Some(certificate.to_vec()),
                    true,
                );
                if let Ok(Ok(tls)) = timeout(CONNECT_TIMEOUT, secure).await {
                    return Ok::<_, anyhow::Error>(tls);
                }
            }
        }
    })
    .await
    .context("phone did not connect to the payload port")?
}

pub fn receive(state: &Arc<State>, id: &str, serial: u64, packet: Packet) {
    let Ok(peer) = state.share_peer(id, Some(serial)) else {
        return;
    };
    if let Some(remote_name) = packet.body.get("filename").and_then(Value::as_str) {
        let filename = clean_name(remote_name);
        let length = packet.payload_size.unwrap_or_default();
        let port = packet.payload_transfer_info.map(|info| info.port);
        let modified = packet
            .body
            .get("lastModified")
            .and_then(Value::as_u64)
            .filter(|time| *time > 0);
        let id = id.to_owned();
        let state = Arc::clone(state);
        tokio::spawn(async move {
            let Ok(peer) = state.share_peer(&id, Some(serial)) else {
                return;
            };
            let Ok(job) = state.transfers.start(
                &id,
                &peer.name,
                filename.clone(),
                "download",
                u64::try_from(length).unwrap_or(0),
            ) else {
                return;
            };
            let result = tokio::select! {
                biased;
                _ = cancelled(job.cancellation()) => Err(anyhow::anyhow!("transfer cancelled")),
                result = download(&state, &id, &peer, &filename, (length, port, modified), &job) => result,
            };
            job.finish(result.map(|path| path.to_string_lossy().into_owned()));
        });
    } else if let Some(url) = packet
        .body
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| url.len() <= 8192)
    {
        if peer.open_urls
            && WEB_SCHEMES.iter().any(|prefix| url.starts_with(prefix))
            && !url
                .chars()
                .any(|char| char.is_control() || char.is_whitespace())
        {
            let _ = crate::clipboard::open(Path::new(url));
        } else {
            state.copy_text(url);
        }
    } else if let Some(text) = packet
        .body
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| text.len() <= 8192)
    {
        state.copy_text(text);
    }
}

async fn download(
    state: &State,
    id: &str,
    peer: &SharePeer,
    filename: &str,
    payload: (i64, Option<u16>, Option<u64>),
    job: &Job<'_>,
) -> Result<PathBuf> {
    let (length, port, modified) = payload;
    let size = if length == -1 {
        None
    } else {
        Some(u64::try_from(length).context("invalid payload size")?)
    };
    ensure!(port.is_some() || length == 0, "payload port missing");
    tokio::fs::create_dir_all(&peer.directory).await?;
    let temporary =
        tempfile::NamedTempFile::with_prefix_in(".caelestia-transfer-", &peer.directory)?;
    let mut sink = AsyncFile::from_std(temporary.as_file().try_clone()?);
    if let Some(port) = port {
        ensure!(port != 0, "invalid payload port");
        let mut tls = timeout(CONNECT_TIMEOUT, async {
            let tcp = TcpStream::connect((peer.address, port)).await?;
            authenticated_stream(
                &state.config.tls,
                tcp,
                id,
                Some(peer.certificate.clone()),
                false,
            )
            .await
        })
        .await
        .context("could not connect to the payload")??;
        copy_payload(&mut tls, &mut sink, size, job).await?;
        let _ = timeout(CONNECT_TIMEOUT, tls.shutdown()).await;
    }
    sink.flush().await?;
    sink.sync_all().await?;
    if let Some(modified) =
        modified.and_then(|value| UNIX_EPOCH.checked_add(Duration::from_millis(value)))
    {
        let _ = temporary
            .as_file()
            .set_times(FileTimes::new().set_modified(modified));
    }
    job.check()?;
    persist(temporary, &peer.directory, filename)
}

fn persist(mut temporary: tempfile::NamedTempFile, root: &Path, filename: &str) -> Result<PathBuf> {
    let name = Path::new(filename);
    let stem = name.file_stem().unwrap_or_default().to_string_lossy();
    let suffix = name
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    for index in 0..10_000 {
        let candidate = if index == 0 {
            root.join(name)
        } else {
            root.join(format!("{stem} ({index}){suffix}"))
        };
        match temporary.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => temporary = error.file,
            Err(error) => return Err(error.error).context("save received file"),
        }
    }
    bail!("could not allocate a filename")
}

async fn copy_payload(
    source: &mut (impl AsyncRead + Unpin),
    sink: &mut (impl AsyncWrite + Unpin),
    length: Option<u64>,
    job: &Job<'_>,
) -> Result<()> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut total = 0;
    while length.is_none_or(|length| total < length) {
        let count = length.map_or(CHUNK, |length| (length - total).min(CHUNK as u64) as usize);
        let received = timeout(IDLE_TIMEOUT, source.read(&mut buffer[..count]))
            .await
            .context("payload read timed out")??;
        if received == 0 {
            if let Some(length) = length {
                bail!("incomplete payload ({total}/{length})");
            }
            break;
        }
        timeout(IDLE_TIMEOUT, sink.write_all(&buffer[..received]))
            .await
            .context("payload write timed out")??;
        total += received as u64;
        job.progress(total);
    }
    timeout(IDLE_TIMEOUT, sink.flush()).await??;
    Ok(())
}
