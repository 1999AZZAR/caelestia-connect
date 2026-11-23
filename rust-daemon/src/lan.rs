use crate::{
    config::{certificate_id, fingerprint},
    state::{Peer, State, Transmission},
    wire::{self, IDENTITY, IDENTITY_LIMIT, Identity, PAIR, Packet, SHARE},
};
use anyhow::{Context, Result, ensure};
use openssl::{
    asn1::Asn1Time,
    ssl::{Ssl, SslContext, SslVerifyMode},
};
use serde_json::json;
use socket2::{SockRef, TcpKeepalive};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{TcpListener, TcpSocket, TcpStream, UdpSocket},
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval, sleep_until, timeout},
};
use tokio_openssl::SslStream;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const RETRY_LIMIT: Duration = Duration::from_secs(60);
const DISCOVERY_HINT_LIFETIME: Duration = Duration::from_secs(120);
const DISCOVERY_SETTLE: Duration = Duration::from_millis(1500);
const MAX_ALTERNATE_ADDRESSES: usize = 4;

type Target = (String, SocketAddrV4, u32);

struct DiscoveryHint {
    seen: Instant,
    endpoints: Vec<(SocketAddrV4, u32)>,
}

fn remember_discovery(
    state: &State,
    hints: &mut BTreeMap<String, DiscoveryHint>,
    discovered: &[Target],
) {
    let Some((id, _, _)) = discovered.first() else {
        return;
    };
    if state.has_pairing(id) {
        let endpoints = discovered
            .iter()
            .take(MAX_ALTERNATE_ADDRESSES - 1)
            .map(|(_, address, version)| (*address, *version))
            .collect();
        hints.insert(
            id.clone(),
            DiscoveryHint {
                seen: Instant::now(),
                endpoints,
            },
        );
    }
}

struct Retry {
    address: SocketAddrV4,
    next: Instant,
    delay: Duration,
}

pub async fn listen(bind: Ipv4Addr) -> Result<TcpListener> {
    for port in wire::PORTS {
        match TcpListener::bind((bind, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error).context("bind KDE Connect TCP listener"),
        }
    }
    anyhow::bail!("no free KDE Connect TCP port (1716–1764)")
}

