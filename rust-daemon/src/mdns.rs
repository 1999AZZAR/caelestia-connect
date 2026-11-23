use crate::{state::State, wire};
use anyhow::Result;
use mdns_sd::{IfKind, Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::{Ipv4Addr, SocketAddrV4};

const SERVICE: &str = "_kdeconnect._udp.local.";

pub struct Mdns {
    daemon: ServiceDaemon,
    name: String,
    events: Receiver<ServiceEvent>,
    loopback: bool,
}

impl Mdns {
    pub fn start(state: &State, bind: Ipv4Addr) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        if bind.is_unspecified() {
            daemon.disable_interface(vec![IfKind::IPv6, IfKind::LoopbackV4])?;
        } else {
            daemon.disable_interface(IfKind::All)?;
            daemon.enable_interface(IfKind::Addr(bind.into()))?;
        }
        let events = daemon.browse(SERVICE)?;
        let service = Self {
            daemon,
            name: format!("{}.{SERVICE}", state.config.id),
            events,
            loopback: bind.is_loopback(),
        };
        service.advertise(state, bind)?;
        Ok(service)
    }

    pub fn advertise(&self, state: &State, bind: Ipv4Addr) -> Result<()> {
        let id = state.config.id.as_str();
        let hostname = format!("{id}.local.");
        let name = state.config.name();
        let version = wire::VERSION.to_string();
        let properties = [
            ("id", id),
            ("name", name.as_str()),
            ("type", "desktop"),
            ("protocol", version.as_str()),
        ];
        let address = if bind.is_unspecified() {
            String::new()
        } else {
            bind.to_string()
        };
        let info = ServiceInfo::new(
            SERVICE,
            id,
            &hostname,
            address,
            state.tcp_port,
            &properties[..],
        )?;
        let info = if bind.is_unspecified() {
            info.enable_addr_auto()
        } else {
            info
        };
        self.daemon.register(info)?;
        Ok(())
    }

    pub async fn next(&self) -> Vec<(String, SocketAddrV4, u32)> {
        while let Ok(event) = self.events.recv_async().await {
            let ServiceEvent::ServiceResolved(service) = event else {
                continue;
            };
            let id = service
                .get_property_val_str("id")
                .or_else(|| {
                    service
                        .get_fullname()
                        .strip_suffix("._kdeconnect._udp.local.")
                })
                .unwrap_or_default();
            let version = service
                .get_property_val_str("protocol")
                .and_then(|value| value.parse().ok())
                .unwrap_or(wire::VERSION);
            if !wire::valid_id(id)
                || !wire::PORTS.contains(&service.get_port())
                || version < wire::VERSION
            {
                continue;
            }
            return service
                .get_addresses_v4()
                .into_iter()
                .filter(|address| {
                    address.is_loopback() == self.loopback
                        && !address.is_multicast()
                        && !address.is_unspecified()
                        && !address.is_broadcast()
                })
                .map(|address| {
                    (
                        id.to_owned(),
                        SocketAddrV4::new(address, service.get_port()),
                        version,
                    )
                })
                .collect();
        }
        std::future::pending().await
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        if let Ok(done) = self.daemon.unregister(&self.name) {
            let _ = done.recv_timeout(std::time::Duration::from_millis(200));
        }
        let _ = self.daemon.shutdown();
    }
}
