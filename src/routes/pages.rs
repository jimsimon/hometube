//! HTML page handlers.
//!
//! Each handler renders an askama template and returns it as `text/html`.
//! Templates live under `templates/` and are compiled into the binary.

use askama::Template;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::setup;
use crate::state::AppState;

#[derive(Template)]
#[template(path = "pages/home.html")]
struct HomeTemplate {
    title: &'static str,
}

/// Placeholder home page used when no profile is signed in. Once an
/// account is on the request, [`root_or_setup`] redirects to the
/// role-appropriate home.
pub async fn home() -> AppResult<impl IntoResponse> {
    let tpl = HomeTemplate {
        title: "HomeTube",
    };
    Ok(Html(tpl.render()?))
}

#[derive(Template)]
#[template(path = "pages/setup-wizard.html")]
struct SetupWizardTemplate {
    suggested_redirect_uri: String,
}

/// `GET /setup` — server-rendered shell for the multi-step setup wizard.
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

/// `GET /` — until setup is complete, bounce to the wizard. Otherwise
/// route the user to the parent or child home page based on their
/// signed-in role. Anonymous users see a placeholder until they pick a
/// profile.
pub async fn root_or_setup(
    State(state): State<AppState>,
    current: Option<CurrentAccount>,
) -> AppResult<Response> {
    if !setup::is_setup_complete(&state.db).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            Ok(Redirect::to("/parent/home").into_response())
        }
        Some(c) if matches!(c.account_type, AccountType::Child) => {
            Ok(Redirect::to("/child/home").into_response())
        }
        _ => Ok(home().await?.into_response()),
    }
}

#[derive(Template)]
#[template(path = "pages/parent/home.html")]
struct ParentHomeTemplate {
    display_name: String,
}

/// `GET /parent/home` — the allowlist + child-management dashboard.
/// Children get redirected to their own home; anonymous users go to the
/// placeholder.
pub async fn parent_home(
    current: Option<CurrentAccount>,
) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentHomeTemplate {
                display_name: c.display_name,
            };
            Ok(Html(tpl.render()?).into_response())
        }
        Some(c) if matches!(c.account_type, AccountType::Child) => {
            Ok(Redirect::to("/child/home").into_response())
        }
        _ => Ok(Redirect::to("/").into_response()),
    }
}

#[derive(Template)]
#[template(path = "pages/child/home.html")]
struct ChildHomeTemplate {
    display_name: String,
}

/// `GET /child/home` — kid-friendly browse page with continue-watching +
/// new-videos rows.
pub async fn child_home(
    current: Option<CurrentAccount>,
) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Child) => {
            let tpl = ChildHomeTemplate {
                display_name: c.display_name,
            };
            Ok(Html(tpl.render()?).into_response())
        }
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            Ok(Redirect::to("/parent/home").into_response())
        }
        _ => Ok(Redirect::to("/").into_response()),
    }
}
