//! Probe: for each MTA peer, measures the delay between first sighting and
//! BlueZ resolving the peer's Bluetooth name.

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
            "\n== {key} (MTA) first seen at +{:.2}s, immediate name = {immediate:?}",
            t0.as_secs_f64()
        );

        let mut resolved: Option<(f64, String)> = None;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(Some(n)) = device.name().await {
                resolved = Some((start.elapsed().as_secs_f64(), n));
                break;
            }
        }
        match resolved {
            Some((t, n)) => println!("   NAME RESOLVED at +{t:.2}s -> {n:?}",),
            None => println!("   NAME NEVER RESOLVED in 30s"),
        }
        let alias = device.alias().await?;
        println!("   alias = {alias:?}");
    }
    Ok(())
}
