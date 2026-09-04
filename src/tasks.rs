use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use lifx::{Color, LifxClient, Power};
use tokio::time::sleep;
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::registry::{ControllerState, LightMode, refresh_registry};
use crate::schedule::{desired_power_now, next_power_boundary};

const COLORS: [(&str, Color); 9] = [
    ("Warm White", Color::white(2_700, 55_000)),
    ("Amber", Color::new(7_500, 50_000, 55_000, 3_500)),
    ("Gold", Color::new(10_500, 48_000, 55_000, 3_500)),
    ("Green", Color::new(21_845, 52_000, 55_000, 3_500)),
    ("Teal", Color::new(27_500, 50_000, 55_000, 3_500)),
    ("Cyan", Color::new(32_768, 50_000, 55_000, 3_500)),
    ("Azure", Color::new(38_229, 52_000, 55_000, 3_500)),
    ("Blue", Color::new(43_690, 52_000, 55_000, 3_500)),
    ("Violet", Color::new(49_151, 48_000, 55_000, 3_500)),
];

pub async fn apply_test_power(
    client: &LifxClient,
    state: &ControllerState,
    config: &Config,
    power: Power,
) {
    let devices = state.devices_in_mode(LightMode::Test).await;

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
    desired_on: Arc<AtomicBool>,
) {
    // Offset this slightly from the state poll so reconciliation normally acts
    // on a fresh physical observation instead of racing the poll timer.
    let interval = config.state_poll_interval + Duration::from_secs(1);

    loop {
        sleep(interval).await;

        let desired = if desired_on.load(Ordering::Relaxed) {
            Power::On
        } else {
            Power::Off
        };

        let devices = state
            .devices_with_power_mismatch(LightMode::Test, desired)
            .await;

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

pub async fn color_task(client: Arc<LifxClient>, state: ControllerState, config: Arc<Config>) {
    let mut color_index = 0usize;

    loop {
        let devices = state.devices_in_mode(LightMode::Test).await;

        if devices.is_empty() {
            debug!("Test Mode color heartbeat skipped: no enrolled lights");
        } else {
            let (name, color) = COLORS[color_index];

            match client
                .set_color_many(&devices, color, config.transition)
                .await
            {
                Ok(()) => info!(
                    lights = devices.len(),
                    color = name,
                    transition_seconds = config.transition.as_secs(),
                    "Test Mode color heartbeat"
                ),
                Err(err) => warn!(error = %err, "Test Mode color heartbeat failed"),
            }

            color_index = (color_index + 1) % COLORS.len();
        }

        sleep(config.color_interval).await;
    }
}

pub async fn power_schedule_task(
    client: Arc<LifxClient>,
    state: ControllerState,
    config: Arc<Config>,
    desired_on: Arc<AtomicBool>,
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

        desired_on.store(matches!(power, Power::On), Ordering::Relaxed);
        info!(power = ?power, "Test Mode power boundary reached");
        apply_test_power(&client, &state, &config, power).await;

        // Re-evaluate civil time after every boundary so DST and wall-clock
        // behavior remain authoritative rather than assuming a fixed cadence.
        let current = desired_power_now(config.timezone, config.off_time, config.on_time);
        desired_on.store(matches!(current, Power::On), Ordering::Relaxed);
    }
}
