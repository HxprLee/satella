//! Probe: dumps every BlueZ property of MTA peers, to find where the peer's
//! Bluetooth name lives.

use std::collections::{HashMap, HashSet};

use bluer::{Adapter, Session};
use futures_util::StreamExt;
use uuid::Uuid;

const BASE: [u8; 12] = [0, 0, 0x10, 0, 0x80, 0, 0, 0x80, 0x5f, 0x9b, 0x34, 0xfb];

fn is_mta(u: &Uuid) -> bool {
    let b = u.as_bytes();
    b[4..] == BASE && b[0] == 0 && b[1] == 0 && b[2] == 0x33 && matches!(b[3], 0x31..=0x34)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let session = Session::new().await?;
    let adapter: Adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    adapter.set_discovery_filter(Default::default()).await?;

    let mut events = adapter.discover_devices().await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    let mut seen: HashSet<String> = HashSet::new();

    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next() => {
                let Ok(device) = adapter.device(addr) else { continue };
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let uuids: HashSet<Uuid> = device.uuids().await?.unwrap_or_default();
                let sd: HashMap<Uuid, Vec<u8>> = device.service_data().await?.unwrap_or_default();
                let md: HashMap<u16, Vec<u8>> = device.manufacturer_data().await?.unwrap_or_default();

                let mta = uuids.iter().any(is_mta) || sd.keys().any(is_mta);
                if !mta { continue; }
                if !seen.insert(addr.to_string()) { continue; }

                println!("### {addr}");
                println!("  name  = {:?}", device.name().await?);
                println!("  alias = {:?}", device.alias().await?);
                println!("  uuids = {}", uuids.len());
                for (u, d) in &sd {
                    println!("  svc {u} len={}", d.len());
                    println!("      hex = {}", d.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
                    println!("      txt = {:?}", String::from_utf8_lossy(d));
                }
                for (k, d) in &md {
                    println!("  manuf 0x{k:04x} len={}", d.len());
                    println!("      hex = {}", d.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
                    println!("      txt = {:?}", String::from_utf8_lossy(d));
                }
                println!();
            }
            else => break,
        }
    }

    Ok(())
}
