//! HTML page handlers.
//!
//! Each handler renders an askama template and returns it as `text/html`.
//! Templates live under `templates/` and are compiled into the binary.

use askama::Template;
use axum::response::{Html, IntoResponse};

use crate::error::AppResult;

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
