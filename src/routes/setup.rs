//! Setup-wizard API.
//!
//! - `GET /api/setup/status` — coarse-grained progress for the wizard UI
//! - `POST /api/setup/credentials` — save Google credentials (after a
//!   minimal validation step)
//! - `POST /api/setup/test-credentials` — same validation, no persistence
//! - `POST /api/setup/complete` — flip the `setup_complete` flag once
//!   the prerequisites are met

use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::error::{AppError, AppResult};
use crate::services::setup;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub complete: bool,
    pub has_credentials: bool,
    pub has_first_parent: bool,
}

#[derive(Debug, Deserialize)]
pub struct CredentialsBody {
    pub google_client_id: String,
    pub google_client_secret: String,
    pub youtube_api_key: String,
    pub redirect_uri: String,
}

/// Google's OpenID Connect discovery document. Used to sanity-check the
/// wizard input — if Google is reachable, this is a 200 OK with JSON; if
/// not, the wizard surfaces the network error to the user.
const DISCOVERY_URL: &str = "https://accounts.google.com/.well-known/openid-configuration";

/// `GET /api/setup/status`.
pub async fn status(State(state): State<AppState>) -> AppResult<Json<SetupStatus>> {
    Ok(Json(SetupStatus {
        complete: setup::is_setup_complete(&state.db).await?,
        has_credentials: setup::has_google_credentials(&state.db).await?,
        has_first_parent: setup::has_first_parent(&state.db).await?,
    }))
}

/// `POST /api/setup/credentials` — validate + persist.
pub async fn save_credentials(
    State(state): State<AppState>,
    Json(body): Json<CredentialsBody>,
) -> AppResult<StatusCode> {
    validate_credentials(&body).await?;
    setup::set_config_value(&state.db, setup::KEY_GOOGLE_CLIENT_ID, &body.google_client_id)
        .await?;
    setup::set_config_value(
        &state.db,
        setup::KEY_GOOGLE_CLIENT_SECRET,
        &body.google_client_secret,
    )
    .await?;
    setup::set_config_value(&state.db, setup::KEY_YOUTUBE_API_KEY, &body.youtube_api_key)
        .await?;
    setup::set_config_value(&state.db, setup::KEY_GOOGLE_REDIRECT_URI, &body.redirect_uri)
        .await?;
    info!("Google credentials saved via setup wizard");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/setup/test-credentials` — validate without persisting.
pub async fn test_credentials(
    Json(body): Json<CredentialsBody>,
) -> AppResult<Json<serde_json::Value>> {
    validate_credentials(&body).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `POST /api/setup/complete`.
pub async fn complete(State(state): State<AppState>) -> AppResult<StatusCode> {
    if !setup::has_google_credentials(&state.db).await? {
        return Err(AppError::BadRequest(
            "credentials not yet configured".into(),
        ));
    }
    if !setup::has_first_parent(&state.db).await? {
        return Err(AppError::BadRequest(
            "at least one parent account is required".into(),
        ));
    }
    setup::set_config_value(&state.db, setup::KEY_SETUP_COMPLETE, "true").await?;
    info!("Setup wizard completed");
    Ok(StatusCode::NO_CONTENT)
}

/// Sanity-check the credentials. We deliberately keep this lightweight —
/// Google does not let us validate the client ID/secret without going
/// through a real OAuth flow, so we settle for shape checks plus a
/// reachability probe of the discovery document.
async fn validate_credentials(body: &CredentialsBody) -> AppResult<()> {
    if body.google_client_id.trim().is_empty() {
        return Err(AppError::BadRequest("client_id is required".into()));
    }
    if body.google_client_secret.trim().is_empty() {
        return Err(AppError::BadRequest("client_secret is required".into()));
    }
    if body.youtube_api_key.trim().is_empty() {
        return Err(AppError::BadRequest("api_key is required".into()));
    }
    if !body.redirect_uri.starts_with("http://") && !body.redirect_uri.starts_with("https://") {
        return Err(AppError::BadRequest(
            "redirect_uri must start with http:// or https://".into(),
        ));
    }

    let res = reqwest::Client::new()
        .get(DISCOVERY_URL)
        .send()
        .await
        .map_err(|e| {
            warn!(error = %e, "Google discovery doc unreachable");
            AppError::BadRequest(format!("could not reach Google: {e}"))
        })?;
    if !res.status().is_success() {
        return Err(AppError::BadRequest(format!(
            "Google discovery doc returned {}",
            res.status()
        )));
    }
    Ok(())
}
