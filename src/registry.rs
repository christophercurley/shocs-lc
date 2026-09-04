use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
pub struct BrightnessTransition {
    pub from: u16,
    pub to: u16,
    pub started: Instant,
    pub duration: Duration,
}

impl BrightnessTransition {
    pub fn projected_value(&self, now: Instant) -> u16 {
        if self.duration.is_zero() {
            return self.to;
        }

        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= self.duration {
            return self.to;
        }

        let progress = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        let from = f64::from(self.from);
        let to = f64::from(self.to);
        (from + (to - from) * progress)
            .round()
            .clamp(0.0, f64::from(u16::MAX)) as u16
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.duration
            .saturating_sub(now.saturating_duration_since(self.started))
    }

    pub fn is_active(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < self.duration
    }
}

#[derive(Debug, Clone)]
pub struct ManagedLight {
    pub device: LifxDevice,
    pub label: Option<String>,
    pub mode: LightMode,
    pub observed: Option<LightState>,

    /// Latest power state commanded by SHOCS. The web UI renders this while a
    /// physical observation is catching up, which prevents toggle bounce.
    pub desired_power: Option<Power>,

    /// Latest brightness target commanded by SHOCS.
    pub desired_brightness: Option<u16>,

    /// A known in-progress brightness transition. The UI can interpolate this
    /// locally instead of waiting for periodic physical state polls.
    pub brightness_transition: Option<BrightnessTransition>,

    /// Explicit manual power choice layered over an automation mode.
    ///
    /// For Test Mode this survives heartbeats/reconciliation until the next
    /// Test power-schedule boundary. Other properties still come from the mode.
    pub power_override: Option<Power>,
    pub last_discovered: Instant,
    pub last_observed: Option<Instant>,
}

impl ManagedLight {
    pub fn projected_brightness(&self, now: Instant) -> Option<u16> {
        if let Some(transition) = self
            .brightness_transition
            .as_ref()
            .filter(|transition| transition.is_active(now))
        {
            return Some(transition.projected_value(now));
        }

        self.desired_brightness
            .or_else(|| self.observed.map(|state| state.brightness))
    }

    fn transition_start_brightness(&self, now: Instant) -> Option<u16> {
        if let Some(transition) = self
            .brightness_transition
            .as_ref()
            .filter(|transition| transition.is_active(now))
        {
            return Some(transition.projected_value(now));
        }

        // When starting a new physical transition, the latest observation is
        // the best estimate of where the bulb is actually starting from.
        self.observed
            .map(|state| state.brightness)
            .or(self.desired_brightness)
    }
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

    pub async fn set_desired_power(
        &self,
        id: LifxId,
        power: Option<Power>,
    ) -> Option<Option<Power>> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let previous = light.desired_power;
        light.desired_power = power;
        Some(previous)
    }

    pub async fn set_desired_power_for_mode(&self, mode: LightMode, power: Power) {
        let mut lights = self.lights.write().await;
        for light in lights.values_mut().filter(|light| light.mode == mode) {
            light.desired_power = Some(power);
        }
    }

    pub async fn set_desired_brightness(&self, id: LifxId, brightness: u16) -> Option<u16> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let previous = light.desired_brightness.unwrap_or(brightness);
        light.desired_brightness = Some(brightness);
        light.brightness_transition = None;
        Some(previous)
    }

    pub async fn begin_brightness_transition(
        &self,
        id: LifxId,
        target: u16,
        duration: Duration,
        started: Instant,
    ) -> bool {
        let mut lights = self.lights.write().await;
        let Some(light) = lights.get_mut(&id) else {
            return false;
        };

        let from = light.transition_start_brightness(started).unwrap_or(target);
        light.desired_brightness = Some(target);
        light.brightness_transition = if duration.is_zero() || from == target {
            None
        } else {
            Some(BrightnessTransition {
                from,
                to: target,
                started,
                duration,
            })
        };

        true
    }

    pub async fn begin_brightness_transition_for_mode(
        &self,
        mode: LightMode,
        target: u16,
        duration: Duration,
        started: Instant,
    ) {
        let mut lights = self.lights.write().await;

        for light in lights.values_mut().filter(|light| light.mode == mode) {
            let from = light.transition_start_brightness(started).unwrap_or(target);
            light.desired_brightness = Some(target);
            light.brightness_transition = if duration.is_zero() || from == target {
                None
            } else {
                Some(BrightnessTransition {
                    from,
                    to: target,
                    started,
                    duration,
                })
            };
        }
    }

    pub async fn clear_brightness_transition(&self, id: LifxId) {
        if let Some(light) = self.lights.write().await.get_mut(&id) {
            light.brightness_transition = None;
        }
    }

    pub async fn set_power_override(
        &self,
        id: LifxId,
        power: Option<Power>,
    ) -> Option<Option<Power>> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let previous = light.power_override;
        light.power_override = power;
        Some(previous)
    }

    /// Clear manual power overrides for every light currently in `mode`.
    /// Returns the number of overrides removed.
    pub async fn clear_power_overrides_in_mode(&self, mode: LightMode) -> usize {
        let mut lights = self.lights.write().await;
        let mut cleared = 0usize;

        for light in lights.values_mut().filter(|light| light.mode == mode) {
            if light.power_override.take().is_some() {
                cleared += 1;
            }
        }

        cleared
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

    /// Return mode lights whose observed power differs from their effective
    /// desired power. A manual power override wins over the mode's default.
    pub async fn devices_with_power_mismatch(
        &self,
        mode: LightMode,
        mode_power: Power,
    ) -> Vec<(LifxDevice, Power)> {
        self.lights
            .read()
            .await
            .values()
            .filter_map(|light| {
                if light.mode != mode {
                    return None;
                }

                let desired = light.power_override.unwrap_or(mode_power);
                match light.observed {
                    Some(observed) if observed.power != desired => {
                        Some((light.device.clone(), desired))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// Record a physical observation. Returns Some(true) when the visible state
    /// changed, Some(false) when it is unchanged, or None for an unknown ID.
    pub async fn record_observation(&self, id: LifxId, observed: LightState) -> Option<bool> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let changed = light.observed != Some(observed);
        let now = Instant::now();

        light.observed = Some(observed);
        light.last_observed = Some(now);

        if light
            .brightness_transition
            .as_ref()
            .is_some_and(|transition| !transition.is_active(now))
        {
            light.brightness_transition = None;
        }

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
                    desired_power: None,
                    desired_brightness: None,
                    brightness_transition: None,
                    power_override: None,
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
