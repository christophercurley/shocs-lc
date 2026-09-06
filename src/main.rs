mod config;
mod logging;
mod registry;
mod schedule;
mod store;
mod tasks;
mod test_mode;
mod web;

use std::error::Error;
use std::sync::Arc;

use lifx::LifxClient;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use config::Config;
use registry::{ControllerState, refresh_registry};
use schedule::desired_power_now;
use store::PostgresStore;
use tasks::{
    apply_test_power, color_task, discovery_task, poll_light_states, power_schedule_task,
    state_poll_task, test_reconcile_task, timer_mode_task,
};
use test_mode::TestModeState;
use web::WebState;

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
        http_bind = %config.http_bind_addr,
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
    info!(
        ids = %initial_test_ids,
        "bootstrap Test Mode IDs configured; persisted database modes take precedence"
    );

    let store = Arc::new(PostgresStore::connect(&config.database_url).await?);
    let persisted_lights = store.load_lights().await?;
    let persisted_groups = store.load_groups().await?;
    let persisted_timers = store.load_timer_schedules().await?;
    info!(
        lights = persisted_lights.len(),
        groups = persisted_groups.len(),
        timers = persisted_timers.len(),
        "PostgreSQL configuration loaded and migrations are current"
    );

    let client =
        Arc::new(LifxClient::bind(config.bind_addr, config.lifx_broadcast_addr, SOURCE_ID).await?);

    let state = ControllerState::new(
        &config.initial_test_ids,
        persisted_lights,
        persisted_groups,
        persisted_timers,
        Arc::clone(&store),
    );

    match refresh_registry(&client, &state).await {
        Ok(count) => info!(devices = count, "initial LIFX discovery finished"),
        Err(err) => warn!(error = %err, "initial LIFX discovery failed"),
    }

    let observed = poll_light_states(&client, &state).await;
    info!(devices = observed, "initial LIFX state poll finished");

    let initial_power = desired_power_now(config.timezone, config.off_time, config.on_time);
    let test_mode = TestModeState::new(initial_power);

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
        test_mode.clone(),
    ));

    let color_handle = tokio::spawn(color_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
        test_mode.clone(),
    ));

    let power_handle = tokio::spawn(power_schedule_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
        test_mode.clone(),
    ));

    let timer_handle = tokio::spawn(timer_mode_task(
        Arc::clone(&client),
        state.clone(),
        Arc::clone(&config),
    ));

    let listener = TcpListener::bind(config.http_bind_addr).await?;
    let app = web::router(WebState {
        client: Arc::clone(&client),
        controller: state,
        config: Arc::clone(&config),
        test_mode,
    });

    info!(
        http_bind = %config.http_bind_addr,
        "internal Axum service is listening on loopback behind the SHOCS gateway"
    );

    let web_handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            error!(error = %err, "SHOCS-LC web server stopped unexpectedly");
        }
    });

    info!(url = %format!("http://{}/", config.http_bind_addr), "SHOCS-LC web UI is listening");
    info!("SHOCS Light Controller tasks are running");

    tokio::signal::ctrl_c().await?;
    info!("shutdown requested");

    discovery_handle.abort();
    state_poll_handle.abort();
    reconcile_handle.abort();
    color_handle.abort();
    power_handle.abort();
    timer_handle.abort();
    web_handle.abort();

    Ok(())
}
