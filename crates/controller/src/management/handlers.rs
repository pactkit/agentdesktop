use std::time::SystemTime;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_usage::{UsageError, UsageQuery, UsageReport};
use anyhow::Context as _;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{
    Settings, State as ManagementState,
    auth::{
        Context, Error,
        authorization::{self, AccessRequest, Verb},
    },
};
use crate::{
    database::{DeviceDetail, DeviceSummary},
    device_lifecycle::{DeviceLifecycle, LifecycleAudit, Target, Transition},
    jobs::{JobRecord, JobSummary, NewJob},
    usage,
};

#[derive(Serialize)]
pub(super) struct Overview {
    total_devices: usize,
    online_devices: usize,
    offline_devices: usize,
    config_failures: usize,
    active_revision: Option<u64>,
    recent_devices: Vec<DeviceSummary>,
}

pub(super) async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
}

pub(super) async fn overview(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<Overview>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::List,
        resource_class: authorization::DEVICES,
        resource: None,
    })
    .await?;
    let devices = state.database.list_devices().await?;
    let now = unix_time_seconds();
    let online_devices = devices
        .iter()
        .filter(|device| {
            device.state == "active"
                && device
                    .last_seen_at
                    .is_some_and(|last_seen| now - last_seen <= 90)
        })
        .count();
    let config_failures = devices
        .iter()
        .filter(|device| device.config_state == Some(2))
        .count();
    Ok(Json(Overview {
        total_devices: devices.len(),
        online_devices,
        offline_devices: devices.len() - online_devices,
        config_failures,
        active_revision: state.daemon_config.current().map(|config| config.revision),
        recent_devices: devices.into_iter().take(5).collect(),
    }))
}

pub(super) async fn devices(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<Vec<DeviceSummary>>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::List,
        resource_class: authorization::DEVICES,
        resource: None,
    })
    .await?;
    Ok(Json(state.database.list_devices().await?))
}

pub(super) async fn device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceDetail>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    state
        .database
        .get_device(&device_id)
        .await?
        .map(Json)
        .ok_or(HandlerError::NotFound)
}

pub(super) async fn delete_device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<StatusCode, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Delete,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    if !state.database.delete_device(&device_id).await? {
        if state.database.device_lifecycle(&device_id).await?.is_some() {
            return Err(HandlerError::Conflict(
                "inactive devices retain their lifecycle record",
            ));
        }
        return Err(HandlerError::NotFound);
    }
    info!(%device_id, "deleted device from controller");
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn device_lifecycle(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceLifecycle>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    state
        .database
        .device_lifecycle(&device_id)
        .await?
        .map(Json)
        .ok_or(HandlerError::NotFound)
}

