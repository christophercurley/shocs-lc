mod config;
mod logging;
mod registry;
mod schedule;
mod tasks;

use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use lifx::{LifxClient, Power};
use tracing::{info, warn};

use config::Config;
use registry::{ControllerState, refresh_registry};
use schedule::desired_power_now;
use tasks::{
    apply_test_power, color_task, discovery_task, poll_light_states, power_schedule_task,
    state_poll_task, test_reconcile_task,
};

const SOURCE_ID: u32 = 0x5348_4F43; // "SHOC"

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    logging::init();

    let config = Arc::new(Config::from_env()?);
    let initial_test_ids = config
        .initial_test_ids
        .iter()
        .map(|id| format!("{id:#018x}"))
        .collect::<Vec<_>>()
        .join(", ");

    info!(
        bind = %config.bind_addr,
        broadcast = %config.lifx_broadcast_addr,
        discovery_seconds = config.discovery_interval.as_secs(),
        state_poll_seconds = config.state_poll_interval.as_secs(),
        "SHOCS Light Controller starting"
    );
    info!(
        color_interval_seconds = config.color_interval.as_secs(),
        transition_seconds = config.transition.as_secs(),
        off_time = %config.off_time.format("%H:%M"),
        on_time = %config.on_time.format("%H:%M"),
        timezone = %config.timezone,
        "Test Mode configured"
    );
    info!(ids = %initial_test_ids, "lights configured to start in Test Mode");

    let client =
        Arc::new(LifxClient::bind(config.bind_addr, config.lifx_broadcast_addr, SOURCE_ID).await?);

    let state = ControllerState::new(&config.initial_test_ids);

    match refresh_registry(&client, &state).await {
        Ok(count) => info!(devices = count, "initial LIFX discovery finished"),
        Err(err) => warn!(error = %err, "initial LIFX discovery failed"),
    }

    let observed = poll_light_states(&client, &state).await;
    info!(devices = observed, "initial LIFX state poll finished");

    let initial_power = desired_power_now(config.timezone, config.off_time, config.on_time);
    let desired_on = Arc::new(AtomicBool::new(matches!(initial_power, Power::On)));

    info!(power = ?initial_power, "current Test Mode scheduled power state");
    apply_test_power(&client, &state, &config, initial_power).await;

    let discovery_handle = tokio::spawn(discovery_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
    ));

    let state_poll_handle = tokio::spawn(state_poll_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
    ));

    let reconcile_handle = tokio::spawn(test_reconcile_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
        Arc::clone(&desired_on),
    ));

    let color_handle = tokio::spawn(color_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
    ));

    let power_handle = tokio::spawn(power_schedule_task(
        Arc::clone(&client),
        state,
        Arc::clone(&config),
        Arc::clone(&desired_on),
    ));

    info!("SHOCS Light Controller tasks are running");

    tokio::signal::ctrl_c().await?;
    info!("shutdown requested");

    discovery_handle.abort();
    state_poll_handle.abort();
    reconcile_handle.abort();
    color_handle.abort();
    power_handle.abort();

    Ok(())
}
