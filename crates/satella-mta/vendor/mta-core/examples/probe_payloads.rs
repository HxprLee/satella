//! Probe: correlates the MTA service-data payloads with the name BlueZ knows,
//! to find which field carries the peer's human-readable device name.

use std::collections::HashMap;

use bluer::{Adapter, Session};
use futures_util::StreamExt;
use uuid::Uuid;

const BASE: [u8; 12] = [0, 0, 0x10, 0, 0x80, 0, 0, 0x80, 0x5f, 0x9b, 0x34, 0xfb];

fn is_mta(u: &Uuid) -> bool {
    let b = u.as_bytes();
    b[4..] == BASE && b[0] == 0 && b[1] == 0 && b[2] == 0x33 && matches!(b[3], 0x31..=0x34)
}

fn printable(b: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(b);
    let trimmed = s.trim_end_matches('\0');
    if trimmed.is_empty() {
        return None;
    }
    // Only accept something that looks like a name.
    if trimmed.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Some(trimmed.to_string());
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let session = Session::new().await?;
    let adapter: Adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    adapter.set_discovery_filter(Default::default()).await?;

    let mut events = adapter.discover_devices().await?;
    let mut seen: HashMap<String, ()> = HashMap::new();

    while let Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next().await {
        let Ok(device) = adapter.device(addr) else {
            continue;
        };
        let key = addr.to_string();
        if seen.insert(key.clone(), ()).is_some() {
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

        let uuids = device.uuids().await?.unwrap_or_default();
        let sd = device.service_data().await?.unwrap_or_default();
        if !uuids.iter().any(is_mta) && !sd.keys().any(is_mta) {
            continue;
        }

        let name = device.name().await?;
        let alias = device.alias().await?;

        println!("### {key}");
        println!("   bluez name = {name:?}   alias = {alias:?}");
        let mut keys: Vec<_> = sd.keys().collect();
        keys.sort();
        for u in keys {
            let d = &sd[u];
            let short = &u.to_string()[..8];
            // Show the first byte and any printable run, for every payload.
            let mut runs: Vec<String> = Vec::new();
            let mut cur = Vec::new();
            for &b in d {
                if b == 0 {
                    if let Some(s) = printable(&cur) {
                        runs.push(s);
                    }
                    cur.clear();
                } else {
                    cur.push(b);
                }
            }
            if let Some(s) = printable(&cur) {
                runs.push(s);
            }
            println!(
                "   {short} len={:<3} b0={:<3} runs={:?}",
                d.len(),
                d.first().copied().unwrap_or(0),
                runs
            );
        }
        println!();
    }
    Ok(())
}
