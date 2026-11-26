//! Probe: tracks one MTA device over time and records when its Bluetooth name
//! becomes visible. Distinguishes a timing race from a stable absence.

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
    let deadline = Instant::now() + Duration::from_secs(45);
    let start = Instant::now();
    // address -> (first seen offset, name at first sight, later name)
    let mut tracked: HashMap<String, (Duration, Option<String>, Option<String>)> = HashMap::new();

    while Instant::now() < deadline {
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_millis(300)) => {},
            Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next() => {
                let Ok(device) = adapter.device(addr) else { continue };

                // Poll this device for a while, to see when the name shows up.
                let mut seen_name: Option<String> = None;
                for _ in 0..20 {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let Ok(Some(name)) = device.name().await else { continue };
                    seen_name = Some(name);
                    break;
                }
                let Some(name) = seen_name else { continue };

                // Only care about devices that look like MTA peers.
                let uuids = device.uuids().await?.unwrap_or_default();
                let sd = device.service_data().await?.unwrap_or_default();
                if !uuids.iter().any(is_mta) && !sd.keys().any(is_mta) { continue; }

                let key = addr.to_string();
                let off = start.elapsed();
                let entry = tracked.entry(key.clone()).or_insert((off, None, None));
                if entry.1.is_none() {
                    entry.1 = Some(name.clone());
                    println!("[{:>7.2}s] FIRST  {key}  name={name:?}", off.as_secs_f64());
                } else if entry.2.as_deref() != Some(name.as_str()) {
                    entry.2 = Some(name.clone());
                    println!("[{:>7.2}s] CHANGE {key}  name={name:?}", off.as_secs_f64());
                }
            }
            else => break,
        }
    }

    println!("\n=== summary ===");
    let mut keys: Vec<_> = tracked.keys().collect();
    keys.sort();
    for k in keys {
        let (off, first, later) = &tracked[k];
        println!("{k}  first_seen={off:?}  first_name={first:?}  later_name={later:?}");
    }
    Ok(())
}
