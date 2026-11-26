use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Receiver;
use tokio::sync::watch;
use tokio::time::{interval_at, Instant};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use crate::utils::{gen_mdns_endpoint_info, gen_mdns_name, reachable_ipv4, DeviceType};

const INNER_NAME: &str = "MDnsServer";
const TICK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub enum Visibility {
    Visible = 0,
    Invisible = 1,
    Temporarily = 2,
}

#[allow(dead_code)]
impl Visibility {
    pub fn from_raw_value(value: u64) -> Self {
        match value {
            0 => Visibility::Visible,
            1 => Visibility::Invisible,
            2 => Visibility::Temporarily,
            _ => unreachable!(),
        }
    }
}

pub struct MDnsServer {
    daemon: ServiceDaemon,
    service_info: ServiceInfo,
    endpoint_id: [u8; 4],
    service_port: u16,
    ble_receiver: Receiver<()>,
    visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
    visibility_receiver: watch::Receiver<Visibility>,
}

impl MDnsServer {
    pub fn new(
        endpoint_id: [u8; 4],
        service_port: u16,
        ble_receiver: Receiver<()>,
        visibility_sender: Arc<Mutex<watch::Sender<Visibility>>>,
        visibility_receiver: watch::Receiver<Visibility>,
    ) -> Result<Self, anyhow::Error> {
        let service_info = Self::build_service(endpoint_id, service_port, DeviceType::Laptop)?;

        Ok(Self {
            daemon: ServiceDaemon::new()?,
            service_info,
            endpoint_id,
            service_port,
            ble_receiver,
            visibility_sender,
            visibility_receiver,
        })
    }

    /// The service to advertise, with the address to advertise.
    ///
    /// Returns `None` when the service already names the address to
    /// advertise.
    fn service_for_the_current_address(
        advertised: &HashSet<Ipv4Addr>,
        endpoint_id: [u8; 4],
        service_port: u16,
    ) -> Result<Option<ServiceInfo>, anyhow::Error> {
        let current = reachable_ipv4();

        match current {
            // The address to advertise is already the one the service names.
            Some(address) if advertised.contains(&address) => Ok(None),
            // There is no address to advertise and the service names none
            // either: nothing to do.
            None if advertised.is_empty() => Ok(None),
            _ => Ok(Some(Self::build_service(
                endpoint_id,
                service_port,
                DeviceType::Laptop,
            )?)),
        }
    }

