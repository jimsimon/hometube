//! Setup-wizard API.
//!
//! - `GET /api/setup/status` — coarse-grained progress for the wizard UI
//! - `POST /api/setup/complete` — flip the `setup_complete` flag once
//!   the prerequisites are met

use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use tracing::info;

use crate::error::{AppError, AppResult};
use crate::services::setup;
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub complete: bool,
    pub has_first_parent: bool,
}

/// `GET /api/setup/status`.
pub async fn status(State(state): State<AppState>) -> AppResult<Json<SetupStatus>> {
    Ok(Json(SetupStatus {
        complete: setup::is_setup_complete(&state.db).await?,
        has_first_parent: setup::has_first_parent(&state.db).await?,
    }))
}

/// `POST /api/setup/complete`.
pub async fn complete(State(state): State<AppState>) -> AppResult<StatusCode> {
    if !setup::has_first_parent(&state.db).await? {
        return Err(AppError::BadRequest(
            "at least one parent account is required".into(),
        ));
    }
    setup::set_config_value(&state.db, setup::KEY_SETUP_COMPLETE, "true").await?;
    info!("Setup wizard completed");
    Ok(StatusCode::NO_CONTENT)
}
