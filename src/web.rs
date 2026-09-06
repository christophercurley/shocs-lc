use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::NaiveTime;
use lifx::{Color, LifxClient, LifxId, Power};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{error, info, trace, warn};

use crate::config::Config;
use crate::registry::{ControllerState, LightMode, ManagedLight, TimerSchedule, TimerTarget};
use crate::store::{StoreError, StoredLight};
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
        .route("/groups", get(groups_page))
        .route("/modes", get(modes_page))
        .route("/static/styles.css", get(stylesheet))
        .route("/static/app.js", get(app_javascript))
        .route("/static/manage.js", get(manage_javascript))
        .route("/static/groups.js", get(groups_javascript))
        .route("/static/modes.js", get(modes_javascript))
        .route("/api/lights", get(list_lights))
        .route("/api/manage/lights", get(list_managed_lights))
        .route("/api/manage/lights/{id}", put(update_managed_light))
        .route("/api/groups", get(list_groups).post(create_group))
        .route("/api/groups/{id}", put(update_group).delete(delete_group))
        .route("/api/groups/{id}/power", put(set_group_power))
        .route("/api/groups/{id}/brightness", put(set_group_brightness))
        .route("/api/groups/{id}/color", put(set_group_color))
        .route("/api/groups/{id}/mode", put(set_group_mode))
        .route("/api/modes", get(list_modes))
        .route(
            "/api/modes/timers",
            axum::routing::post(create_timer_schedule),
        )
        .route(
            "/api/modes/timers/{id}",
            put(update_timer_schedule).delete(delete_timer_schedule),
        )
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

async fn groups_page() -> Html<&'static str> {
    Html(include_str!("static/groups.html"))
}

async fn modes_page() -> Html<&'static str> {
    Html(include_str!("static/modes.html"))
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

async fn groups_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/groups.js"),
    )
}

async fn modes_javascript() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("static/modes.js"),
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
    let online_window = state.config.state_poll_interval.saturating_mul(3);

    let mut runtime = state
        .controller
        .lights()
        .await
        .into_iter()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    // PostgreSQL is the durable inventory. A persisted light remains visible
    // when it is unplugged or LC restarts while it is offline; discovery only
    // supplies the optional runtime/network half of the card.
    let configured = state.controller.configured_lights().await;
    let mut views = configured
        .into_iter()
        .map(|stored| {
            if let Some(light) = runtime.remove(&stored.id) {
                to_light_view(light, online_window)
            } else {
                to_offline_light_view(stored)
            }
        })
        .collect::<Vec<_>>();

    // This should normally be empty because runtime insertion happens only
    // after persistence succeeds, but retaining it makes the API robust during
    // an unusual partial persistence/recovery situation.
    views.extend(
        runtime
            .into_values()
            .map(|light| to_light_view(light, online_window)),
    );

    views.sort_by_key(|light| light.label.to_ascii_lowercase());
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

#[derive(Debug, Serialize)]
struct GroupLightOption {
    id: String,
    display_name: String,
    control_enabled: bool,
    online: bool,
}

#[derive(Debug, Serialize)]
struct GroupView {
    id: i64,
    name: String,
    member_ids: Vec<String>,
    member_count: usize,
    control_enabled_count: usize,
    online_count: usize,
    online_control_enabled_count: usize,
    mode_state: Option<&'static str>,
    power_state: Option<&'static str>,
    brightness_percent: Option<u8>,
    brightness_transition: Option<BrightnessTransitionView>,
    hue_degrees: Option<u16>,
    saturation_percent: Option<u8>,
    kelvin: Option<u16>,
}

#[derive(Debug, Serialize)]
struct GroupsView {
    groups: Vec<GroupView>,
    lights: Vec<GroupLightOption>,
}

