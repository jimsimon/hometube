//! HTML page handlers.
//!
//! Each handler renders an askama template and returns it as `text/html`.
//! Templates live under `templates/` and are compiled into the binary.

use askama::Template;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};

use crate::error::AppResult;
use crate::services::setup;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/home.html")]
struct HomeTemplate {
    title: &'static str,
}

/// Placeholder home page — replaced in later phases by the profile picker /
/// real home pages once auth lands.
pub async fn home() -> AppResult<impl IntoResponse> {
    let tpl = HomeTemplate {
        title: "HomeTube",
    };
    Ok(Html(tpl.render()?))
}

#[derive(Template)]
#[template(path = "pages/setup-wizard.html")]
struct SetupWizardTemplate {
    /// Best-guess redirect URI (auto-detected from request `Host` header)
    /// used as the default value in the credentials step.
    suggested_redirect_uri: String,
}

/// `GET /setup` — server-rendered shell for the multi-step setup wizard.
///
/// The actual step UI is implemented by Lit web components included on
/// the page; this handler only computes a sensible default redirect URI
/// (`http://<host>/api/auth/callback`) so the wizard form can be
/// pre-filled.
pub async fn setup_wizard(
    State(_state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3000");
    let scheme = if host.starts_with("localhost") || host.starts_with("127.") {
        "http"
    } else {
        "https"
    };
    let suggested_redirect_uri = format!("{scheme}://{host}/api/auth/callback");

    let tpl = SetupWizardTemplate {
        suggested_redirect_uri,
    };
    Ok(Html(tpl.render()?))
}

/// `GET /` — until setup is complete, just bounce the user to the wizard
/// to give a sensible landing experience even if they bypass the
/// redirect middleware (e.g., during tests).
pub async fn root_or_setup(
    State(state): State<AppState>,
) -> AppResult<axum::response::Response> {
    if setup::is_setup_complete(&state.db).await? {
        Ok(home().await?.into_response())
    } else {
        Ok(axum::response::Redirect::to("/setup").into_response())
    }
}