pub(super) async fn device_jobs(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<Vec<JobSummary>>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DEVICE_JOBS,
        resource: Some(&device_id),
    })
    .await?;
    if state.database.device_lifecycle(&device_id).await?.is_none() {
        return Err(HandlerError::NotFound);
    }
    Ok(Json(state.database.list_device_jobs(&device_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateDeviceJob {
    agent_id: String,
    version: String,
    action: String,
    idempotency_key: String,
    deadline_unix_seconds: i64,
}

pub(super) async fn create_device_job(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
    Json(input): Json<CreateDeviceJob>,
) -> Result<Json<JobRecord>, HandlerError> {
    use agentdesktop_proto::fleet::JobAction;
    auth.require(AccessRequest {
        verb: Verb::Create,
        resource_class: authorization::DEVICE_JOBS,
        resource: Some(&device_id),
    })
    .await?;
    let now = unix_time_seconds();
    if input.deadline_unix_seconds <= now
        || input.deadline_unix_seconds > now.saturating_add(86400)
        || input.idempotency_key.trim().is_empty()
        || input.idempotency_key.len() > 128
        || input.idempotency_key.chars().any(char::is_control)
    {
        return Err(HandlerError::BadRequest("invalid deadline or idempotency key"));
    }
    let action = match input.action.as_str() {
        "install" => JobAction::Install,
        "update" => JobAction::Update,
        "uninstall" => JobAction::Uninstall,
        _ => return Err(HandlerError::BadRequest("unknown job action")),
    };
    let device = state.database.get_device(&device_id).await?
        .ok_or(HandlerError::NotFound)?.device;
    if device.state != "active" || device.os != "macos" {
        return Err(HandlerError::Conflict("device is inactive or platform installer is unsupported"));
    }
    let snapshot = state.catalog.current()
        .ok_or(HandlerError::Conflict("no signed catalog configured"))?;
    if snapshot.catalog.created_at > now as u64
        || snapshot.catalog.is_expired(now as u64)
        || input.deadline_unix_seconds as u64 > snapshot.catalog.expires_at
    {
        return Err(HandlerError::Conflict("catalog not valid through job deadline"));
    }
    let entry = snapshot.catalog.entries.iter().find(|entry| entry.id == input.agent_id)
        .ok_or(HandlerError::BadRequest("agent not in signed catalog"))?;
    if !entry.platforms.contains(&device.os)
        || !entry.architectures.contains(&device.architecture)
        || !entry.versions.iter().any(|version| version.version == input.version)
    {
        return Err(HandlerError::BadRequest("unsupported device or version"));
    }
    let actor = auth.principal();
    let requested_by = serde_json::to_string(&(&actor.issuer, &actor.subject))
        .context("encode job requester")?;
    let job = NewJob {
        device_id,
        idempotency_key: input.idempotency_key,
        origin: "admin".into(),
        requested_by,
        catalog_id: snapshot.catalog.catalog_id.clone(),
        catalog_revision: i64::try_from(snapshot.catalog.revision)
            .map_err(|_| HandlerError::BadRequest("catalog revision exceeds database range"))?,
        catalog_sha256: snapshot.sha256.clone(),
        agent_id: input.agent_id,
        version: input.version,
        action,
        deadline_unix_seconds: input.deadline_unix_seconds,
    };
    Ok(Json(state.database.create_job(&job).await?))
}

pub(super) async fn device_audit(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<Vec<LifecycleAudit>>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DEVICE_AUDIT,
        resource: Some(&device_id),
    })
    .await?;
    if state.database.device_lifecycle(&device_id).await?.is_none() {
        return Err(HandlerError::NotFound);
    }
    Ok(Json(state.database.lifecycle_audit(&device_id).await?))
}

pub(super) async fn retire_device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceLifecycle>, HandlerError> {
    transition_device(state, auth, device_id, Target::Retired).await
}

pub(super) async fn revoke_device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceLifecycle>, HandlerError> {
    transition_device(state, auth, device_id, Target::Revoked).await
}

async fn transition_device(
    state: ManagementState,
    auth: Context,
    device_id: String,
    target: Target,
) -> Result<Json<DeviceLifecycle>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Update,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    match state
        .database
        .transition_device(&device_id, target, auth.principal())
        .await?
    {
        Transition::Changed(device) | Transition::Unchanged(device) => Ok(Json(device)),
        Transition::Missing => Err(HandlerError::NotFound),
        Transition::Conflict => Err(HandlerError::Conflict(
            "invalid device lifecycle transition",
        )),
    }
}

#[derive(Serialize)]
pub(super) struct SettingsResponse {
    #[serde(flatten)]
    settings: Settings,
    version: &'static str,
}

pub(super) async fn settings(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<SettingsResponse>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::SETTINGS,
        resource: None,
    })
    .await?;
    Ok(Json(SettingsResponse {
        settings: state.settings,
        version: agentdesktop_core::VERSION,
    }))
}

pub(super) async fn usage_report(
    State(state): State<ManagementState>,
    auth: Context,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageReport>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::USAGE,
        resource: None,
    })
    .await?;
    let labels = usage::labels(&state.database).await?;
    Ok(Json(
        agentdesktop_usage::report(
            &state.database.usage(),
            &query,
            state.settings.usage_enabled,
            &labels,
        )
        .await?,
    ))
}

#[derive(Serialize)]
pub(super) struct ActiveDaemonConfig {
    config: Option<DaemonConfig>,
}

pub(super) async fn daemon_config(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<ActiveDaemonConfig>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DAEMON_CONFIG,
        resource: None,
    })
    .await?;
    let config = state
        .daemon_config
        .current()
        .map(|active| {
            let yaml = std::str::from_utf8(&active.yaml).context("daemon config is not UTF-8")?;
            agentdesktop_core::config::parse_daemon(yaml)
        })
        .transpose()?;
    Ok(Json(ActiveDaemonConfig { config }))
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) enum HandlerError {
    Auth(Error),
    Internal(anyhow::Error),
    NotFound,
    BadRequest(&'static str),
    Conflict(&'static str),
}

impl From<Error> for HandlerError {
    fn from(error: Error) -> Self {
        Self::Auth(error)
    }
}
impl From<anyhow::Error> for HandlerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<UsageError> for HandlerError {
    fn from(error: UsageError) -> Self {
        match error {
            UsageError::InvalidQuery(message) => Self::BadRequest(message),
            UsageError::Storage(error) => Self::Internal(error),
        }
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        match self {
            Self::Auth(error) => error.into_response(),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "device not found" })),
            )
                .into_response(),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Self::Conflict(message) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Self::Internal(error) => {
                tracing::error!(%error, "admin API operation failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "controller state error" })),
                )
                    .into_response()
            }
        }
    }
}