#[derive(Debug, Deserialize)]
struct CreateGroupRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct UpdateGroupRequest {
    name: Option<String>,
    member_ids: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct CreateGroupResult {
    id: i64,
    name: String,
}

async fn list_groups(State(state): State<WebState>) -> Json<GroupsView> {
    let online_window = state.config.state_poll_interval.saturating_mul(3);
    let runtime_lights = state.controller.lights().await;
    let runtime = runtime_lights
        .iter()
        .cloned()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    let configured = state.controller.configured_lights().await;
    let configured_by_id = configured
        .iter()
        .cloned()
        .map(|light| (light.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    let mut light_options = configured
        .iter()
        .map(|light| {
            let runtime_light = runtime.get(&light.id);
            GroupLightOption {
                id: format!("{:#018x}", light.id),
                display_name: light
                    .friendly_name
                    .clone()
                    .or_else(|| light.device_label.clone())
                    .unwrap_or_else(|| format!("LIFX {:#018x}", light.id)),
                control_enabled: light.control_enabled,
                online: runtime_light
                    .and_then(|light| light.last_observed)
                    .is_some_and(|seen| seen.elapsed() <= online_window),
            }
        })
        .collect::<Vec<_>>();
    light_options.sort_by_key(|light| light.display_name.to_ascii_lowercase());

    let mut groups = state.controller.groups().await;
    groups.sort_by_key(|group| group.name.to_ascii_lowercase());

    let views = groups
        .into_iter()
        .map(|group| {
            let member_count = group.member_ids.len();
            let members = group
                .member_ids
                .iter()
                .filter_map(|id| runtime.get(id))
                .collect::<Vec<_>>();

            let controllable = members
                .iter()
                .copied()
                .filter(|light| light.control_enabled)
                .collect::<Vec<_>>();

            let online_count = members
                .iter()
                .filter(|light| {
                    light
                        .last_observed
                        .is_some_and(|seen| seen.elapsed() <= online_window)
                })
                .count();

            let configured_members = group
                .member_ids
                .iter()
                .filter_map(|id| configured_by_id.get(id))
                .collect::<Vec<_>>();

            let control_enabled_count = configured_members
                .iter()
                .filter(|light| light.control_enabled)
                .count();

            let online_control_enabled_count = controllable
                .iter()
                .filter(|light| {
                    light
                        .last_observed
                        .is_some_and(|seen| seen.elapsed() <= online_window)
                })
                .count();

            let mut modes = configured_members.iter().map(|light| light.mode);
            let first_mode = modes.next();
            let mode_state = match first_mode {
                Some(first) if modes.all(|mode| mode == first) => Some(first.as_str()),
                Some(_) => Some("mixed"),
                None => None,
            };

            let mut power_values = controllable.iter().filter_map(|light| {
                light
                    .desired_power
                    .or_else(|| light.observed.map(|state| state.power))
            });
            let first_power = power_values.next();
            let power_state = match first_power {
                Some(first) if power_values.all(|power| power == first) => Some(match first {
                    Power::On => "on",
                    Power::Off => "off",
                }),
                Some(_) => Some("mixed"),
                None => None,
            };

            let now = Instant::now();
            let brightness_values = controllable
                .iter()
                .filter_map(|light| light.projected_brightness(now))
                .map(u32::from)
                .collect::<Vec<_>>();
            let brightness_percent = if brightness_values.is_empty() {
                None
            } else {
                let average =
                    brightness_values.iter().copied().sum::<u32>() / brightness_values.len() as u32;
                Some(u16_to_percent(average.min(u32::from(u16::MAX)) as u16))
            };

            // Mirror the individual-light API's transition metadata so the
            // group slider can animate to a mode-owned brightness target instead
            // of freezing at the transition's starting value.
            let active_transitions = controllable
                .iter()
                .filter_map(|light| light.brightness_transition.as_ref())
                .filter(|transition| transition.is_active(now))
                .collect::<Vec<_>>();

            let brightness_transition = if active_transitions.is_empty() {
                None
            } else {
                let average_target = active_transitions
                    .iter()
                    .map(|transition| u32::from(transition.to))
                    .sum::<u32>()
                    / active_transitions.len() as u32;
                let remaining = active_transitions
                    .iter()
                    .map(|transition| transition.remaining(now))
                    .max()
                    .unwrap_or_default();

                Some(BrightnessTransitionView {
                    to_percent: u16_to_percent(average_target.min(u32::from(u16::MAX)) as u16),
                    remaining_ms: remaining.as_millis().min(u128::from(u64::MAX)) as u64,
                })
            };

            let color_source = controllable.first().copied();
            let hue = color_source.and_then(|light| {
                light
                    .desired_color
                    .map(|color| color.hue)
                    .or_else(|| light.observed.map(|state| state.hue))
            });
            let saturation = color_source.and_then(|light| {
                light
                    .desired_color
                    .map(|color| color.saturation)
                    .or_else(|| light.observed.map(|state| state.saturation))
            });
            let kelvin = color_source.and_then(|light| {
                light
                    .desired_color
                    .map(|color| color.kelvin)
                    .or_else(|| light.observed.map(|state| state.kelvin))
            });

            GroupView {
                id: group.id,
                name: group.name,
                member_ids: group
                    .member_ids
                    .into_iter()
                    .map(|id| format!("{id:#018x}"))
                    .collect(),
                member_count,
                control_enabled_count,
                online_count,
                online_control_enabled_count,
                mode_state,
                power_state,
                brightness_percent,
                brightness_transition,
                hue_degrees: hue.map(u16_to_hue_degrees),
                saturation_percent: saturation.map(u16_to_percent),
                kelvin,
            }
        })
        .collect();

    Json(GroupsView {
        groups: views,
        lights: light_options,
    })
}

async fn create_group(
    State(state): State<WebState>,
    Json(request): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<CreateGroupResult>), ApiError> {
    let name = normalize_group_name(request.name)?;
    let group = state
        .controller
        .create_group(name)
        .await
        .map_err(ApiError::store)?;

    info!(group_id = group.id, group_name = %group.name, "light group created");

    Ok((
        StatusCode::CREATED,
        Json(CreateGroupResult {
            id: group.id,
            name: group.name,
        }),
    ))
}

async fn update_group(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<UpdateGroupRequest>,
) -> Result<StatusCode, ApiError> {
    if request.name.is_none() && request.member_ids.is_none() {
        return Err(ApiError::bad_request(
            "no group-management fields were supplied",
        ));
    }

    let previous_group = state
        .controller
        .group(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light group"))?;

    if let Some(name) = request.name {
        let name = normalize_group_name(name)?;
        state
            .controller
            .rename_group(id, name.clone())
            .await
            .map_err(ApiError::store)?;
        info!(group_id = id, group_name = %name, "light group renamed");
    }

    if let Some(member_ids) = request.member_ids {
        let mut parsed = Vec::with_capacity(member_ids.len());
        for member_id in member_ids {
            parsed.push(parse_lifx_id(&member_id)?);
        }
        parsed.sort_unstable();
        parsed.dedup();

        let timer = state
            .controller
            .timer_for_target(TimerTarget::Group(id))
            .await
            .filter(|schedule| schedule.enabled);

        if let Some(schedule) = &timer {
            ensure_timer_members_overlap_free(&state, &parsed, Some(schedule.id)).await?;
        }

        state
            .controller
            .set_group_members(id, parsed.clone())
            .await
            .map_err(ApiError::store)?;

        if timer.is_some() {
            let current = parsed
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            let removed = previous_group
                .member_ids
                .iter()
                .copied()
                .filter(|member_id| !current.contains(member_id))
                .collect::<Vec<_>>();

            state
                .controller
                .set_mode_for_ids(&removed, LightMode::Custom)
                .await
                .map_err(ApiError::store)?;
            state
                .controller
                .set_mode_for_ids(&parsed, LightMode::Timer)
                .await
                .map_err(ApiError::store)?;
        }

        info!(
            group_id = id,
            members = parsed.len(),
            timer_mode = timer.is_some(),
            "light group membership updated"
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn delete_group(
    State(state): State<WebState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let group = state
        .controller
        .group(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light group"))?;
    let had_enabled_timer = state
        .controller
        .timer_for_target(TimerTarget::Group(id))
        .await
        .is_some_and(|schedule| schedule.enabled);

    state
        .controller
        .delete_group(id)
        .await
        .map_err(ApiError::store)?;

    if had_enabled_timer {
        state
            .controller
            .set_mode_for_ids(&group.member_ids, LightMode::Custom)
            .await
            .map_err(ApiError::store)?;
    }

    info!(group_id = id, "light group deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn controlled_group_lights(
    state: &WebState,
    group_id: i64,
) -> Result<Vec<ManagedLight>, ApiError> {
    let lights = state
        .controller
        .lights_in_group(group_id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light group"))?;

    let lights = lights
        .into_iter()
        .filter(|light| light.control_enabled)
        .collect::<Vec<_>>();

    if lights.is_empty() {
        return Err(ApiError::conflict(
            "group has no currently controllable lights",
        ));
    }

    Ok(lights)
}

async fn set_group_power(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<PowerRequest>,
) -> Result<StatusCode, ApiError> {
    let lights = controlled_group_lights(&state, id).await?;
    let power = if request.on { Power::On } else { Power::Off };
    let devices = lights
        .iter()
        .map(|light| light.device.clone())
        .collect::<Vec<_>>();

    state
        .client
        .set_power_many(&devices, power, MANUAL_TRANSITION)
        .await
        .map_err(ApiError::lifx)?;

    for light in &lights {
        if light.mode.supports_power_override() {
            let _ = state
                .controller
                .set_power_override(light.device.id, Some(power))
                .await;
        }
        let _ = state
            .controller
            .set_desired_power(light.device.id, Some(power))
            .await;
    }

    info!(
        group_id = id,
        lights = lights.len(),
        power = ?power,
        "manual group power command"
    );

    Ok(StatusCode::NO_CONTENT)
}

async fn set_group_brightness(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<BrightnessRequest>,
) -> Result<StatusCode, ApiError> {
    if request.percent > 100 {
        return Err(ApiError::bad_request("brightness percent must be 0-100"));
    }

    let lights = controlled_group_lights(&state, id).await?;
    let brightness = percent_to_u16(request.percent);

    for light in &lights {
        let previous_mode = state
            .controller
            .set_mode(light.device.id, LightMode::Custom)
            .await
            .map_err(ApiError::store)?;

        if previous_mode.is_some_and(|mode| mode.supports_power_override()) {
            let _ = state
                .controller
                .set_power_override(light.device.id, None)
                .await;
        }

        let observed = match light.observed {
            Some(observed) => observed,
            None => state
                .client
                .get_light_state(&light.device)
                .await
                .map_err(ApiError::lifx)?,
        };

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
            .set_desired_brightness(light.device.id, brightness)
            .await;
    }

    if request.final_update {
        info!(
            group_id = id,
            lights = lights.len(),
            brightness_percent = request.percent,
            "manual group brightness command committed"
        );
    } else {
        trace!(
            group_id = id,
            lights = lights.len(),
            brightness_percent = request.percent,
            "manual group brightness live update"
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

async fn set_group_color(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<ColorRequest>,
) -> Result<StatusCode, ApiError> {
    if request.hue_degrees > 360 {
        return Err(ApiError::bad_request("hue must be 0-360 degrees"));
    }
    if request.saturation_percent > 100 {
        return Err(ApiError::bad_request("saturation percent must be 0-100"));
    }

    let lights = controlled_group_lights(&state, id).await?;
    let hue = hue_degrees_to_u16(request.hue_degrees);
    let saturation = percent_to_u16(request.saturation_percent);

    for light in &lights {
        let previous_mode = state
            .controller
            .set_mode(light.device.id, LightMode::Custom)
            .await
            .map_err(ApiError::store)?;

        if previous_mode.is_some_and(|mode| mode.supports_power_override()) {
            let _ = state
                .controller
                .set_power_override(light.device.id, None)
                .await;
        }

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
        let color = Color::new(hue, saturation, brightness, kelvin);

        state
            .client
            .set_color(&light.device, color, LIVE_CONTROL_TRANSITION)
            .await
            .map_err(ApiError::lifx)?;

        state
            .controller
            .set_desired_color(light.device.id, color)
            .await;
        state
            .controller
            .set_desired_brightness(light.device.id, brightness)
            .await;
    }

    if request.final_update {
        info!(
            group_id = id,
            lights = lights.len(),
            hue_degrees = request.hue_degrees,
            saturation_percent = request.saturation_percent,
            "manual group color command committed"
        );
    } else {
        trace!(
            group_id = id,
            lights = lights.len(),
            hue_degrees = request.hue_degrees,
            saturation_percent = request.saturation_percent,
            "manual group color live update"
        );
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
struct GroupModeResult {
    mode: &'static str,
    members: usize,
    synced_now: usize,
    pending_sync: usize,
}

async fn set_group_mode(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<ModeRequest>,
) -> Result<Json<GroupModeResult>, ApiError> {
    let group = state
        .controller
        .group(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light group"))?;

    if group.member_ids.is_empty() {
        return Err(ApiError::conflict("group has no members"));
    }

    let mode = if request.test {
        LightMode::Test
    } else {
        LightMode::Custom
    };

    let updated_ids = state
        .controller
        .set_group_mode(id, mode)
        .await
        .map_err(ApiError::store)?;

    let runtime = state
        .controller
        .lights_in_group(id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    let mut synced_now = 0usize;
    let mut pending_sync = 0usize;

    for light_id in &updated_ids {
        let Some(light) = runtime.get(light_id) else {
            // Mode is already durable. This light will pick it up whenever it
            // is discovered again.
            pending_sync += 1;
            continue;
        };

        if !light.control_enabled {
            pending_sync += 1;
            continue;
        }

        let _ = state.controller.set_power_override(*light_id, None).await;

        if mode == LightMode::Test {
            match start_test_mode_sync(&state, *light_id, light).await {
                Ok(()) => synced_now += 1,
                Err(err) => {
                    pending_sync += 1;
                    warn!(
                        group_id = id,
                        lifx_id = %format!("{light_id:#018x}"),
                        error = %err,
                        "group Test Mode enrollment persisted; physical sync will retry later"
                    );
                }
            }
        } else {
            // Custom has no mode-owned physical state to push immediately.
            synced_now += 1;
        }
    }

    info!(
        group_id = id,
        group_name = %group.name,
        members = updated_ids.len(),
        mode = mode.as_str(),
        synced_now,
        pending_sync,
        "light group mode changed"
    );

    Ok(Json(GroupModeResult {
        mode: mode.as_str(),
        members: updated_ids.len(),
        synced_now,
        pending_sync,
    }))
}

#[derive(Debug, Serialize)]
struct ModesView {
    timezone: String,
    test_mode: TestModeView,
    targets: Vec<TimerTargetView>,
    timers: Vec<TimerScheduleView>,
}

#[derive(Debug, Serialize)]
struct TestModeView {
    on_time: String,
    off_time: String,
    color_interval_seconds: u64,
    transition_seconds: u64,
}

#[derive(Debug, Serialize)]
struct TimerTargetView {
    target_type: &'static str,
    target_id: String,
    display_name: String,
    member_count: usize,
}

#[derive(Debug, Serialize)]
struct TimerScheduleView {
    id: i64,
    target_type: &'static str,
    target_id: String,
    target_name: String,
    on_time: String,
    off_time: String,
    timezone: String,
    enabled: bool,
    member_count: usize,
    online_count: usize,
    mode_state: Option<&'static str>,
    scheduled_power: &'static str,
}

#[derive(Debug, Deserialize)]
struct TimerScheduleRequest {
    target_type: String,
    target_id: String,
    on_time: String,
    off_time: String,
    enabled: bool,
}

async fn list_modes(State(state): State<WebState>) -> Result<Json<ModesView>, ApiError> {
    let configured = state.controller.configured_lights().await;
    let mut targets = configured
        .iter()
        .map(|light| TimerTargetView {
            target_type: "light",
            target_id: format!("{:016x}", light.id),
            display_name: light
                .friendly_name
                .clone()
                .or_else(|| light.device_label.clone())
                .unwrap_or_else(|| format!("LIFX {:016x}", light.id)),
            member_count: 1,
        })
        .collect::<Vec<_>>();

    let mut groups = state.controller.groups().await;
    groups.sort_by_key(|group| group.name.to_ascii_lowercase());
    targets.extend(groups.iter().map(|group| TimerTargetView {
        target_type: "group",
        target_id: group.id.to_string(),
        display_name: group.name.clone(),
        member_count: group.member_ids.len(),
    }));
    targets.sort_by_key(|target| target.display_name.to_ascii_lowercase());

    let online_window = state.config.state_poll_interval.saturating_mul(3);
    let runtime = state
        .controller
        .lights()
        .await
        .into_iter()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();
    let configured_by_id = configured
        .into_iter()
        .map(|light| (light.id, light))
        .collect::<std::collections::HashMap<_, _>>();

    let mut timers = state.controller.timer_schedules().await;
    timers.sort_by_key(|schedule| schedule.id);
    let mut timer_views = Vec::with_capacity(timers.len());

    for schedule in timers {
        let member_ids = state
            .controller
            .timer_target_member_ids(schedule.target)
            .await;
        let configured_members = member_ids
            .iter()
            .filter_map(|id| configured_by_id.get(id))
            .collect::<Vec<_>>();
        let mut modes = configured_members.iter().map(|light| light.mode);
        let first_mode = modes.next();
        let mode_state = match first_mode {
            Some(first) if modes.all(|mode| mode == first) => Some(first.as_str()),
            Some(_) => Some("mixed"),
            None => None,
        };
        let online_count = member_ids
            .iter()
            .filter_map(|id| runtime.get(id))
            .filter(|light| {
                light
                    .last_observed
                    .is_some_and(|seen| seen.elapsed() <= online_window)
            })
            .count();
        let (target_type, target_id, target_name) =
            timer_target_identity(&state, schedule.target).await;
        let scheduled_power = crate::schedule::desired_power_now(
            schedule.timezone,
            schedule.off_time,
            schedule.on_time,
        );

        timer_views.push(TimerScheduleView {
            id: schedule.id,
            target_type,
            target_id,
            target_name,
            on_time: schedule.on_time.format("%H:%M").to_string(),
            off_time: schedule.off_time.format("%H:%M").to_string(),
            timezone: schedule.timezone.to_string(),
            enabled: schedule.enabled,
            member_count: member_ids.len(),
            online_count,
            mode_state,
            scheduled_power: match scheduled_power {
                Power::On => "on",
                Power::Off => "off",
            },
        });
    }

    Ok(Json(ModesView {
        timezone: state.config.timezone.to_string(),
        test_mode: TestModeView {
            on_time: state.config.on_time.format("%H:%M").to_string(),
            off_time: state.config.off_time.format("%H:%M").to_string(),
            color_interval_seconds: state.config.color_interval.as_secs(),
            transition_seconds: state.config.transition.as_secs(),
        },
        targets,
        timers: timer_views,
    }))
}

async fn create_timer_schedule(
    State(state): State<WebState>,
    Json(request): Json<TimerScheduleRequest>,
) -> Result<(StatusCode, Json<TimerScheduleView>), ApiError> {
    let target = parse_timer_target(&state, &request.target_type, &request.target_id).await?;
    let on_time = parse_timer_time("on time", &request.on_time)?;
    let off_time = parse_timer_time("off time", &request.off_time)?;
    validate_timer_times(on_time, off_time)?;
    let member_ids = require_timer_target_members(&state, target).await?;

    if request.enabled {
        ensure_timer_overlap_free(&state, target, None).await?;
    }

    let schedule = state
        .controller
        .create_timer_schedule(
            target,
            on_time,
            off_time,
            state.config.timezone,
            request.enabled,
        )
        .await
        .map_err(ApiError::store)?;

    if request.enabled {
        if let Err(err) = state
            .controller
            .set_mode_for_ids(&member_ids, LightMode::Timer)
            .await
        {
            let _ = state.controller.delete_timer_schedule(schedule.id).await;
            return Err(ApiError::store(err));
        }
    }

    info!(
        timer_id = schedule.id,
        target = %format_timer_target(schedule.target),
        on_time = %schedule.on_time.format("%H:%M"),
        off_time = %schedule.off_time.format("%H:%M"),
        enabled = schedule.enabled,
        "Timer Mode schedule created"
    );

    let view = timer_schedule_view(&state, schedule).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

async fn update_timer_schedule(
    State(state): State<WebState>,
    Path(id): Path<i64>,
    Json(request): Json<TimerScheduleRequest>,
) -> Result<Json<TimerScheduleView>, ApiError> {
    let previous = state
        .controller
        .timer_schedule(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown timer schedule"))?;
    let target = parse_timer_target(&state, &request.target_type, &request.target_id).await?;
    let on_time = parse_timer_time("on time", &request.on_time)?;
    let off_time = parse_timer_time("off time", &request.off_time)?;
    validate_timer_times(on_time, off_time)?;
    let new_members = require_timer_target_members(&state, target).await?;

    if request.enabled {
        ensure_timer_overlap_free(&state, target, Some(id)).await?;
    }

    let old_members = state
        .controller
        .timer_target_member_ids(previous.target)
        .await;

    let schedule = state
        .controller
        .update_timer_schedule(
            id,
            target,
            on_time,
            off_time,
            state.config.timezone,
            request.enabled,
        )
        .await
        .map_err(ApiError::store)?;

    if previous.enabled {
        state
            .controller
            .set_mode_for_ids(&old_members, LightMode::Custom)
            .await
            .map_err(ApiError::store)?;
    }
    if request.enabled {
        state
            .controller
            .set_mode_for_ids(&new_members, LightMode::Timer)
            .await
            .map_err(ApiError::store)?;
    }

    info!(
        timer_id = id,
        target = %format_timer_target(schedule.target),
        on_time = %schedule.on_time.format("%H:%M"),
        off_time = %schedule.off_time.format("%H:%M"),
        enabled = schedule.enabled,
        "Timer Mode schedule updated"
    );

    Ok(Json(timer_schedule_view(&state, schedule).await?))
}

async fn delete_timer_schedule(
    State(state): State<WebState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let schedule = state
        .controller
        .timer_schedule(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown timer schedule"))?;
    let members = state
        .controller
        .timer_target_member_ids(schedule.target)
        .await;

    state
        .controller
        .delete_timer_schedule(id)
        .await
        .map_err(ApiError::store)?;

    if schedule.enabled {
        state
            .controller
            .set_mode_for_ids(&members, LightMode::Custom)
            .await
            .map_err(ApiError::store)?;
    }

    info!(timer_id = id, "Timer Mode schedule deleted");
    Ok(StatusCode::NO_CONTENT)
}

async fn timer_schedule_view(
    state: &WebState,
    schedule: TimerSchedule,
) -> Result<TimerScheduleView, ApiError> {
    let member_ids = state
        .controller
        .timer_target_member_ids(schedule.target)
        .await;
    let configured = state.controller.configured_lights().await;
    let configured_by_id = configured
        .into_iter()
        .map(|light| (light.id, light))
        .collect::<std::collections::HashMap<_, _>>();
    let configured_members = member_ids
        .iter()
        .filter_map(|id| configured_by_id.get(id))
        .collect::<Vec<_>>();
    let mut modes = configured_members.iter().map(|light| light.mode);
    let first_mode = modes.next();
    let mode_state = match first_mode {
        Some(first) if modes.all(|mode| mode == first) => Some(first.as_str()),
        Some(_) => Some("mixed"),
        None => None,
    };
    let online_window = state.config.state_poll_interval.saturating_mul(3);
    let runtime = state
        .controller
        .lights()
        .await
        .into_iter()
        .map(|light| (light.device.id, light))
        .collect::<std::collections::HashMap<_, _>>();
    let online_count = member_ids
        .iter()
        .filter_map(|id| runtime.get(id))
        .filter(|light| {
            light
                .last_observed
                .is_some_and(|seen| seen.elapsed() <= online_window)
        })
        .count();
    let (target_type, target_id, target_name) = timer_target_identity(state, schedule.target).await;
    let scheduled_power =
        crate::schedule::desired_power_now(schedule.timezone, schedule.off_time, schedule.on_time);

    Ok(TimerScheduleView {
        id: schedule.id,
        target_type,
        target_id,
        target_name,
        on_time: schedule.on_time.format("%H:%M").to_string(),
        off_time: schedule.off_time.format("%H:%M").to_string(),
        timezone: schedule.timezone.to_string(),
        enabled: schedule.enabled,
        member_count: member_ids.len(),
        online_count,
        mode_state,
        scheduled_power: match scheduled_power {
            Power::On => "on",
            Power::Off => "off",
        },
    })
}

async fn timer_target_identity(
    state: &WebState,
    target: TimerTarget,
) -> (&'static str, String, String) {
    match target {
        TimerTarget::Light(id) => {
            let name = state
                .controller
                .configured_light(id)
                .await
                .and_then(|light| light.friendly_name.or(light.device_label))
                .unwrap_or_else(|| format!("LIFX {id:016x}"));
            ("light", format!("{id:016x}"), name)
        }
        TimerTarget::Group(id) => {
            let name = state
                .controller
                .group(id)
                .await
                .map(|group| group.name)
                .unwrap_or_else(|| format!("Group {id}"));
            ("group", id.to_string(), name)
        }
    }
}

async fn parse_timer_target(
    state: &WebState,
    target_type: &str,
    target_id: &str,
) -> Result<TimerTarget, ApiError> {
    match target_type {
        "light" => {
            let id = parse_lifx_id(target_id)?;
            if state.controller.configured_light(id).await.is_none() {
                return Err(ApiError::not_found("unknown timer light target"));
            }
            Ok(TimerTarget::Light(id))
        }
        "group" => {
            let id = target_id
                .parse::<i64>()
                .map_err(|_| ApiError::bad_request("invalid timer group target"))?;
            if state.controller.group(id).await.is_none() {
                return Err(ApiError::not_found("unknown timer group target"));
            }
            Ok(TimerTarget::Group(id))
        }
        _ => Err(ApiError::bad_request(
            "timer target type must be 'light' or 'group'",
        )),
    }
}

async fn require_timer_target_members(
    state: &WebState,
    target: TimerTarget,
) -> Result<Vec<LifxId>, ApiError> {
    let ids = state.controller.timer_target_member_ids(target).await;
    if ids.is_empty() {
        return Err(ApiError::conflict("timer target has no lights"));
    }
    Ok(ids)
}

async fn ensure_timer_overlap_free(
    state: &WebState,
    target: TimerTarget,
    exclude_id: Option<i64>,
) -> Result<(), ApiError> {
    let member_ids = state.controller.timer_target_member_ids(target).await;
    ensure_timer_members_overlap_free(state, &member_ids, exclude_id).await
}

async fn ensure_timer_members_overlap_free(
    state: &WebState,
    member_ids: &[LifxId],
    exclude_id: Option<i64>,
) -> Result<(), ApiError> {
    let candidate = member_ids
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();

    for schedule in state.controller.timer_schedules().await {
        if !schedule.enabled || Some(schedule.id) == exclude_id {
            continue;
        }
        let other = state
            .controller
            .timer_target_member_ids(schedule.target)
            .await;
        if let Some(overlap) = other.into_iter().find(|id| candidate.contains(id)) {
            return Err(ApiError::conflict(format!(
                "Timer target overlaps enabled timer {} on light {overlap:016x}",
                schedule.id
            )));
        }
    }

    Ok(())
}

fn parse_timer_time(label: &str, value: &str) -> Result<NaiveTime, ApiError> {
    NaiveTime::parse_from_str(value, "%H:%M")
        .map_err(|_| ApiError::bad_request(format!("invalid {label}; expected HH:MM")))
}

fn validate_timer_times(on_time: NaiveTime, off_time: NaiveTime) -> Result<(), ApiError> {
    if on_time == off_time {
        return Err(ApiError::bad_request("Timer ON and OFF times must differ"));
    }
    Ok(())
}

fn format_timer_target(target: TimerTarget) -> String {
    match target {
        TimerTarget::Light(id) => format!("light:{id:016x}"),
        TimerTarget::Group(id) => format!("group:{id}"),
    }
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

    // Power is intentionally orthogonal to automation mode. In Test or Timer
    // Mode, a manual power command becomes a temporary per-light override
    // instead of ejecting the light from the mode. The next owning power
    // schedule boundary clears the override and resumes scheduled power.
    let previous_override = if light.mode.supports_power_override() {
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

        if light.mode.supports_power_override() {
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
        power_override = light.mode.supports_power_override(),
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

    if previous_mode.is_some_and(|mode| mode.supports_power_override()) {
        let _ = state.controller.set_power_override(id, None).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            "manual live control moved light from automation to Custom mode"
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

    if previous_mode.is_some_and(|mode| mode.supports_power_override()) {
        let _ = state.controller.set_power_override(id, None).await;

        info!(
            lifx_id = %format!("{id:#018x}"),
            "manual color control moved light from automation to Custom mode"
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
    let configured = state
        .controller
        .configured_light(id)
        .await
        .ok_or_else(|| ApiError::not_found("unknown light"))?;

    if !configured.control_enabled {
        return Err(ApiError::conflict(
            "SHOCS control is disabled for this light; re-enable it from Manage",
        ));
    }

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

    // An offline light can still change durable mode. Physical synchronization
    // is simply deferred until discovery gives us a current network endpoint.
    if let Some(light) = state.controller.light(id).await {
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
    }

    // Keep awaits out of tracing macro arguments. Axum requires handler
    // futures to be Send, and tracing's macro temporaries can otherwise be held
    // across the await and make this handler future non-Send.
    let online = state.controller.light(id).await.is_some();

    info!(
        lifx_id = %format!("{id:#018x}"),
        previous_mode = previous.as_str(),
        mode = mode.as_str(),
        online,
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

fn normalize_group_name(value: String) -> Result<String, ApiError> {
    let value = value.trim();

    if value.is_empty() {
        return Err(ApiError::bad_request("group name cannot be empty"));
    }

    if value.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "group name cannot contain control characters",
        ));
    }

    if value.len() > 64 {
        return Err(ApiError::bad_request(
            "group name must be 64 UTF-8 bytes or fewer",
        ));
    }

    Ok(value.to_string())
}

fn normalize_friendly_name(value: Option<String>) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };

    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    if value.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "friendly name cannot contain control characters",
        ));
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

fn to_offline_light_view(light: StoredLight) -> LightView {
    LightView {
        id: format!("{:#018x}", light.id),
        label: light
            .friendly_name
            .or(light.device_label)
            .unwrap_or_else(|| format!("LIFX {:#018x}", light.id)),
        address: "Not currently discovered".to_string(),
        online: false,
        control_enabled: light.control_enabled,
        mode: light.mode.as_str(),
        power_on: None,
        brightness_percent: None,
        brightness_transition: None,
        hue_degrees: None,
        saturation_percent: None,
        kelvin: None,
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
        match err {
            StoreError::FriendlyNameConflict(name) => Self::conflict(format!(
                "A light named '{name}' already exists. Friendly names must be unique."
            )),
            StoreError::InvalidFriendlyName(_) => Self::bad_request(
                "friendly name is invalid; trim whitespace and use 31 UTF-8 bytes or fewer",
            ),
            StoreError::GroupNameConflict(name) => Self::conflict(format!(
                "A group named '{name}' already exists. Group names must be unique."
            )),
            StoreError::InvalidGroupName(_) => Self::bad_request(
                "group name is invalid; trim whitespace and use 64 UTF-8 bytes or fewer",
            ),
            StoreError::UnknownGroup(_) => Self::not_found("unknown light group"),
            StoreError::UnknownTimerSchedule(_) => Self::not_found("unknown timer schedule"),
            StoreError::TimerTargetConflict(_) => {
                Self::conflict("that target already has a Timer schedule")
            }
            StoreError::InvalidTimerTarget(_) | StoreError::InvalidTimerTimezone(_) => {
                Self::bad_request("invalid Timer schedule configuration")
            }
            other => {
                error!(error = %other, "persistent light configuration update failed");
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: "persistent configuration is temporarily unavailable".to_string(),
                }
            }
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
