use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use lifx::{LifxClient, LifxDevice, Power};
use tokio::time::sleep;
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::registry::{ControllerState, LightMode, TimerSchedule, TimerTarget, refresh_registry};
use crate::schedule::{desired_power_now, next_power_boundary};
use crate::test_mode::TestModeState;

const TEST_SYNC_SETTLE_MARGIN: Duration = Duration::from_millis(400);
const TEST_SYNC_MAX_ATTEMPTS: usize = 2;
const TEST_SYNC_COLOR_TOLERANCE: u16 = 512;
const TEST_SYNC_KELVIN_TOLERANCE: u16 = 50;

pub async fn apply_test_power(
    client: &LifxClient,
    state: &ControllerState,
    config: &Config,
    power: Power,
) {
    let devices = state.devices_in_mode(LightMode::Test).await;
    state
        .set_desired_power_for_mode(LightMode::Test, power)
        .await;

    if devices.is_empty() {
        debug!("Test Mode power update skipped: no enrolled lights");
        return;
    }

    match client
        .set_power_many(&devices, power, config.transition)
        .await
    {
        Ok(()) => info!(
            power = ?power,
            lights = devices.len(),
            transition_seconds = config.transition.as_secs(),
            "applied Test Mode power"
        ),
        Err(err) => warn!(
            power = ?power,
            error = %err,
            "failed to apply Test Mode power"
        ),
    }
}

/// Apply Test Mode's complete current desired state to one light.
///
/// Color and brightness are carried in the same LIFX HSBK command, so both
/// travel toward the mode target over the configured transition time.
pub async fn sync_light_to_test_mode(
    client: &LifxClient,
    device: &LifxDevice,
    config: &Config,
    test_mode: &TestModeState,
    power: Power,
) -> lifx::Result<()> {
    let (_, color) = test_mode.current_color();

    client.set_color(device, color, config.transition).await?;
    client.set_power(device, power, config.transition).await?;

    Ok(())
}

/// Confirm that a newly enrolled light actually arrived at Test Mode's current
/// state after the transition. If it did not, retry the complete state once.
///
/// The mode is checked before each verification/retry so a user who moves the
/// light back to Custom while this is running is never fought by stale work.
pub async fn confirm_test_mode_sync(
    client: Arc<LifxClient>,
    state: ControllerState,
    config: Arc<Config>,
    test_mode: TestModeState,
    device: LifxDevice,
) {
    let settle_time = config.transition.saturating_add(TEST_SYNC_SETTLE_MARGIN);

    sleep(settle_time).await;

    for attempt in 1..=TEST_SYNC_MAX_ATTEMPTS {
        let Some(light) = state.light(device.id).await else {
            return;
        };

        if light.mode != LightMode::Test {
            trace!(
                lifx_id = %format!("{:#018x}", device.id),
                "Test Mode synchronization cancelled because light left the mode"
            );
            return;
        }

        let (color_name, desired_color) = test_mode.current_color();
        let desired_power = light.power_override.unwrap_or(test_mode.power());

        match client.get_light_state(&device).await {
            Ok(observed) => {
                state.record_observation(device.id, observed).await;

                if test_state_matches(observed, desired_color, desired_power) {
                    state.clear_brightness_transition(device.id).await;

                    info!(
                        lifx_id = %format!("{:#018x}", device.id),
                        color = color_name,
                        brightness = desired_color.brightness,
                        power = ?desired_power,
                        attempt,
                        "confirmed light synchronized to Test Mode"
                    );
                    return;
                }

                warn!(
                    lifx_id = %format!("{:#018x}", device.id),
                    color = color_name,
                    desired_brightness = desired_color.brightness,
                    observed_brightness = observed.brightness,
                    desired_power = ?desired_power,
                    observed_power = ?observed.power,
                    attempt,
                    "light did not fully reach Test Mode target"
                );
            }
            Err(err) => warn!(
                lifx_id = %format!("{:#018x}", device.id),
                attempt,
                error = %err,
                "could not verify Test Mode synchronization"
            ),
        }

        if attempt == TEST_SYNC_MAX_ATTEMPTS {
            break;
        }

        // Re-checking current Test state inside sync_light_to_test_mode means a
        // color heartbeat that happened during the first transition is handled
        // naturally: the retry targets whatever Test Mode owns *now*.
        let (_, retry_color) = test_mode.current_color();
        let retry_started = Instant::now();
        state.set_desired_color(device.id, retry_color).await;
        state
            .set_desired_power(device.id, Some(desired_power))
            .await;
        state
            .begin_brightness_transition(
                device.id,
                retry_color.brightness,
                config.transition,
                retry_started,
            )
            .await;

        if let Err(err) =
            sync_light_to_test_mode(&client, &device, &config, &test_mode, desired_power).await
        {
            state.clear_brightness_transition(device.id).await;
            warn!(
                lifx_id = %format!("{:#018x}", device.id),
                error = %err,
                "failed to retry Test Mode synchronization"
            );
            return;
        }

        sleep(settle_time).await;
    }

    warn!(
        lifx_id = %format!("{:#018x}", device.id),
        "Test Mode synchronization could not be confirmed after retry"
    );
}

