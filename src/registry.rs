use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lifx::{Color, LifxClient, LifxDevice, LifxId, LightState, Power};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::store::{PostgresStore, StoreError, StoredGroup, StoredLight};

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

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "test" => Some(Self::Test),
            "custom" => Some(Self::Custom),
            _ => None,
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

/// Color intent is tracked separately from brightness because SHOCS exposes
/// brightness as an independent live control even though LIFX carries it in HSBK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTarget {
    pub hue: u16,
    pub saturation: u16,
    pub kelvin: u16,
}

impl From<Color> for ColorTarget {
    fn from(color: Color) -> Self {
        Self {
            hue: color.hue,
            saturation: color.saturation,
            kelvin: color.kelvin,
        }
    }
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
    pub friendly_name: Option<String>,
    pub control_enabled: bool,
    pub mode: LightMode,
    pub observed: Option<LightState>,

    /// Latest power state commanded by SHOCS. The web UI renders this while a
    /// physical observation is catching up, which prevents toggle bounce.
    pub desired_power: Option<Power>,

    /// Latest color target commanded by SHOCS. Keeping this separate from
    /// observed state prevents the picker/swatches from bouncing while the
    /// physical bulb is still catching up.
    pub desired_color: Option<ColorTarget>,

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

#[derive(Debug, Clone)]
pub struct LightGroup {
    pub id: i64,
    pub name: String,
    pub member_ids: Vec<LifxId>,
}

impl From<StoredGroup> for LightGroup {
    fn from(group: StoredGroup) -> Self {
        Self {
            id: group.id,
            name: group.name,
            member_ids: group.member_ids,
        }
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
    persisted_lights: Arc<RwLock<HashMap<LifxId, StoredLight>>>,
    groups: Arc<RwLock<HashMap<i64, LightGroup>>>,
    initial_test_ids: Arc<HashSet<LifxId>>,
    store: Arc<PostgresStore>,
}

impl ControllerState {
    pub fn new(
        initial_test_ids: &[LifxId],
        persisted_lights: Vec<StoredLight>,
        persisted_groups: Vec<StoredGroup>,
        store: Arc<PostgresStore>,
    ) -> Self {
        let persisted_lights = persisted_lights
            .into_iter()
            .map(|light| (light.id, light))
            .collect();

        let groups = persisted_groups
            .into_iter()
            .map(|group| {
                let group = LightGroup::from(group);
                (group.id, group)
            })
            .collect();

        Self {
            lights: Arc::new(RwLock::new(HashMap::new())),
            persisted_lights: Arc::new(RwLock::new(persisted_lights)),
            groups: Arc::new(RwLock::new(groups)),
            initial_test_ids: Arc::new(initial_test_ids.iter().copied().collect()),
            store,
        }
    }

    fn bootstrap_mode(&self, id: LifxId) -> LightMode {
        if self.initial_test_ids.contains(&id) {
            LightMode::Test
        } else {
            LightMode::Custom
        }
    }

    async fn ensure_persisted_light(
        &self,
        id: LifxId,
        device_label: Option<&str>,
    ) -> Result<StoredLight, StoreError> {
        let existing = self.persisted_lights.read().await.get(&id).cloned();

        if let Some(existing) = existing {
            let label_changed =
                device_label.is_some() && existing.device_label.as_deref() != device_label;

            if !label_changed {
                return Ok(existing);
            }

            let refreshed = self
                .store
                .upsert_discovered_light(id, device_label, existing.mode)
                .await?;
            self.persisted_lights
                .write()
                .await
                .insert(id, refreshed.clone());
            return Ok(refreshed);
        }

        let stored = self
            .store
            .upsert_discovered_light(id, device_label, self.bootstrap_mode(id))
            .await?;
        self.persisted_lights
            .write()
            .await
            .insert(id, stored.clone());
        Ok(stored)
    }

    pub async fn lights(&self) -> Vec<ManagedLight> {
        self.lights.read().await.values().cloned().collect()
    }

    pub async fn configured_lights(&self) -> Vec<StoredLight> {
        self.persisted_lights
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    pub async fn groups(&self) -> Vec<LightGroup> {
        self.groups.read().await.values().cloned().collect()
    }

    pub async fn group(&self, id: i64) -> Option<LightGroup> {
        self.groups.read().await.get(&id).cloned()
    }

    pub async fn create_group(&self, name: String) -> Result<LightGroup, StoreError> {
        let stored = self.store.create_group(&name).await?;
        let group = LightGroup::from(stored);
        self.groups.write().await.insert(group.id, group.clone());
        Ok(group)
    }

    pub async fn rename_group(&self, id: i64, name: String) -> Result<(), StoreError> {
        self.store.rename_group(id, &name).await?;

        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&id).ok_or(StoreError::UnknownGroup(id))?;
        group.name = name;
        Ok(())
    }

