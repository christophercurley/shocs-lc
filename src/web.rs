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
use tracing::{info, trace};

use crate::config::Config;
use crate::registry::{ControllerState, LightMode, ManagedLight};
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
        .route("/static/styles.css", get(stylesheet))
        .route("/static/app.js", get(app_javascript))
        .route("/api/lights", get(list_lights))
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

#[derive(Debug, Serialize)]
struct LightView {
    id: String,
    label: String,
    address: String,
    online: bool,
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

    let previous_mode = state.controller.set_mode(id, LightMode::Custom).await;

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

    let previous_mode = state.controller.set_mode(id, LightMode::Custom).await;

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

    let mode = if request.test {
        LightMode::Test
    } else {
        LightMode::Custom
    };

    let previous = state
        .controller
        .set_mode(id, mode)
        .await
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
        let (color_name, color) = state.test_mode.current_color();
        let power = state.test_mode.power();

        // Joining a mode means adopting its complete current desired state.
        // This keeps every enrolled light synchronized immediately.
        let transition_started = Instant::now();
        if let Err(err) = sync_light_to_test_mode(
            &state.client,
            &light.device,
            &state.config,
            &state.test_mode,
            power,
        )
        .await
        {
            let _ = state.controller.set_mode(id, previous).await;
            let _ = state
                .controller
                .set_power_override(id, previous_override)
                .await;
            return Err(ApiError::lifx(err));
        }

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

        // The regular 10-second observation poll can catch a light halfway
        // through a 5-second fade. Do a dedicated post-transition readback so
        // the controller/UI receives the real final value instead of caching
        // an intermediate brightness such as 80% or 89%.
        tokio::spawn(confirm_test_mode_sync(
            Arc::clone(&state.client),
            state.controller.clone(),
            Arc::clone(&state.config),
            state.test_mode.clone(),
            light.device.clone(),
        ));
    }

    info!(
        lifx_id = %format!("{id:#018x}"),
        previous_mode = previous.as_str(),
        mode = mode.as_str(),
        "web light mode changed"
    );

    Ok(StatusCode::NO_CONTENT)
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
    light.label.as_deref().unwrap_or("").to_ascii_lowercase()
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
            .label
            .unwrap_or_else(|| format!("LIFX {:#018x}", light.device.id)),
        address: light.device.addr.to_string(),
        online,
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

    fn lifx(err: lifx::Error) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: format!("LIFX command failed: {err}"),
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
