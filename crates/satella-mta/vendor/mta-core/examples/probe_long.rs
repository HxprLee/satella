//! Probe: cold-cache test over a long window. Removes every cached device,
//! then for each MTA peer records when BlueZ first learns its Bluetooth name
//! and what the MTA service data carries meanwhile.

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

/// The CatShare/OnePlus device name: service data 0x6666 is
/// `len || utf8-name || padding || 6 trailing bytes`.
fn name_from_6666(data: &[u8]) -> Option<String> {
    let len = *data.first()? as usize;
    if len == 0 || len + 7 > data.len() {
        return None;
    }
    let raw = &data[1..1 + len];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let s = std::str::from_utf8(&raw[..end]).ok()?;
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let session = Session::new().await?;
    let adapter: Adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    for addr in adapter.device_addresses().await? {
        let _ = adapter.remove_device(addr).await;
    }
    println!("cache cleared");
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

        let mut svc_names: Vec<String> = Vec::new();
        for (u, d) in &sd {
            if u.to_string().starts_with("00006666") {
                if let Some(n) = name_from_6666(d) {
                    svc_names.push(n);
                }
            }
        }
        println!(
            "\n== {key} MTA seen@+{:.1}s  svc6666_names={svc_names:?}",
            t0.as_secs_f64()
        );

        // Watch for up to 150s: when does BlueZ learn the name?
        for i in 0..750 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Ok(Some(n)) = device.name().await {
                println!(
                    "   BLUEZ NAME at +{:.1}s (waited {}ms) = {n:?}",
                    start.elapsed().as_secs_f64(),
                    i * 200
                );
                break;
            }
            if i == 749 {
                println!("   BLUEZ NAME never appeared in 150s");
            }
        }
    }
    Ok(())
}
