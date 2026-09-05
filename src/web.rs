use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use lifx::{Color, LifxClient, LifxId, Power};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{error, info, trace, warn};

use crate::config::Config;
use crate::registry::{ControllerState, LightMode, ManagedLight};
use crate::store::StoreError;
use crate::tasks::{confirm_test_mode_sync, sync_light_to_test_mode};
use crate::test_mode::TestModeState;

const MANUAL_TRANSITION: Duration = Duration::from_millis(250);
const LIVE_CONTROL_TRANSITION: Duration = Duration::from_millis(100);
const MANUAL_CONFIRM_DELAY: Duration = Duration::from_millis(350);

#[derive(Clone)]
pub struct WebState {
    pub client: Arc<LifxClient>,
    pub controller: ControllerState,
    pub config: Arc<Config>,
    pub test_mode: TestModeState,
}

pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/lights", get(lights_page))
        .route("/manage", get(manage_page))
        .route("/static/styles.css", get(stylesheet))
        .route("/static/app.js", get(app_javascript))
        .route("/static/manage.js", get(manage_javascript))
        .route("/api/lights", get(list_lights))
        .route("/api/manage/lights", get(list_managed_lights))
        .route("/api/manage/lights/{id}", put(update_managed_light))
        .route("/api/lights/{id}/power", put(set_power))
        .route("/api/lights/{id}/brightness", put(set_brightness))
        .route("/api/lights/{id}/color", put(set_color))
        .route("/api/lights/{id}/mode", put(set_mode))
        .with_state(state)
}

async fn index_page() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn lights_page() -> Html<&'static str> {
    Html(include_str!("static/lights.html"))
}

async fn manage_page() -> Html<&'static str> {
    Html(include_str!("static/manage.html"))
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("static/styles.css"),
    )
}

async fn app_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/app.js"),
    )
}

async fn manage_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/manage.js"),
    )
}

#[derive(Debug, Serialize)]
struct LightView {
    id: String,
    label: String,
    address: String,
    online: bool,
    control_enabled: bool,
    mode: &'static str,
    power_on: Option<bool>,
    brightness_percent: Option<u8>,
    brightness_transition: Option<BrightnessTransitionView>,
    hue_degrees: Option<u16>,
    saturation_percent: Option<u8>,
    kelvin: Option<u16>,
}

#[derive(Debug, Serialize)]
struct BrightnessTransitionView {
    to_percent: u8,
    remaining_ms: u64,
}

async fn list_lights(State(state): State<WebState>) -> Json<Vec<LightView>> {
    let mut lights = state.controller.lights().await;
    lights.sort_by_key(sort_name);

    let online_window = state.config.state_poll_interval.saturating_mul(3);
    let views = lights
        .into_iter()
        .map(|light| to_light_view(light, online_window))
        .collect();

    Json(views)
}

