use std::sync::Arc;
use std::time::Duration;

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
use crate::tasks::sync_light_to_test_mode;
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

    // Manual control is authoritative: leave automation before sending the
    // physical command so Test Mode cannot immediately fight the user.
    state.controller.set_mode(id, LightMode::Custom).await;

    let power = if request.on { Power::On } else { Power::Off };
    state
        .client
        .set_power(&light.device, power, MANUAL_TRANSITION)
        .await
        .map_err(ApiError::lifx)?;

    confirm_observation(&state, id, &light).await;

    info!(
        lifx_id = %format!("{id:#018x}"),
        power = ?power,
        mode = "custom",
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
        info!(
            lifx_id = %format!("{id:#018x}"),
            "manual live control moved light from Test to Custom mode"
        );
    }

    // Live sliders can issue up to ~10 commands per second. Re-querying the
    // bulb before every command would add latency and serialize the stream on
    // the shared UDP socket, so preserve hue/saturation/kelvin from the most
    // recent observation instead. The normal state poll keeps this cache fresh.
    let observed = match light.observed {
        Some(observed) => observed,
        None => state
            .client
            .get_light_state(&light.device)
            .await
            .map_err(ApiError::lifx)?,
    };

    let brightness = percent_to_u16(request.percent);
    let color = Color::new(
        observed.hue,
        observed.saturation,
        brightness,
        observed.kelvin,
    );

    state
        .client
        .set_color(&light.device, color, LIVE_CONTROL_TRANSITION)
        .await
        .map_err(ApiError::lifx)?;

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

    if mode == LightMode::Test {
        let (color_name, color) = state.test_mode.current_color();
        let power = state.test_mode.power();

        // Joining a mode means adopting its complete current desired state.
        // This keeps every enrolled light synchronized immediately.
        if let Err(err) = sync_light_to_test_mode(
            &state.client,
            &light.device,
            &state.config,
            &state.test_mode,
        )
        .await
        {
            state.controller.set_mode(id, previous).await;
            return Err(ApiError::lifx(err));
        }

        info!(
            lifx_id = %format!("{id:#018x}"),
            color = color_name,
            brightness = color.brightness,
            power = ?power,
            "synchronized light to current Test Mode state"
        );
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
    light
        .label
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn to_light_view(light: ManagedLight, online_window: Duration) -> LightView {
    let online = light
        .last_observed
        .is_some_and(|seen| seen.elapsed() <= online_window);

    let (power_on, brightness_percent) = match light.observed {
        Some(observed) => (
            Some(matches!(observed.power, Power::On)),
            Some(u16_to_percent(observed.brightness)),
        ),
        None => (None, None),
    };

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
    }
}

fn parse_lifx_id(value: &str) -> Result<LifxId, ApiError> {
    let hex = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);

    u64::from_str_radix(hex, 16).map_err(|_| ApiError::bad_request("invalid LIFX ID"))
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
