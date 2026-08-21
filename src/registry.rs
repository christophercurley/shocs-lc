use std::collections::HashMap;
use std::sync::Arc;

use lifx::{LifxClient, LifxDevice, LifxId};
use tokio::sync::RwLock;

pub type DeviceRegistry = Arc<RwLock<HashMap<LifxId, LifxDevice>>>;

pub fn new_registry() -> DeviceRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

pub async fn refresh_registry(
    client: &LifxClient,
    registry: &DeviceRegistry,
) -> lifx::Result<usize> {
    let discovered = client.discover().await?;
    let previous = registry.read().await.clone();

    for device in discovered.values() {
        match previous.get(&device.id) {
            None => match client.get_label(device).await {
                Ok(label) => println!(
                    "Discovered new LIFX device: {:#018x} -> {} [{}]",
                    device.id, device.addr, label
                ),
                Err(err) => println!(
                    "Discovered new LIFX device: {:#018x} -> {} [label unavailable: {}]",
                    device.id, device.addr, err
                ),
            },
            Some(old) if old.addr != device.addr => {
                println!(
                    "LIFX address changed: {:#018x} {} -> {}",
                    device.id, old.addr, device.addr
                );
            }
            Some(_) => {}
        }
    }

    let discovered_count = discovered.len();

    // Merge instead of replacing so one missed UDP discovery response does
    // not immediately erase a known device from the runtime registry.
    let mut current = registry.write().await;
    for (id, device) in discovered {
        current.insert(id, device);
    }

    Ok(discovered_count)
}

pub async fn resolve_devices(registry: &DeviceRegistry, ids: &[LifxId]) -> Vec<LifxDevice> {
    let registry = registry.read().await;

    ids.iter()
        .filter_map(|id| registry.get(id).cloned())
        .collect()
}
