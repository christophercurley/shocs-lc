use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use lifx::{LifxClient, LifxDevice, LifxId, LightState, Power};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightMode {
    Test,
    Custom,
}

impl LightMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManagedLight {
    pub device: LifxDevice,
    pub label: Option<String>,
    pub mode: LightMode,
    pub observed: Option<LightState>,
    pub last_discovered: Instant,
    pub last_observed: Option<Instant>,
}

/// Shared in-memory controller state.
///
/// Web/API code talks to this abstraction instead of reaching into the
/// underlying map directly. A persistent store can replace or back this later
/// without coupling the rest of the controller to SQLite today.
#[derive(Clone)]
pub struct ControllerState {
    lights: Arc<RwLock<HashMap<LifxId, ManagedLight>>>,
    initial_test_ids: Arc<HashSet<LifxId>>,
}

impl ControllerState {
    pub fn new(initial_test_ids: &[LifxId]) -> Self {
        Self {
            lights: Arc::new(RwLock::new(HashMap::new())),
            initial_test_ids: Arc::new(initial_test_ids.iter().copied().collect()),
        }
    }

    fn initial_mode(&self, id: LifxId) -> LightMode {
        if self.initial_test_ids.contains(&id) {
            LightMode::Test
        } else {
            LightMode::Custom
        }
    }

    pub async fn lights(&self) -> Vec<ManagedLight> {
        self.lights.read().await.values().cloned().collect()
    }

    pub async fn light(&self, id: LifxId) -> Option<ManagedLight> {
        self.lights.read().await.get(&id).cloned()
    }

    pub async fn set_mode(&self, id: LifxId, mode: LightMode) -> Option<LightMode> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let previous = light.mode;
        light.mode = mode;
        Some(previous)
    }

    pub async fn known_devices(&self) -> Vec<LifxDevice> {
        self.lights
            .read()
            .await
            .values()
            .map(|light| light.device.clone())
            .collect()
    }

    pub async fn devices_in_mode(&self, mode: LightMode) -> Vec<LifxDevice> {
        self.lights
            .read()
            .await
            .values()
            .filter(|light| light.mode == mode)
            .map(|light| light.device.clone())
            .collect()
    }

    pub async fn devices_with_power_mismatch(
        &self,
        mode: LightMode,
        desired: Power,
    ) -> Vec<LifxDevice> {
        self.lights
            .read()
            .await
            .values()
            .filter(|light| {
                light.mode == mode
                    && matches!(light.observed, Some(observed) if observed.power != desired)
            })
            .map(|light| light.device.clone())
            .collect()
    }

    /// Record a physical observation. Returns Some(true) when the visible state
    /// changed, Some(false) when it is unchanged, or None for an unknown ID.
    pub async fn record_observation(&self, id: LifxId, observed: LightState) -> Option<bool> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let changed = light.observed != Some(observed);

        light.observed = Some(observed);
        light.last_observed = Some(Instant::now());

        Some(changed)
    }

    async fn known_count(&self) -> usize {
        self.lights.read().await.len()
    }
}

pub async fn refresh_registry(client: &LifxClient, state: &ControllerState) -> lifx::Result<usize> {
    let discovered = client.discover().await?;

    for device in discovered.values() {
        let existing = {
            let lights = state.lights.read().await;
            lights.get(&device.id).cloned()
        };

        // Labels are relatively static metadata. Fetch them for new devices,
        // and retry later if an earlier lookup failed.
        let label = match existing.as_ref().and_then(|light| light.label.clone()) {
            Some(label) => Some(label),
            None => match client.get_label(device).await {
                Ok(label) => Some(label),
                Err(err) => {
                    warn!(
                        lifx_id = %format!("{:#018x}", device.id),
                        address = %device.addr,
                        error = %err,
                        "could not read LIFX label"
                    );
                    None
                }
            },
        };

        let now = Instant::now();
        let mut lights = state.lights.write().await;

        match lights.entry(device.id) {
            Entry::Vacant(entry) => {
                let mode = state.initial_mode(device.id);
                let label_for_log = label.as_deref().unwrap_or("<unknown>");

                info!(
                    lifx_id = %format!("{:#018x}", device.id),
                    address = %device.addr,
                    label = %label_for_log,
                    mode = ?mode,
                    "discovered new LIFX device"
                );

                entry.insert(ManagedLight {
                    device: device.clone(),
                    label,
                    mode,
                    observed: None,
                    last_discovered: now,
                    last_observed: None,
                });
            }
            Entry::Occupied(mut entry) => {
                let light = entry.get_mut();

                if light.device.addr != device.addr {
                    info!(
                        lifx_id = %format!("{:#018x}", device.id),
                        old_address = %light.device.addr,
                        new_address = %device.addr,
                        "LIFX address changed"
                    );
                }

                light.device = device.clone();
                light.last_discovered = now;

                if light.label.is_none() {
                    light.label = label;
                }
            }
        }
    }

    let discovered_count = discovered.len();
    let known_count = state.known_count().await;

    debug!(
        discovered = discovered_count,
        known = known_count,
        "LIFX discovery complete"
    );

    Ok(discovered_count)
}