pub async fn run(
    state: Arc<State>,
    listener: TcpListener,
    udp: Option<UdpSocket>,
    bind: Ipv4Addr,
    mut refresh: mpsc::Receiver<Option<Ipv4Addr>>,
) -> Result<()> {
    let handshakes = Arc::new(Semaphore::new(16));
    let mut jobs = JoinSet::new();
    let mut dialing = BTreeSet::new();
    let (dial_finished, mut dial_completions) = mpsc::unbounded_channel();
    let mut announce_at = udp.as_ref().map(|_| Instant::now());
    let mut reconnect = interval(RETRY_INTERVAL);
    reconnect.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut retries = BTreeMap::<String, Retry>::new();
    let mut hints = BTreeMap::<String, DiscoveryHint>::new();
    let mut buffer = [0_u8; IDENTITY_LIMIT + 1];
    let mut changes = state.subscribe();
    let mut advertised_name = state.config.name();
    let mdns = udp
        .as_ref()
        .and_then(|_| match crate::mdns::Mdns::start(&state, bind) {
            Ok(service) => Some(service),
            Err(error) => {
                eprintln!("mDNS discovery unavailable: {error}");
                None
            }
        });
    loop {
        let (targets, remembered) = tokio::select! {
            matches = async {
                match &mdns {
                    Some(mdns) => mdns.next().await,
                    None => std::future::pending().await,
                }
            } => {
                remember_discovery(&state, &mut hints, &matches);
                (matches, false)
            }
            _ = async {
                match &mdns {
                    Some(mdns) => mdns.new_address().await,
                    None => std::future::pending().await,
                }
            } => {
                announce_at = Some(Instant::now() + DISCOVERY_SETTLE);
                retries.clear();
                continue;
            }
            _ = async {
                match announce_at {
                    Some(due) => sleep_until(due).await,
                    None => std::future::pending().await,
                }
            } => {
                announce_at = None;
                announce(&state, udp.as_ref(), bind, None).await?;
                continue;
            }
            _ = reconnect.tick(), if udp.is_some() => {
                let mut targets = saved_targets(&state, bind);
                hints.retain(|id, hint| hint.seen.elapsed() < DISCOVERY_HINT_LIFETIME && state.has_pairing(id));
                for (id, hint) in hints.iter().filter(|(id, _)| state.can_dial(id)) {
                    targets.extend(hint.endpoints.iter().map(|(address, version)| (id.clone(), *address, *version)));
                }
                retries.retain(|id, _| targets.iter().any(|(device, _, _)| device == id));
                (targets, true)
            }
            _ = changes.changed() => {
                let name = state.config.name();
                if name == advertised_name { continue; }
                advertised_name = name;
                if let Some(mdns) = &mdns {
                    if let Err(error) = mdns.advertise(&state, bind) {
                        eprintln!("Could not update the mDNS announcement: {error}");
                    }
                }
                announce(&state, udp.as_ref(), bind, None).await?;
                announce_at = None;
                continue;
            }
            signal = refresh.recv() => {
                let Some(address) = signal else { return Ok(()); };
                announce(&state, udp.as_ref(), bind, address).await?;
                announce_at = None;
                if udp.is_some() { (saved_targets(&state, bind), false) } else { continue; }
            }
            result = async {
                match &udp {
                    Some(socket) => socket.recv_from(&mut buffer).await,
                    None => std::future::pending().await,
                }
            } => {
                let (length, address) = result.context("receive UDP discovery")?;
                let SocketAddr::V4(address) = address else { continue; };
                if length > IDENTITY_LIMIT || address.ip().is_unspecified() || address.ip().is_multicast()
                    || (bind.is_unspecified() && address.ip().is_loopback()) {
                    continue;
                }
                let Some(identity) = serde_json::from_slice::<Packet>(&buffer[..length]).ok().and_then(|packet| packet.identity().ok()) else {
                    continue;
                };
                if !wire::PORTS.contains(&identity.tcp_port) || !identity.is_targeted_to(&state.config.id) {
                    continue;
                }
                let targets = vec![(identity.device_id, SocketAddrV4::new(*address.ip(), identity.tcp_port), identity.protocol_version)];
                remember_discovery(&state, &mut hints, &targets);
                (targets, false)
            }
            result = listener.accept() => {
                let (stream, address) = result.context("accept TCP connection")?;
                let SocketAddr::V4(address) = address else { continue; };
                let Ok(permit) = handshakes.clone().try_acquire_owned() else { continue; };
                let state = state.clone();
                jobs.spawn(async move {
                    let established = timeout(HANDSHAKE_TIMEOUT, incoming(&state, stream)).await;
                    drop(permit);
                    if let Ok(Ok((identity, stream))) = established {
                        let _ = serve_link(&state, identity, stream, *address.ip(), None).await;
                    }
                });
                continue;
            }
            Some((id, connected)) = dial_completions.recv() => {
                dialing.remove(&id);
                if connected {
                    retries.remove(&id);
                }
                continue;
            }
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                result.context("LAN worker failed")?;
                continue;
            }
        };
        let mut candidates = BTreeMap::<String, Vec<_>>::new();
        for (id, address, version) in targets {
            if id != state.config.id
                && !dialing.contains(&id)
                && state.can_dial(&id)
                && state.certificate_pin(&id, version).is_ok()
            {
                let options = candidates.entry(id).or_default();
                if options.len() < MAX_ALTERNATE_ADDRESSES && !options.contains(&(address, version))
                {
                    options.push((address, version));
                }
            }
        }
        for (id, options) in candidates {
            let now = Instant::now();
            let address = options[0].0;
            let last_retry = retries.get(&id).filter(|retry| retry.address == address);
            if remembered && last_retry.is_some_and(|retry| retry.next > now) {
                continue;
            }
            let Ok(permit) = handshakes.clone().try_acquire_owned() else {
                break;
            };
            if remembered {
                let delay =
                    last_retry.map_or(RETRY_INTERVAL, |retry| (2 * retry.delay).min(RETRY_LIMIT));
                retries.insert(
                    id.clone(),
                    Retry {
                        address,
                        next: now + delay,
                        delay,
                    },
                );
            }
            dialing.insert(id.clone());
            let state = state.clone();
            let dial_finished = dial_finished.clone();
            jobs.spawn(async move {
                let established = timeout(HANDSHAKE_TIMEOUT, async {
                    for (address, version) in options {
                        if !state.can_dial(&id) {
                            return None;
                        }
                        if let Ok((identity, stream)) =
                            outgoing(&state, address, bind, &id, version).await
                        {
                            return Some((address, identity, stream));
                        }
                    }
                    None
                })
                .await
                .ok()
                .flatten();
                drop(permit);
                let _ = dial_finished.send((id, established.is_some()));
                if let Some((address, identity, stream)) = established {
                    let _ = serve_link(
                        &state,
                        identity,
                        stream,
                        *address.ip(),
                        Some(address.port()),
                    )
                    .await;
                }
            });
        }
    }
}

