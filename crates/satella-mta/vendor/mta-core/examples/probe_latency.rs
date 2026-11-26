//! Probe: measures how long after first sighting BlueZ resolves a peer's
//! Bluetooth name, and whether the property-change event fires.

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
    let deadline = Instant::now() + Duration::from_secs(60);
    let start = Instant::now();
    let mut tracked: HashMap<String, ()> = HashMap::new();

    while Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_millis(50)) => {},
            Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next() => {
                let Ok(device) = adapter.device(addr) else { continue };
                let key = addr.to_string();
                if tracked.insert(key.clone(), ()).is_some() { continue; }

                let uuids = device.uuids().await?.unwrap_or_default();
                let sd = device.service_data().await?.unwrap_or_default();
                let mta = uuids.iter().any(is_mta) || sd.keys().any(is_mta);
                if !mta { continue; }

                let immediate = device.name().await?;
                println!("\n== {key} (MTA) immediate name = {immediate:?}");

                // Poll the name until it shows up.
                let mut t = start.elapsed();
                let mut got: Option<String> = None;
                while t < start.elapsed() + Duration::from_secs(15) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if let Ok(Some(n)) = device.name().await {
                        got = Some(n);
                        break;
                    }
                }
                t = start.elapsed();
                match &got {
                    Some(n) => println!("   resolved to {n:?} at +{:.2}s after start", t.as_secs_f64()),
                    None => println!("   NEVER resolved within 15s"),
                }
                let alias = device.alias().await?;
                println!("   alias = {alias:?}");
            }
            else => break,
        }
    }
    Ok(())
}
