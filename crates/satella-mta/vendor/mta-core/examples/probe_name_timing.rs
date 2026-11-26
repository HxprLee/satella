//! Probe: measures how long after first sighting BlueZ resolves a peer's
//! Bluetooth name. Distinguishes "never resolvable" from "resolvable, but
//! only later".

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bluer::{Adapter, Session};
use futures_util::StreamExt;

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
            _ = tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_millis(100)) => {},
            Some(bluer::AdapterEvent::DeviceAdded(addr)) = events.next() => {
                let Ok(device) = adapter.device(addr) else { continue };
                let key = addr.to_string();
                if tracked.insert(key.clone(), ()).is_some() { continue; }

                // Read the name exactly once, right at first sighting.
                let immediate = device.name().await?;

                // Then watch the property for changes.
                let mut changes = device.events().await?;
                let mut resolved_at: Option<f64> = None;
                let watch_deadline = Instant::now() + Duration::from_secs(12);

                while Instant::now() < watch_deadline {
                    tokio::select! {
                        _ = tokio::time::sleep_until(tokio::time::Instant::now() + Duration::from_millis(100)) => {},
                        Some(ev) = changes.next() => {
                            if let bluer::DeviceEvent::PropertyChanged(
                                bluer::DeviceProperty::Name(ref n),
                            ) = ev {
                                resolved_at = Some(start.elapsed().as_secs_f64());
                                println!("[{:>6.2}s] {key} NAME EVENT -> {n:?}", resolved_at.unwrap());
                            }
                        }
                        else => break,
                    }
                }

                let alias = device.alias().await?;
                println!("{key}: immediate_name={immediate:?} resolved_at={resolved_at:?} alias={alias:?}");
            }
            else => break,
        }
    }
    Ok(())
}
