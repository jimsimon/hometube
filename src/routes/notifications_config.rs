//! Parent-only REST API for the self-hosted notification forwarder.
//!
//! Endpoints (registered in [`crate::routes::mod`]):
//!
//! - `GET /api/notifications/config` — current settings with secrets
//!   redacted.
//! - `PUT /api/notifications/config` — replace settings. Any secret
//!   field whose value equals
//!   [`crate::services::notification_forwarders::SECRET_PLACEHOLDER`]
//!   is merged back to the previously stored value, so callers can
//!   round-trip a GET into a PUT without re-typing secrets.
//! - `POST /api/notifications/config/test` — send a synthetic
//!   notification to the configured provider and return success/failure.

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::notification_forwarders::{self, ForwardingSettings, KNOWN_TYPES};
use crate::state::AppState;

/// `GET /api/notifications/config`.
pub async fn get_config(State(state): State<AppState>) -> AppResult<Json<ConfigResponse>> {
    let settings = notification_forwarders::load(&state.db).await?;
    Ok(Json(ConfigResponse {
        settings: notification_forwarders::redact(&settings),
        known_types: KNOWN_TYPES.iter().map(|s| s.to_string()).collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub settings: ForwardingSettings,
    pub known_types: Vec<String>,
}

/// `PUT /api/notifications/config`.
pub async fn put_config(
    State(state): State<AppState>,
    Json(incoming): Json<ForwardingSettings>,
) -> AppResult<Json<ConfigResponse>> {
    let existing = notification_forwarders::load(&state.db).await?;
    let merged = notification_forwarders::merge_secrets(&existing, incoming);
    notification_forwarders::validate(&merged)?;
    notification_forwarders::save(&state.db, &merged).await?;
    Ok(Json(ConfigResponse {
        settings: notification_forwarders::redact(&merged),
        known_types: KNOWN_TYPES.iter().map(|s| s.to_string()).collect(),
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct TestRequest {
    #[serde(default)]
    pub notification_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `POST /api/notifications/config/test`.
pub async fn test(
    State(state): State<AppState>,
    Json(body): Json<TestRequest>,
) -> AppResult<Json<TestResponse>> {
    let settings = notification_forwarders::load(&state.db).await?;
    let Some(provider) = settings.provider else {
        return Err(AppError::BadRequest(
            "no notification provider is configured".into(),
        ));
    };
    let kind = body
        .notification_type
        .as_deref()
        .unwrap_or("system_update")
        .to_string();
    if !KNOWN_TYPES.contains(&kind.as_str()) {
        return Err(AppError::BadRequest(format!(
            "unknown notification type: {kind}"
        )));
    }
    let result = notification_forwarders::send(
        notification_forwarders::shared_client(),
        &provider,
        "HomeTube test notification",
        "If you can read this, your notification forwarder is configured correctly.",
        &kind,
    )
    .await;
    match result {
        Ok(()) => Ok(Json(TestResponse {
            ok: true,
            error: None,
        })),
        Err(err) => Ok(Json(TestResponse {
            ok: false,
            error: Some(err),
        })),
    }
}