    pub async fn delete_group(&self, id: i64) -> Result<(), StoreError> {
        self.store.delete_group(id).await?;
        self.groups.write().await.remove(&id);
        Ok(())
    }

    pub async fn set_group_members(
        &self,
        id: i64,
        mut member_ids: Vec<LifxId>,
    ) -> Result<(), StoreError> {
        member_ids.sort_unstable();
        member_ids.dedup();

        {
            let configured = self.persisted_lights.read().await;
            if let Some(unknown) = member_ids.iter().find(|id| !configured.contains_key(id)) {
                return Err(StoreError::UnknownLight(*unknown));
            }
        }

        self.store.set_group_members(id, &member_ids).await?;

        let mut groups = self.groups.write().await;
        let group = groups.get_mut(&id).ok_or(StoreError::UnknownGroup(id))?;
        group.member_ids = member_ids;
        Ok(())
    }

    /// Resolve the runtime lights currently belonging to a group. Persisted
    /// membership survives while bulbs are offline; only currently known runtime
    /// devices are returned for immediate control.
    pub async fn lights_in_group(&self, id: i64) -> Option<Vec<ManagedLight>> {
        let group = self.groups.read().await.get(&id).cloned()?;
        let lights = self.lights.read().await;

        Some(
            group
                .member_ids
                .iter()
                .filter_map(|member_id| lights.get(member_id).cloned())
                .collect(),
        )
    }

    pub async fn light(&self, id: LifxId) -> Option<ManagedLight> {
        self.lights.read().await.get(&id).cloned()
    }

    pub async fn set_mode(
        &self,
        id: LifxId,
        mode: LightMode,
    ) -> Result<Option<LightMode>, StoreError> {
        let previous = self.lights.read().await.get(&id).map(|light| light.mode);
        let Some(previous) = previous else {
            return Ok(None);
        };

        if previous == mode {
            return Ok(Some(previous));
        }

        // Persist first. If PostgreSQL is temporarily unavailable, the web/API
        // operation fails cleanly and in-memory configuration remains unchanged.
        self.store.set_light_mode(id, mode).await?;

        if let Some(light) = self.lights.write().await.get_mut(&id) {
            light.mode = mode;
        }
        if let Some(light) = self.persisted_lights.write().await.get_mut(&id) {
            light.mode = mode;
        }

        Ok(Some(previous))
    }

    pub async fn set_friendly_name(
        &self,
        id: LifxId,
        friendly_name: Option<String>,
    ) -> Result<Option<String>, StoreError> {
        let previous = self
            .persisted_lights
            .read()
            .await
            .get(&id)
            .and_then(|light| light.friendly_name.clone());

        self.store
            .set_friendly_name(id, friendly_name.as_deref())
            .await?;

        if let Some(light) = self.persisted_lights.write().await.get_mut(&id) {
            light.friendly_name = friendly_name.clone();
        }
        if let Some(light) = self.lights.write().await.get_mut(&id) {
            light.friendly_name = friendly_name;
        }

        Ok(previous)
    }

    pub async fn set_control_enabled(
        &self,
        id: LifxId,
        control_enabled: bool,
    ) -> Result<Option<bool>, StoreError> {
        let previous = self
            .persisted_lights
            .read()
            .await
            .get(&id)
            .map(|light| light.control_enabled);

        self.store.set_control_enabled(id, control_enabled).await?;

        if let Some(light) = self.persisted_lights.write().await.get_mut(&id) {
            light.control_enabled = control_enabled;
        }
        if let Some(light) = self.lights.write().await.get_mut(&id) {
            light.control_enabled = control_enabled;
            if !control_enabled {
                light.power_override = None;
            }
        }

        Ok(previous)
    }

    pub async fn record_device_label(
        &self,
        id: LifxId,
        device_label: Option<String>,
    ) -> Result<(), StoreError> {
        self.store
            .set_device_label(id, device_label.as_deref())
            .await?;

        if let Some(light) = self.persisted_lights.write().await.get_mut(&id) {
            light.device_label = device_label.clone();
        }
        if let Some(light) = self.lights.write().await.get_mut(&id) {
            light.label = device_label;
        }

        Ok(())
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
        for light in lights
            .values_mut()
            .filter(|light| light.mode == mode && light.control_enabled)
        {
            light.desired_power = Some(power);
        }
    }