    pub async fn run(&mut self, ctk: CancellationToken) -> Result<(), anyhow::Error> {
        info!("{INNER_NAME}: service starting");
        let monitor = self.daemon.monitor()?;
        let endpoint_id = self.endpoint_id;
        let service_port = self.service_port;
        // The addresses the service names, so the loop can tell whether the
        // address to advertise changed without borrowing the service.
        let mut advertised: HashSet<Ipv4Addr> = self
            .service_info
            .get_addresses_v4()
            .into_iter()
            .copied()
            .collect();
        let ble_receiver = &mut self.ble_receiver;
        let mut visibility = *self.visibility_receiver.borrow();
        let mut interval = interval_at(Instant::now() + TICK_INTERVAL, TICK_INTERVAL);
        // The service to advertise, rebuilt when the address to advertise
        // changes. `None` means the service already names it.
        let mut refreshed = None;

        loop {
            tokio::select! {
                _ = ctk.cancelled() => {
                    info!("{INNER_NAME}: tracker cancelled, breaking");
                    break;
                }
                r = monitor.recv_async() => {
                    match r {
                        Ok(_) => continue,
                        Err(err) => return Err(err.into()),
                    }
                },
                _ = self.visibility_receiver.changed() => {
                    visibility = *self.visibility_receiver.borrow_and_update();

                    debug!("{INNER_NAME}: visibility changed: {visibility:?}");
                    if visibility == Visibility::Visible {
                        self.daemon.register(self.service_info.clone())?;
                    } else if visibility == Visibility::Invisible {
                        let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
                        let _ = receiver.recv();
                    } else if visibility == Visibility::Temporarily {
                        self.daemon.register(self.service_info.clone())?;
                        interval.reset();
                    }
                }
                _ = ble_receiver.recv() => {
                    if visibility == Visibility::Invisible {
                        continue;
                    }

                    debug!("{INNER_NAME}: ble_receiver: got event");

                    // The network may have come up, or changed, since the
                    // service was built.
                    refreshed = Self::service_for_the_current_address(
                        &advertised,
                        endpoint_id,
                        service_port,
                    )?;

                    if visibility == Visibility::Visible || visibility == Visibility::Temporarily {
                        // Android can sometime not see the mDNS service if the service
                        // was running BEFORE Android started the Discovery phase for QuickShare.
                        // So resend a broadcast if there's a android device sending.
                        self.daemon.register_resend(self.service_info.get_fullname())?;
                    } else {
                        self.daemon.register(self.service_info.clone())?;
                    }
                },
                _ = interval.tick() => {
                    // The network may have come up, or changed, since the
                    // service was built.
                    refreshed = Self::service_for_the_current_address(
                        &advertised,
                        endpoint_id,
                        service_port,
                    )?;

                    if visibility != Visibility::Temporarily {
                        continue;
                    }

                    let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
                    let _ = receiver.recv();
                    let _ = self.visibility_sender.lock().unwrap().send(Visibility::Invisible);
                }
            }

            // A new address to advertise: build the service with it and
            // register that one instead of the stale one.
            if let Some(service_info) = refreshed.take() {
                advertised = service_info
                    .get_addresses_v4()
                    .into_iter()
                    .copied()
                    .collect();
                self.daemon.register(service_info.clone())?;
                self.service_info = service_info;
            }
        }

        // Unregister the mDNS service - we're shutting down
        let receiver = self.daemon.unregister(self.service_info.get_fullname())?;
        if let Ok(event) = receiver.recv() {
            info!("MDnsServer: service unregistered: {:?}", &event);
        }

        Ok(())
    }

    fn build_service(
        endpoint_id: [u8; 4],
        service_port: u16,
        device_type: DeviceType,
    ) -> Result<ServiceInfo, anyhow::Error> {
        let name = gen_mdns_name(endpoint_id);
        let hostname = sys_metrics::host::get_hostname()?;
        info!("Broadcasting with: {hostname}");
        let endpoint_info = gen_mdns_endpoint_info(device_type as u8, &hostname);

        // The advertisement must name exactly one address. The daemon answers
        // the queries on every address this host has, so an address a peer
        // cannot route to (a Wi-Fi Direct group owner, a VPN, a Docker bridge)
        // becomes a second, competing answer for the same service.
        //
        // `enable_addr_auto` has to stay off: it makes the daemon sweep in
        // every address of the host, and it puts a new address back into the
        // service within 30 seconds of it appearing.
        //
        // A host with no IPv4 at all yet (the network is still coming up) gets
        // an empty address set rather than no service: the daemon has to come
        // up anyway, and the service is registered again when the visibility
        // or the BLE signal changes.
        let address = match reachable_ipv4() {
            Some(address) => {
                info!("Broadcasting address: {address}");
                address.to_string()
            }
            None => {
                warn!("No IPv4 address to broadcast, the service names none");
                String::new()
            }
        };

        let properties = [("n", endpoint_info), ("IPv4", address.clone())];
        let si = ServiceInfo::new(
            "_FC9F5ED42C8A._tcp.local.",
            &name,
            &hostname,
            address,
            service_port,
            &properties[..],
        )?;

        Ok(si)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertisement_names_exactly_one_address() {
        // The daemon answers the queries on every address the host has, so a
        // second address in the service is a second, competing answer for it.
        let si = MDnsServer::build_service([1, 2, 3, 4], 1234, DeviceType::Laptop).unwrap();

        assert_eq!(si.get_addresses().len(), 1);
        assert!(
            !si.is_addr_auto(),
            "addr_auto sweeps every address of the host into the service"
        );

        let address = *si.get_addresses().iter().next().unwrap();
        assert!(address.is_ipv4());
    }
}
