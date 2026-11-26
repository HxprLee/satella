//! Probe: cold-cache test. Removes every cached device, then for each MTA peer
//! records when BlueZ first knows its Bluetooth name.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

    // Cold start: drop every cached device object.
    for addr in adapter.device_addresses().await? {
        let _ = adapter.remove_device(addr).await;
        println!("removed cached device {addr}");
    }
    adapter.set_discovery_filter(Default::default()).await?;

    let mut events = adapter.discover_devices().await?;
    let mut seen: HashMap<String, ()> = HashMap::new();
    let start = Instant::now();

    while let Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next().await {
        let Ok(device) = adapter.device(addr) else {
            continue;
        };
        let key = addr.to_string();
        if seen.insert(key.clone(), ()).is_some() {
            continue;
        }
        let t0 = start.elapsed();

        let uuids = device.uuids().await?.unwrap_or_default();
        let sd = device.service_data().await?.unwrap_or_default();
        if !uuids.iter().any(is_mta) && !sd.keys().any(is_mta) {
            continue;
        }

        let immediate = device.name().await?;
        println!(
            "\n== {key} MTA  seen@+{:.2}s  name_now={immediate:?}",
            t0.as_secs_f64()
        );

        for i in 0..150 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(Some(n)) = device.name().await {
                println!(
                    "   -> NAME at +{:.2}s (waited {}ms) = {n:?}",
                    start.elapsed().as_secs_f64(),
                    i * 200
                );
                break;
            }
            if i == 149 {
                println!("   -> NO NAME after 30s");
            }
        }
    }
    Ok(())
}
