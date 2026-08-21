use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use lifx::{Color, LifxClient, Power};
use tokio::time::sleep;

use crate::config::Config;
use crate::registry::{DeviceRegistry, refresh_registry, resolve_devices};
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

pub async fn apply_desired_power(
    client: &LifxClient,
    registry: &DeviceRegistry,
    config: &Config,
    power: Power,
) {
    let devices = resolve_devices(registry, &config.controlled_ids).await;

    if devices.is_empty() {
        println!("No controlled LIFX devices are currently in the registry.");
        return;
    }

    match client
        .set_power_many(&devices, power, config.transition)
        .await
    {
        Ok(()) => println!(
            "Applied desired power {:?} to {} controlled device(s).",
            power,
            devices.len()
        ),
        Err(err) => eprintln!("Failed to apply desired power {:?}: {}", power, err),
    }
}

pub async fn discovery_task(
    client: Arc<LifxClient>,
    registry: DeviceRegistry,
    config: Arc<Config>,
    desired_on: Arc<AtomicBool>,
) {
    loop {
        sleep(config.discovery_interval).await;

        println!("Running scheduled LIFX discovery...");

        match refresh_registry(&client, &registry).await {
            Ok(count) => {
                println!("Discovery saw {count} LIFX device(s).");

                let power = if desired_on.load(Ordering::Relaxed) {
                    Power::On
                } else {
                    Power::Off
                };

                // Primitive desired-state reconciliation. A bulb that reboots
                // into the wrong power state gets corrected on the next scan.
                apply_desired_power(&client, &registry, &config, power).await;
            }
            Err(err) => eprintln!("LIFX discovery failed: {err}"),
        }
    }
}

pub async fn color_task(client: Arc<LifxClient>, registry: DeviceRegistry, config: Arc<Config>) {
    let mut color_index = 0usize;

    loop {
        let devices = resolve_devices(&registry, &config.controlled_ids).await;

        if devices.is_empty() {
            println!("Color heartbeat skipped: no controlled devices in registry.");
        } else {
            let (name, color) = COLORS[color_index];

            match client
                .set_color_many(&devices, color, config.transition)
                .await
            {
                Ok(()) => println!(
                    "Color heartbeat: fading {} controlled device(s) to {} over {} seconds.",
                    devices.len(),
                    name,
                    config.transition.as_secs()
                ),
                Err(err) => eprintln!("Color heartbeat failed: {err}"),
            }

            color_index = (color_index + 1) % COLORS.len();
        }

        sleep(config.color_interval).await;
    }
}

pub async fn power_schedule_task(
    client: Arc<LifxClient>,
    registry: DeviceRegistry,
    config: Arc<Config>,
    desired_on: Arc<AtomicBool>,
) {
    loop {
        let (boundary, power) =
            match next_power_boundary(config.timezone, config.off_time, config.on_time) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("Could not calculate next power boundary: {err}");
                    sleep(std::time::Duration::from_secs(60)).await;
                    continue;
                }
            };

        let now = Utc::now();
        let boundary_utc = boundary.with_timezone(&Utc);

        let wait = match (boundary_utc - now).to_std() {
            Ok(value) => value,
            Err(_) => std::time::Duration::ZERO,
        };

        println!(
            "Next power boundary: {} -> {:?}",
            boundary.format("%Y-%m-%d %H:%M:%S %Z"),
            power
        );

        sleep(wait).await;

        desired_on.store(matches!(power, Power::On), Ordering::Relaxed);

        println!("Power schedule boundary reached: {:?}.", power);
        apply_desired_power(&client, &registry, &config, power).await;

        // Re-evaluate from civil time after each boundary instead of assuming
        // the previous monotonic sleep remains authoritative forever.
        let current = desired_power_now(config.timezone, config.off_time, config.on_time);
        desired_on.store(matches!(current, Power::On), Ordering::Relaxed);
    }
}
