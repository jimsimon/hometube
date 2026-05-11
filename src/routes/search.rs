//! Search helpers.
//!
//! Two distinct concerns share this module name in the plan:
//!
//! - `GET /api/parent/search` — parent-side discovery, used by the
//!   allowlist UI to find content to add. Hits the YouTube Data API
//!   directly. Implemented here.
//! - `GET /api/search` — child-side allowlist-bounded search.
//!   Implemented in Phase 10; not part of this module.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::services::youtube::{SearchItem, SearchType, YoutubeClient};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ParentSearchQuery {
    pub q: String,
    /// `"channel"`, `"playlist"`, or `"video"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub items: Vec<SearchItem>,
}

/// `GET /api/parent/search?q=&type=channel|playlist|video`.
pub async fn parent_search(
    State(state): State<AppState>,
    Query(q): Query<ParentSearchQuery>,
) -> AppResult<Json<SearchResponse>> {
    let kind = SearchType::parse(&q.kind)
        .ok_or_else(|| AppError::BadRequest("type must be channel|playlist|video".into()))?;
    let yt = YoutubeClient::from_db(&state.db).await?;
    let items = yt
        .search(&q.q, kind, q.max_results.unwrap_or(15))
        .await?;
    Ok(Json(SearchResponse { items }))
}