fn saved_targets(state: &State, bind: Ipv4Addr) -> Vec<Target> {
    state
        .known_addresses()
        .into_iter()
        .filter(|(_, address, _)| !bind.is_loopback() || address.ip().is_loopback())
        .collect()
}

async fn announce(
    state: &State,
    udp: Option<&UdpSocket>,
    bind: Ipv4Addr,
    address: Option<Ipv4Addr>,
) -> Result<()> {
    if let Some(udp) = udp {
        let mut body = state.config.identity(false);
        body["tcpPort"] = json!(state.tcp_port);
        let frame = wire::encode(&Packet::new(IDENTITY, body))?;
        let destinations: BTreeSet<_> = std::iter::once(Ipv4Addr::BROADCAST)
            .filter(|_| !bind.is_loopback())
            .chain(state.config.custom_addresses())
            .chain(address)
            .collect();
        for ip in destinations {
            if !bind.is_loopback() || ip.is_loopback() {
                let _ = udp.send_to(&frame, (ip, wire::DISCOVERY_PORT)).await;
            }
        }
    }
    Ok(())
}

type SecureReader = BufReader<SslStream<TcpStream>>;

async fn incoming(state: &State, mut stream: TcpStream) -> Result<(Identity, SecureReader)> {
    let identity = wire::plaintext_identity(&mut stream).await?;
    ensure!(
        identity.is_targeted_to(&state.config.id),
        "identity targets another device"
    );
    secure(
        state,
        stream,
        &identity.device_id,
        identity.protocol_version,
        false,
    )
    .await
}

async fn outgoing(
    state: &State,
    addr: SocketAddrV4,
    bind: Ipv4Addr,
    id: &str,
    version: u32,
) -> Result<(Identity, SecureReader)> {
    let socket = TcpSocket::new_v4()?;
    if !bind.is_unspecified() {
        socket.bind(SocketAddrV4::new(bind, 0).into())?;
    }
    let mut stream = timeout(CONNECT_TIMEOUT, socket.connect(addr.into())).await??;
    let mut body = state.config.identity(false);
    body["targetDeviceId"] = json!(id);
    body["targetProtocolVersion"] = json!(version);
    wire::write_frame(&mut stream, &Packet::new(IDENTITY, body)).await?;
    secure(state, stream, id, version, true).await
}

async fn secure(
    state: &State,
    stream: TcpStream,
    id: &str,
    version: u32,
    dialed: bool,
) -> Result<(Identity, SecureReader)> {
    ensure!(id != state.config.id, "connection from own device ID");
    stream.set_nodelay(true)?;
    SockRef::from(&stream).set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3),
    )?;
    let expected = state.certificate_pin(id, version)?;
    // KDE Connect reverses TCP and TLS roles on the control connection.
    let mut tls = authenticated_stream(&state.config.tls, stream, id, expected, dialed).await?;
    let mut body = state.config.identity(true);
    body["tcpPort"] = json!(state.tcp_port);
    wire::write_frame(&mut tls, &Packet::new(IDENTITY, body)).await?;
    let mut reader = BufReader::new(tls);
    let frame = wire::read_frame(&mut reader, &mut Vec::new(), IDENTITY_LIMIT)
        .await?
        .context("missing encrypted identity")?;
    let identity = serde_json::from_slice::<Packet>(&frame)?.identity()?;
    ensure!(
        identity.device_id == id && identity.protocol_version == version,
        "encrypted identity mismatch"
    );
    ensure!(
        !wire::device_name(&identity.device_name).is_empty(),
        "missing device name"
    );
    Ok((identity, reader))
}

pub async fn authenticated_stream(
    context: &SslContext,
    stream: TcpStream,
    id: &str,
    expected: Option<Vec<u8>>,
    server: bool,
) -> Result<SslStream<TcpStream>> {
    let expected_id = id.to_owned();
    let now = Asn1Time::days_from_now(0)?;
    let mut ssl = Ssl::new(context)?;
    ssl.set_verify_callback(
        SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT,
        move |_, chain| {
            // KDE Connect uses self-signed certificates; authenticate the leaf directly.
            if chain.error_depth() != 0 {
                return true;
            }
            chain.current_cert().is_some_and(|cert| {
                cert.not_before() <= now
                    && cert.not_after() >= now
                    && certificate_id(cert).is_ok_and(|name| name == expected_id)
                    && expected
                        .as_ref()
                        .is_none_or(|pin| cert.to_der().is_ok_and(|der| &der == pin))
            })
        },
    );
    let mut tls = SslStream::new(ssl, stream)?;
    if server {
        Pin::new(&mut tls).accept().await.context("accept TLS")?;
    } else {
        Pin::new(&mut tls).connect().await.context("connect TLS")?;
    }
    ensure!(
        tls.ssl().peer_certificate().is_some(),
        "peer did not present a certificate"
    );
    Ok(tls)
}