#[derive(Debug, Serialize)]
struct ManagedLightView {
    id: String,
    display_name: String,
    device_label: Option<String>,
    friendly_name: Option<String>,
    control_enabled: bool,
    mode: &'static str,
    online: bool,
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManagedLightUpdate {
    friendly_name: Option<String>,
    control_enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ManagedLightUpdateResult {
    saved: bool,
    label_synced: Option<bool>,
    test_state_synced: Option<bool>,
}

async fn list_managed_lights(State(state): State<WebState>) -> Json<Vec<ManagedLightView>> {
    let configured = state.controller.configured_lights().await;
    let runtime = state
        .controller
        .lights()
        .await
        .into_iter()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    let online_window = state.config.state_poll_interval.saturating_mul(3);
    let mut views = configured
        .into_iter()
        .map(|stored| {
            let runtime_light = runtime.get(&stored.id);
            let online = runtime_light
                .and_then(|light| light.last_observed)
                .is_some_and(|seen| seen.elapsed() <= online_window);

            ManagedLightView {
                id: format!("{:#018x}", stored.id),
                display_name: stored
                    .friendly_name
                    .clone()
                    .or_else(|| stored.device_label.clone())
                    .unwrap_or_else(|| format!("LIFX {:#018x}", stored.id)),
                device_label: stored.device_label,
                friendly_name: stored.friendly_name,
                control_enabled: stored.control_enabled,
                mode: stored.mode.as_str(),
                online,
                address: runtime_light.map(|light| light.device.addr.to_string()),
            }
        })
        .collect::<Vec<_>>();

    views.sort_by_key(|light| light.display_name.to_ascii_lowercase());
    Json(views)
}

async fn update_managed_light(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<ManagedLightUpdate>,
) -> Result<Json<ManagedLightUpdateResult>, ApiError> {
    let id = parse_lifx_id(&id)?;

    if request.friendly_name.is_none() && request.control_enabled.is_none() {
        return Err(ApiError::bad_request(
            "no device-management fields were supplied",
        ));
    }

    let mut label_synced = None;
    let mut test_state_synced = None;

    if let Some(friendly_name) = request.friendly_name {
        let friendly_name = normalize_friendly_name(Some(friendly_name))?;
        state
            .controller
            .set_friendly_name(id, friendly_name.clone())
            .await
            .map_err(ApiError::store)?;

        if let Some(name) = friendly_name.as_deref() {
            // Persistence is authoritative. Assume physical sync is pending
            // until a reachable, control-enabled bulb confirms the label.
            label_synced = Some(false);

            if let Some(light) = state.controller.light(id).await {
                if light.control_enabled {
                    match state.client.set_label(&light.device, name).await {
                        Ok(confirmed) => {
                            state
                                .controller
                                .record_device_label(id, Some(confirmed.clone()))
                                .await
                                .map_err(ApiError::store)?;
                            label_synced = Some(true);

                            info!(
                                lifx_id = %format!("{id:#018x}"),
                                label = %confirmed,
                                "SHOCS friendly name mirrored to physical LIFX label"
                            );
                        }
                        Err(err) => {
                            label_synced = Some(false);
                            warn!(
                                lifx_id = %format!("{id:#018x}"),
                                desired_label = %name,
                                error = %err,
                                "friendly name persisted; physical label sync will retry later"
                            );
                        }
                    }
                }
            }
        }
    }

    if let Some(control_enabled) = request.control_enabled {
        state
            .controller
            .set_control_enabled(id, control_enabled)
            .await
            .map_err(ApiError::store)?;

        info!(
            lifx_id = %format!("{id:#018x}"),
            control_enabled,
            "light control-enabled state changed"
        );

        if control_enabled {
            if let Some(light) = state.controller.light(id).await {
                // Re-enabling control is also a convenient opportunity to
                // reconcile the desired SHOCS name immediately.
                if let Some(name) = light.friendly_name.as_deref() {
                    if light.label.as_deref() != Some(name) {
                        match state.client.set_label(&light.device, name).await {
                            Ok(confirmed) => {
                                state
                                    .controller
                                    .record_device_label(id, Some(confirmed))
                                    .await
                                    .map_err(ApiError::store)?;
                                label_synced = Some(true);
                            }
                            Err(err) => {
                                label_synced = Some(false);
                                warn!(
                                    lifx_id = %format!("{id:#018x}"),
                                    error = %err,
                                    "re-enabled light but physical label sync is pending"
                                );
                            }
                        }
                    }
                }

                if light.mode == LightMode::Test {
                    match start_test_mode_sync(&state, id, &light).await {
                        Ok(()) => test_state_synced = Some(true),
                        Err(err) => {
                            test_state_synced = Some(false);
                            warn!(
                                lifx_id = %format!("{id:#018x}"),
                                error = %err,
                                "re-enabled Test light; full mode sync will retry through normal automation"
                            );
                        }
                    }
                }
            }
        }
    }

    Ok(Json(ManagedLightUpdateResult {
        saved: true,
        label_synced,
        test_state_synced,
    }))
}

#[derive(Debug, Deserialize)]
struct PowerRequest {
    on: bool,
}

async fn set_power(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<PowerRequest>,
) -> Result<StatusCode, ApiError> {
    let id = parse_lifx_id(&id)?;
    let light = state
        .controller
        .light(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    require_control_enabled(&light)?;

    let power = if request.on { Power::On } else { Power::Off };

    // Power is intentionally orthogonal to mode. While a light is in Test
    // Mode, a manual power command becomes a temporary per-light override
    // instead of ejecting the light from the mode. The next Test power
    // schedule boundary clears the override and resumes scheduled power.
    let previous_override = if light.mode == LightMode::Test {
        state
            .controller
            .set_power_override(id, Some(power))
            .await
            .flatten()
    } else {
        light.power_override
    };
    let previous_desired = state
        .controller
        .set_desired_power(id, Some(power))
        .await
        .flatten();

    if let Err(err) = state
        .client
        .set_power(&light.device, power, MANUAL_TRANSITION)
        .await
    {
        let _ = state
            .controller
            .set_desired_power(id, previous_desired)
            .await;

        if light.mode == LightMode::Test {
            let _ = state
                .controller
                .set_power_override(id, previous_override)
                .await;
        }
        return Err(ApiError::lifx(err));
    }

    confirm_observation(&state, id, &light).await;

    info!(
        lifx_id = %format!("{id:#018x}"),
        power = ?power,
        mode = light.mode.as_str(),
        power_override = light.mode == LightMode::Test,
        "manual web power command"
    );

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct BrightnessRequest {
    percent: u8,
    #[serde(default, rename = "final")]
    final_update: bool,
}

async fn set_brightness(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<BrightnessRequest>,
) -> Result<StatusCode, ApiError> {
    if request.percent > 100 {
        return Err(ApiError::bad_request("brightness percent must be 0-100"));
    }

    let id = parse_lifx_id(&id)?;
    let light = state
        .controller
        .light(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    require_control_enabled(&light)?;

    let previous_mode = state
        .controller
        .set_mode(id, LightMode::Custom)
        .await
        .map_err(ApiError::store)?;

    if previous_mode == Some(LightMode::Test) {
        let _ = state.controller.set_power_override(id, None).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            "manual live control moved light from Test to Custom mode"
        );
    }

    // Live sliders can issue up to ~10 commands per second. Re-querying the
    // bulb before every command would add latency and serialize the stream on
    // the shared UDP socket, so preserve SHOCS's current color intent (falling
    // back to the most recent physical observation).
    let observed = match light.observed {
        Some(observed) => observed,
        None => state
            .client
            .get_light_state(&light.device)
            .await
            .map_err(ApiError::lifx)?,
    };

    let brightness = percent_to_u16(request.percent);
    let desired_color = light.desired_color;
    let color = Color::new(
        desired_color.map(|color| color.hue).unwrap_or(observed.hue),
        desired_color
            .map(|color| color.saturation)
            .unwrap_or(observed.saturation),
        brightness,
        desired_color
            .map(|color| color.kelvin)
            .unwrap_or(observed.kelvin),
    );

    state
        .client
        .set_color(&light.device, color, LIVE_CONTROL_TRANSITION)
        .await
        .map_err(ApiError::lifx)?;

    state
        .controller
        .set_desired_brightness(id, brightness)
        .await;

    // Intermediate slider updates stay fast and fire-and-forget. Only the
    // release/final update waits for a physical readback and emits an INFO log.
    if request.final_update {
        confirm_observation(&state, id, &light).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            brightness_percent = request.percent,
            mode = "custom",
            "manual web brightness command committed"
        );
    } else {
        trace!(
            lifx_id = %format!("{id:#018x}"),
            brightness_percent = request.percent,
            "manual web brightness live update"
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct ColorRequest {
    hue_degrees: u16,
    saturation_percent: u8,
    #[serde(default, rename = "final")]
    final_update: bool,
}

async fn set_color(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<ColorRequest>,
) -> Result<StatusCode, ApiError> {
    if request.hue_degrees > 360 {
        return Err(ApiError::bad_request("hue must be 0-360 degrees"));
    }
    if request.saturation_percent > 100 {
        return Err(ApiError::bad_request("saturation percent must be 0-100"));
    }

    let id = parse_lifx_id(&id)?;
    let light = state
        .controller
        .light(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    require_control_enabled(&light)?;

    let previous_mode = state
        .controller
        .set_mode(id, LightMode::Custom)
        .await
        .map_err(ApiError::store)?;

    if previous_mode == Some(LightMode::Test) {
        let _ = state.controller.set_power_override(id, None).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            "manual color control moved light from Test to Custom mode"
        );
    }

    // A manual color gesture owns hue/saturation but preserves the light's
    // current SHOCS brightness intent and color temperature.
    let observed = match light.observed {
        Some(observed) => observed,
        None => state
            .client
            .get_light_state(&light.device)
            .await
            .map_err(ApiError::lifx)?,
    };

    let brightness = light
        .projected_brightness(Instant::now())
        .unwrap_or(observed.brightness);
    let kelvin = light
        .desired_color
        .map(|color| color.kelvin)
        .unwrap_or(observed.kelvin);

    let hue = hue_degrees_to_u16(request.hue_degrees);
    let saturation = percent_to_u16(request.saturation_percent);
    let color = Color::new(hue, saturation, brightness, kelvin);

    state
        .client
        .set_color(&light.device, color, LIVE_CONTROL_TRANSITION)
        .await
        .map_err(ApiError::lifx)?;

    state.controller.set_desired_color(id, color).await;
    // Color packets also carry brightness. Freeze any prior mode transition at
    // the projected current level so touching color never causes a brightness jump.
    state
        .controller
        .set_desired_brightness(id, brightness)
        .await;

    if request.final_update {
        confirm_observation(&state, id, &light).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            hue_degrees = request.hue_degrees,
            saturation_percent = request.saturation_percent,
            mode = "custom",
            "manual web color command committed"
        );
    } else {
        trace!(
            lifx_id = %format!("{id:#018x}"),
            hue_degrees = request.hue_degrees,
            saturation_percent = request.saturation_percent,
            "manual web color live update"
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct ModeRequest {
    test: bool,
}

async fn set_mode(
    State(state): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<ModeRequest>,
) -> Result<StatusCode, ApiError> {
    let id = parse_lifx_id(&id)?;
    let light = state
        .controller
        .light(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    require_control_enabled(&light)?;

    let mode = if request.test {
        LightMode::Test
    } else {
        LightMode::Custom
    };

    let previous = state
        .controller
        .set_mode(id, mode)
        .await
        .map_err(ApiError::store)?
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    // Changing modes starts with a clean mode-owned power state. Manual Test
    // power overrides are intentionally per-enrollment and do not leak across
    // a mode toggle.
    let previous_override = state
        .controller
        .set_power_override(id, None)
        .await
        .flatten();

    if mode == LightMode::Test {
        if let Err(err) = start_test_mode_sync(&state, id, &light).await {
            if let Err(store_err) = state.controller.set_mode(id, previous).await {
                error!(
                    lifx_id = %format!("{id:#018x}"),
                    error = %store_err,
                    "failed to roll back persisted light mode after Test Mode sync failure"
                );
            }
            let _ = state
                .controller
                .set_power_override(id, previous_override)
                .await;
            return Err(ApiError::lifx(err));
        }
    }

    info!(
        lifx_id = %format!("{id:#018x}"),
        previous_mode = previous.as_str(),
        mode = mode.as_str(),
        "web light mode changed"
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn start_test_mode_sync(
    state: &WebState,
    id: LifxId,
    light: &ManagedLight,
) -> Result<(), lifx::Error> {
    let (color_name, color) = state.test_mode.current_color();
    let power = state.test_mode.power();
    let transition_started = Instant::now();

    sync_light_to_test_mode(
        &state.client,
        &light.device,
        &state.config,
        &state.test_mode,
        power,
    )
    .await?;

    state.controller.set_desired_color(id, color).await;
    state.controller.set_desired_power(id, Some(power)).await;
    state
        .controller
        .begin_brightness_transition(
            id,
            color.brightness,
            state.config.transition,
            transition_started,
        )
        .await;

    info!(
        lifx_id = %format!("{id:#018x}"),
        color = color_name,
        brightness = color.brightness,
        power = ?power,
        transition_seconds = state.config.transition.as_secs(),
        "started synchronization to current Test Mode state"
    );

    tokio::spawn(confirm_test_mode_sync(
        Arc::clone(&state.client),
        state.controller.clone(),
        Arc::clone(&state.config),
        state.test_mode.clone(),
        light.device.clone(),
    ));

    Ok(())
}

fn require_control_enabled(light: &ManagedLight) -> Result<(), ApiError> {
    if !light.control_enabled {
        return Err(ApiError::conflict(
            "SHOCS control is disabled for this light; re-enable it from Manage",
        ));
    }

    Ok(())
}

fn normalize_friendly_name(value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };

    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    // lifx-lan-rs intentionally mirrors the current NUL-terminated LifxString
    // representation and therefore accepts 31 visible UTF-8 bytes.
    if value.len() > 31 {
        return Err(ApiError::bad_request(
            "friendly name must be 31 UTF-8 bytes or fewer",
        ));
    }

    Ok(Some(value.to_string()))
}

async fn confirm_observation(state: &WebState, id: LifxId, light: &ManagedLight) {
    sleep(MANUAL_CONFIRM_DELAY).await;

    match state.client.get_light_state(&light.device).await {
        Ok(observed) => {
            state.controller.record_observation(id, observed).await;
        }
        Err(err) => trace!(
            lifx_id = %format!("{id:#018x}"),
            error = %err,
            "could not immediately confirm manual web command"
        ),
    }
}

fn sort_name(light: &ManagedLight) -> String {
    light
        .friendly_name
        .as_deref()
        .or(light.label.as_deref())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn to_light_view(light: ManagedLight, online_window: Duration) -> LightView {
    let online = light
        .last_observed
        .is_some_and(|seen| seen.elapsed() <= online_window);
    let now = Instant::now();

    // Interactive controls render SHOCS's current command/intent while the
    // physical observation catches up. Observed state remains available for
    // reconciliation and confirmation instead of making the UI bounce.
    let power_on = light
        .desired_power
        .or_else(|| light.observed.map(|state| state.power))
        .map(|power| matches!(power, Power::On));

    let brightness_percent = light.projected_brightness(now).map(u16_to_percent);

    let brightness_transition = light
        .brightness_transition
        .as_ref()
        .filter(|transition| transition.is_active(now))
        .map(|transition| BrightnessTransitionView {
            to_percent: u16_to_percent(transition.to),
            remaining_ms: transition
                .remaining(now)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        });

    let desired_color = light.desired_color;
    let hue = desired_color
        .map(|color| color.hue)
        .or_else(|| light.observed.map(|state| state.hue));
    let saturation = desired_color
        .map(|color| color.saturation)
        .or_else(|| light.observed.map(|state| state.saturation));
    let kelvin = desired_color
        .map(|color| color.kelvin)
        .or_else(|| light.observed.map(|state| state.kelvin));

    LightView {
        id: format!("{:#018x}", light.device.id),
        label: light
            .friendly_name
            .or(light.label)
            .unwrap_or_else(|| format!("LIFX {:#018x}", light.device.id)),
        address: light.device.addr.to_string(),
        online,
        control_enabled: light.control_enabled,
        mode: light.mode.as_str(),
        power_on,
        brightness_percent,
        brightness_transition,
        hue_degrees: hue.map(u16_to_hue_degrees),
        saturation_percent: saturation.map(u16_to_percent),
        kelvin,
    }
}

fn parse_lifx_id(value: &str) -> Result<LifxId, ApiError> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    u64::from_str_radix(hex, 16).map_err(|_| ApiError::bad_request("invalid LIFX ID"))
}

fn hue_degrees_to_u16(degrees: u16) -> u16 {
    let normalized = u32::from(degrees % 360);
    ((normalized * u32::from(u16::MAX) + 180) / 360) as u16
}

fn u16_to_hue_degrees(value: u16) -> u16 {
    (((u32::from(value) * 360 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) % 360) as u16
}

fn percent_to_u16(percent: u8) -> u16 {
    ((u32::from(percent) * u32::from(u16::MAX) + 50) / 100) as u16
}

fn u16_to_percent(value: u16) -> u8 {
    ((u32::from(value) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)) as u8
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: String,
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    fn lifx(err: lifx::Error) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("LIFX command failed: {err}"),
        }
    }

    fn store(err: StoreError) -> Self {
        error!(error = %err, "persistent light configuration update failed");
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "persistent configuration is temporarily unavailable".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
