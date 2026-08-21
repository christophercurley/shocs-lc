mod config;
mod registry;
mod schedule;
mod tasks;

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use lifx::{LifxClient, Power};

use config::Config;
use registry::{new_registry, refresh_registry};
use schedule::desired_power_now;
use tasks::{apply_desired_power, color_task, discovery_task, power_schedule_task};

const SOURCE_ID: u32 = 0x5348_4F43; // "SHOC"

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Arc::new(Config::from_env()?);

    println!("SHOCS Light Controller starting...");
    println!("LIFX bind address      : {}", config.bind_addr);
    println!("LIFX broadcast address : {}", config.lifx_broadcast_addr);
    println!(
        "Discovery interval     : {} seconds",
        config.discovery_interval.as_secs()
    );
    println!(
        "Color heartbeat        : every {} seconds, {} second fade",
        config.color_interval.as_secs(),
        config.transition.as_secs()
    );
    println!(
        "Power schedule         : OFF {} / ON {} ({})",
        config.off_time.format("%H:%M"),
        config.on_time.format("%H:%M"),
        config.timezone
    );
    println!(
        "Controlled LIFX IDs    : {}",
        config
            .controlled_ids
            .iter()
            .map(|id| format!("{id:#018x}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();

    let client =
        Arc::new(LifxClient::bind(config.bind_addr, config.lifx_broadcast_addr, SOURCE_ID).await?);

    let registry = new_registry();

    println!("Running initial LIFX discovery...");
    match refresh_registry(&client, &registry).await {
        Ok(count) => println!("Initial discovery saw {count} LIFX device(s)."),
        Err(err) => eprintln!("Initial LIFX discovery failed: {err}"),
    }

    let initial_power = desired_power_now(config.timezone, config.off_time, config.on_time);

    let desired_on = Arc::new(AtomicBool::new(matches!(initial_power, Power::On)));

    println!("Current scheduled power state: {:?}.", initial_power);
    apply_desired_power(&client, &registry, &config, initial_power).await;

    let discovery_handle = tokio::spawn(discovery_task(
        Arc::clone(&client),
        Arc::clone(&registry),
        Arc::clone(&config),
        Arc::clone(&desired_on),
    ));

    let color_handle = tokio::spawn(color_task(
        Arc::clone(&client),
        Arc::clone(&registry),
        Arc::clone(&config),
    ));

    let power_handle = tokio::spawn(power_schedule_task(
        Arc::clone(&client),
        Arc::clone(&registry),
        Arc::clone(&config),
        Arc::clone(&desired_on),
    ));

    println!("SHOCS Light Controller tasks are running.");
    println!();

    tokio::signal::ctrl_c().await?;
    println!("Shutdown requested.");

    discovery_handle.abort();
    color_handle.abort();
    power_handle.abort();

    Ok(())
}