async fn serve_link(
    state: &Arc<State>,
    identity: Identity,
    mut stream: SecureReader,
    address: Ipv4Addr,
    dialed_port: Option<u16>,
) -> Result<()> {
    let cert = stream
        .get_ref()
        .ssl()
        .peer_certificate()
        .context("missing peer certificate")?;
    let fingerprint = fingerprint(&cert)?;
    let SocketAddr::V4(local_address) = stream.get_ref().get_ref().local_addr()? else {
        anyhow::bail!("IPv4 connection required");
    };
    let id = identity.device_id;
    let (outbound, mut outgoing) = mpsc::channel::<Transmission>(32);
    let serial = state.register(
        Peer {
            id: id.clone(),
            name: wire::device_name(&identity.device_name),
            device_type: wire::device_name(&identity.device_type),
            version: identity.protocol_version,
            fingerprint,
            certificate: cert,
            address,
            local_address: *local_address.ip(),
            port: dialed_port.unwrap_or(identity.tcp_port),
            incoming_capabilities: identity.incoming_capabilities,
            outgoing_capabilities: identity.outgoing_capabilities,
        },
        dialed_port.is_some(),
        outbound,
    )?;
    let mut buffer = Vec::new();
    let online_since = Instant::now();
    let reason = loop {
        tokio::select! {
            received = wire::read_frame(&mut stream, &mut buffer, state.network_frame_limit(&id, serial)) => {
                let frame = match received {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break "peer closed the connection".to_owned(),
                    Err(error) => break format!("could not read from the peer: {error:#}"),
                };
                if let Ok(packet) = serde_json::from_slice::<Packet>(&frame) {
                    if packet.kind == PAIR {
                        if let Err(error) = state.peer_pair(&id, serial, &packet.body) {
                            break format!("pairing update failed: {error:#}");
                        }
                    } else if packet.kind == SHARE {
                        crate::share::receive(state, &id, serial, packet);
                    } else if packet.kind == crate::media::INFO {
                        state.receive_media(&id, serial, packet);
                    } else if packet.kind == crate::media::REQUEST {
                        state.receive_local_media(&id, serial, packet.body);
                    } else if crate::messaging::incoming_plugin(&packet.kind).is_some() {
                        state.receive_messaging(&id, serial, packet);
                    } else if packet.kind == crate::notifications::INFO {
                        state.receive_notification(&id, serial, packet);
                    } else if packet.kind == crate::state::telephony::EVENT {
                        state.receive_call(&id, serial, &packet.body).await;
                    } else if packet.kind == crate::audio::INFO {
                        state.receive_remote_sinks(&id, serial, packet.body);
                    } else if packet.kind == crate::audio::REQUEST {
                        state.receive_volume_request(&id, serial, packet.body);
                    } else if packet.kind == crate::commands::REQUEST {
                        state.receive_command_request(&id, serial, &packet.body);
                    } else if matches!(packet.kind.as_str(), crate::commands::LIST | crate::commands::OUTPUT) {
                        state.receive_command_state(&id, serial, packet);
                    } else {
                        state.receive_status(&id, serial, &packet);
                    }
                }
            }
            transmission = outgoing.recv() => {
                let Some(transmission) = transmission else { break "link was replaced".to_owned(); };
                if !state.can_transmit(&id, serial, &transmission.packet) {
                    if let Some(completed) = transmission.completed {
                        let _ = completed.send(Err(anyhow::anyhow!("device is unpaired or disconnected")));
                    }
                    continue;
                }
                let result = timeout(Duration::from_secs(4), wire::write_frame(stream.get_mut(), &transmission.packet))
                    .await.context("timed out writing to device").and_then(|result| result);
                let failure = result.as_ref().err().map(|error| format!("could not write to the peer: {error:#}"));
                if let Some(completed) = transmission.completed {
                    let _ = completed.send(result);
                }
                if let Some(failure) = failure { break failure; }
            }
        }
    };
    if state.disconnect(&id, serial) {
        eprintln!(
            "Paired LAN connection {serial} ended after {}s: {reason}",
            online_since.elapsed().as_secs()
        );
    }
    Ok(())
}