fn test_state_matches(observed: lifx::LightState, desired: lifx::Color, power: Power) -> bool {
    let hue_matches = desired.saturation <= TEST_SYNC_COLOR_TOLERANCE
        || observed.hue.abs_diff(desired.hue) <= TEST_SYNC_COLOR_TOLERANCE;

    observed.power == power
        && hue_matches
        && observed.saturation.abs_diff(desired.saturation) <= TEST_SYNC_COLOR_TOLERANCE
        && observed.brightness.abs_diff(desired.brightness) <= TEST_SYNC_COLOR_TOLERANCE
        && observed.kelvin.abs_diff(desired.kelvin) <= TEST_SYNC_KELVIN_TOLERANCE
}

pub async fn poll_light_states(client: &LifxClient, state: &ControllerState) -> usize {
    let devices = state.known_devices().await;
    let mut updated = 0usize;

    for device in devices {
        match client.get_light_state(&device).await {
            Ok(observed) => {
                if let Some(changed) = state.record_observation(device.id, observed).await {
                    updated += 1;

                    if changed {
                        debug!(
                            lifx_id = %format!("{:#018x}", device.id),
                            power = ?observed.power,
                            hue = observed.hue,
                            saturation = observed.saturation,
                            brightness = observed.brightness,
                            kelvin = observed.kelvin,
                            "observed LIFX state changed"
                        );
                    } else {
                        trace!(
                            lifx_id = %format!("{:#018x}", device.id),
                            "observed LIFX state unchanged"
                        );
                    }
                }
            }
            Err(err) => trace!(
                lifx_id = %format!("{:#018x}", device.id),
                address = %device.addr,
                error = %err,
                "LIFX state poll failed"
            ),
        }
    }

    updated
}

pub async fn discovery_task(client: Arc<LifxClient>, state: ControllerState, config: Arc<Config>) {
    loop {
        sleep(config.discovery_interval).await;

        match refresh_registry(&client, &state).await {
            Ok(count) => debug!(devices = count, "scheduled LIFX discovery finished"),
            Err(err) => warn!(error = %err, "scheduled LIFX discovery failed"),
        }
    }
}

pub async fn state_poll_task(client: Arc<LifxClient>, state: ControllerState, config: Arc<Config>) {
    loop {
        sleep(config.state_poll_interval).await;
        let updated = poll_light_states(&client, &state).await;
        trace!(updated, "scheduled LIFX state poll finished");
    }
}

pub async fn test_reconcile_task(
    client: Arc<LifxClient>,
    state: ControllerState,
    config: Arc<Config>,
    test_mode: TestModeState,
) {
    // Offset this slightly from the state poll so reconciliation normally acts
    // on a fresh physical observation instead of racing the poll timer.
    let interval = config.state_poll_interval + Duration::from_secs(1);

    loop {
        sleep(interval).await;

        let mode_power = test_mode.power();
        let mismatches = state
            .devices_with_power_mismatch(LightMode::Test, mode_power)
            .await;

        if mismatches.is_empty() {
            continue;
        }

        for desired in [Power::Off, Power::On] {
            let devices = mismatches
                .iter()
                .filter(|(_, power)| *power == desired)
                .map(|(device, _)| device.clone())
                .collect::<Vec<_>>();

            if devices.is_empty() {
                continue;
            }

            match client
                .set_power_many(&devices, desired, config.transition)
                .await
            {
                Ok(()) => info!(
                    power = ?desired,
                    lights = devices.len(),
                    "reconciled Test Mode power after observed mismatch"
                ),
                Err(err) => warn!(
                    power = ?desired,
                    error = %err,
                    "Test Mode power reconciliation failed"
                ),
            }
        }
    }
}

pub async fn color_task(
    client: Arc<LifxClient>,
    state: ControllerState,
    config: Arc<Config>,
    test_mode: TestModeState,
) {
    loop {
        let devices = state.devices_in_mode(LightMode::Test).await;
        let (name, color) = test_mode.current_color();

        if devices.is_empty() {
            debug!(
                color = name,
                "Test Mode color heartbeat skipped: no enrolled lights"
            );
        } else {
            let transition_started = Instant::now();
            match client
                .set_color_many(&devices, color, config.transition)
                .await
            {
                Ok(()) => {
                    state
                        .set_desired_color_for_mode(LightMode::Test, color)
                        .await;
                    state
                        .begin_brightness_transition_for_mode(
                            LightMode::Test,
                            color.brightness,
                            config.transition,
                            transition_started,
                        )
                        .await;

                    info!(
                        lights = devices.len(),
                        color = name,
                        brightness = color.brightness,
                        transition_seconds = config.transition.as_secs(),
                        "Test Mode color heartbeat"
                    );
                }
                Err(err) => warn!(error = %err, "Test Mode color heartbeat failed"),
            }
        }

        sleep(config.color_interval).await;
        test_mode.advance_color();
    }
}