    pub async fn set_desired_color(&self, id: LifxId, color: Color) -> Option<ColorTarget> {
        let mut lights = self.lights.write().await;
        let light = lights.get_mut(&id)?;
        let previous = light.desired_color;
        light.desired_color = Some(color.into());
        previous
    }

    pub async fn set_desired_color_for_mode(&self, mode: LightMode, color: Color) {
        let mut lights = self.lights.write().await;
        let target = ColorTarget::from(color);

        for light in lights
            .values_mut()
            .filter(|light| light.mode == mode && light.control_enabled)
        {
            light.desired_color = Some(target);
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

        for light in lights
            .values_mut()
            .filter(|light| light.mode == mode && light.control_enabled)
        {
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

        for light in lights
            .values_mut()
            .filter(|light| light.mode == mode && light.control_enabled)
        {
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
            .filter(|light| light.mode == mode && light.control_enabled)
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
                if light.mode != mode || !light.control_enabled {
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
        // PH3-A1 intentionally reconciles labels on the normal discovery
        // cadence. This keeps out-of-band LIFX-app renames observable and lets
        // SHOCS re-assert a configured friendly name after a bulb reconnects.
        // We can split metadata onto a slower cadence later if scale warrants it.
        let mut physical_label = match client.get_label(device).await {
            Ok(label) => Some(label),
            Err(err) => {
                warn!(
                    lifx_id = %format!("{:#018x}", device.id),
                    address = %device.addr,
                    error = %err,
                    "could not read LIFX label"
                );

                state
                    .lights
                    .read()
                    .await
                    .get(&device.id)
                    .and_then(|light| light.label.clone())
            }
        };

        let mut stored = match state
            .ensure_persisted_light(device.id, physical_label.as_deref())
            .await
        {
            Ok(stored) => stored,
            Err(err) => {
                warn!(
                    lifx_id = %format!("{:#018x}", device.id),
                    error = %err,
                    "could not persist discovered LIFX device; will retry on a later discovery"
                );
                continue;
            }
        };

        // A SHOCS friendly name is durable desired metadata. When control is
        // enabled, mirror it back to the bulb so other LIFX clients see the same
        // name. If the bulb is temporarily unreachable, discovery retries later.
        if stored.control_enabled {
            if let Some(friendly_name) = stored.friendly_name.as_deref() {
                if physical_label.as_deref() != Some(friendly_name) {
                    match client.set_label(device, friendly_name).await {
                        Ok(confirmed) => {
                            info!(
                                lifx_id = %format!("{:#018x}", device.id),
                                label = %confirmed,
                                "reconciled physical LIFX label to SHOCS friendly name"
                            );
                            physical_label = Some(confirmed.clone());

                            if let Err(err) = state
                                .record_device_label(device.id, Some(confirmed.clone()))
                                .await
                            {
                                warn!(
                                    lifx_id = %format!("{:#018x}", device.id),
                                    error = %err,
                                    "physical label changed but database refresh failed"
                                );
                            } else {
                                stored.device_label = Some(confirmed);
                            }
                        }
                        Err(err) => warn!(
                            lifx_id = %format!("{:#018x}", device.id),
                            desired_label = %friendly_name,
                            error = %err,
                            "could not mirror SHOCS friendly name to physical LIFX device"
                        ),
                    }
                }
            }
        }

        let label = physical_label.or_else(|| stored.device_label.clone());
        let now = Instant::now();
        let mut lights = state.lights.write().await;

        match lights.entry(device.id) {
            Entry::Vacant(entry) => {
                let mode = stored.mode;
                let label_for_log = stored
                    .friendly_name
                    .as_deref()
                    .or(label.as_deref())
                    .unwrap_or("<unknown>");

                info!(
                    lifx_id = %format!("{:#018x}", device.id),
                    address = %device.addr,
                    label = %label_for_log,
                    control_enabled = stored.control_enabled,
                    mode = ?mode,
                    "discovered new LIFX device"
                );

                entry.insert(ManagedLight {
                    device: device.clone(),
                    label,
                    friendly_name: stored.friendly_name,
                    control_enabled: stored.control_enabled,
                    mode,
                    observed: None,
                    desired_power: None,
                    desired_color: None,
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
                light.label = label;
                light.friendly_name = stored.friendly_name;
                light.control_enabled = stored.control_enabled;
                light.mode = stored.mode;
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