pub async fn power_schedule_task(
    client: Arc<LifxClient>,
    state: ControllerState,
    config: Arc<Config>,
    test_mode: TestModeState,
) {
    loop {
        let (boundary, power) =
            match next_power_boundary(config.timezone, config.off_time, config.on_time) {
                Ok(value) => value,
                Err(err) => {
                    warn!(error = %err, "could not calculate next Test Mode power boundary");
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };

        let now = Utc::now();
        let boundary_utc = boundary.with_timezone(&Utc);

        let wait = match (boundary_utc - now).to_std() {
            Ok(value) => value,
            Err(_) => Duration::ZERO,
        };

        info!(
            boundary = %boundary.format("%Y-%m-%d %H:%M:%S %Z"),
            power = ?power,
            "next Test Mode power boundary"
        );

        sleep(wait).await;

        test_mode.set_power(power);
        let cleared = state.clear_power_overrides_in_mode(LightMode::Test).await;

        info!(
            power = ?power,
            cleared_power_overrides = cleared,
            "Test Mode power boundary reached"
        );
        apply_test_power(&client, &state, &config, power).await;

        // Re-evaluate civil time after every boundary so DST and wall-clock
        // behavior remain authoritative rather than assuming a fixed cadence.
        let current = desired_power_now(config.timezone, config.off_time, config.on_time);
        test_mode.set_power(current);
    }
}

/// Run persisted Timer Mode schedules entirely from in-memory configuration.
///
/// PostgreSQL is read only at startup/configuration changes; this loop does not
/// poll the database. A manual power override on a Timer light is honored until
/// that timer's next ON/OFF boundary, mirroring Test Mode's power semantics.
pub async fn timer_mode_task(client: Arc<LifxClient>, state: ControllerState, config: Arc<Config>) {
    let mut applied = HashMap::<i64, String>::new();

    loop {
        let mut schedules = state.timer_schedules().await;
        schedules.sort_by_key(|schedule| schedule.id);

        let active_ids = schedules
            .iter()
            .filter(|schedule| schedule.enabled)
            .map(|schedule| schedule.id)
            .collect::<HashSet<_>>();
        applied.retain(|id, _| active_ids.contains(id));

        // Lowest timer ID wins defensively if a later group-membership edit
        // somehow creates overlap that bypassed API validation.
        let mut claimed = HashSet::new();

        for schedule in schedules.into_iter().filter(|schedule| schedule.enabled) {
            let desired = desired_power_now(schedule.timezone, schedule.off_time, schedule.on_time);
            let fingerprint = timer_fingerprint(&schedule, desired);
            let boundary_or_config_changed = applied.get(&schedule.id) != Some(&fingerprint);

            let lights = state.timer_target_lights(schedule.target).await;
            let mut on_devices = Vec::new();
            let mut off_devices = Vec::new();

            for light in lights {
                if light.mode != LightMode::Timer || !light.control_enabled {
                    continue;
                }
                if !claimed.insert(light.device.id) {
                    continue;
                }

                if boundary_or_config_changed {
                    let _ = state.set_power_override(light.device.id, None).await;
                }

                let refreshed = state.light(light.device.id).await.unwrap_or(light.clone());
                let effective = refreshed.power_override.unwrap_or(desired);
                let mismatch = refreshed.desired_power != Some(effective)
                    || refreshed
                        .observed
                        .map_or(true, |observed| observed.power != effective);

                if !boundary_or_config_changed && !mismatch {
                    continue;
                }

                let _ = state
                    .set_desired_power(refreshed.device.id, Some(effective))
                    .await;

                match effective {
                    Power::On => on_devices.push(refreshed.device),
                    Power::Off => off_devices.push(refreshed.device),
                }
            }

            for (power, devices) in [(Power::Off, off_devices), (Power::On, on_devices)] {
                if devices.is_empty() {
                    continue;
                }

                match client
                    .set_power_many(&devices, power, config.transition)
                    .await
                {
                    Ok(()) => info!(
                        timer_id = schedule.id,
                        lights = devices.len(),
                        power = ?power,
                        transition_seconds = config.transition.as_secs(),
                        "applied Timer Mode power"
                    ),
                    Err(err) => warn!(
                        timer_id = schedule.id,
                        power = ?power,
                        error = %err,
                        "Timer Mode power update failed"
                    ),
                }
            }

            if boundary_or_config_changed {
                info!(
                    timer_id = schedule.id,
                    target = %timer_target_log(schedule.target),
                    on_time = %schedule.on_time.format("%H:%M"),
                    off_time = %schedule.off_time.format("%H:%M"),
                    timezone = %schedule.timezone,
                    power = ?desired,
                    "Timer Mode schedule became authoritative"
                );
                applied.insert(schedule.id, fingerprint);
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

fn timer_fingerprint(schedule: &TimerSchedule, desired: Power) -> String {
    format!(
        "{}|{}|{}|{}|{:?}",
        timer_target_log(schedule.target),
        schedule.on_time.format("%H:%M"),
        schedule.off_time.format("%H:%M"),
        schedule.timezone,
        desired
    )
}

fn timer_target_log(target: TimerTarget) -> String {
    match target {
        TimerTarget::Light(id) => format!("light:{id:016x}"),
        TimerTarget::Group(id) => format!("group:{id}"),
    }
}
