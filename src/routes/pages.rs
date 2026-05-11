//! HTML page handlers.
//!
//! Each handler renders an askama template and returns it as `text/html`.
//! Templates live under `templates/` and are compiled into the binary.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use crate::error::AppResult;
use crate::middleware::auth::CurrentAccount;
use crate::models::account::AccountType;
use crate::services::access::can_child_view;
use crate::services::setup;
use crate::services::video_cache::VideoCache;
use crate::state::AppState;

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
/// signed-in role. Anonymous users are sent to `/profiles` so they can
/// pick a profile (Phase 15).
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
        _ => Ok(Redirect::to("/profiles").into_response()),
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
#[template(path = "pages/parent/family.html")]
struct ParentFamilyTemplate {
    display_name: String,
}

/// `GET /parent/family` — family-management page (Phase 13).
pub async fn parent_family(current: Option<CurrentAccount>) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentFamilyTemplate {
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
#[template(path = "pages/profile-picker.html")]
struct ProfilePickerTemplate {}

/// `GET /profiles` — Netflix-style profile picker (Phase 15).
///
/// Available with or without an active session. If setup hasn't been
/// completed yet (no parent accounts at all) we redirect to the wizard
/// instead of showing an empty grid.
pub async fn profile_picker(State(state): State<AppState>) -> AppResult<Response> {
    if !setup::is_setup_complete(&state.db).await? {
        return Ok(Redirect::to("/setup").into_response());
    }
    let tpl = ProfilePickerTemplate {};
    Ok(Html(tpl.render()?).into_response())
}

#[derive(Template)]
#[template(path = "pages/set-pin.html")]
struct SetPinTemplate {
    display_name: String,
    /// `true` for the "freshly added parent must set a PIN" path.
    for_new_parent: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetPinQuery {
    /// Set to `1` when the page was reached via the family flow.
    /// Surfaces a slightly different message + redirects to
    /// `/parent/family` on success.
    #[serde(default)]
    pub for_new_parent: Option<String>,
}

/// `GET /setup/pin` — single-form page that POSTs to `/api/auth/pin`.
/// Used both during the setup wizard and by the family-management flow
/// to force newly-added parents to set a PIN before continuing.
pub async fn set_pin(
    current: Option<CurrentAccount>,
    Query(q): Query<SetPinQuery>,
) -> AppResult<Response> {
    let Some(c) = current else {
        return Ok(Redirect::to("/profiles").into_response());
    };
    if !matches!(c.account_type, AccountType::Parent) {
        return Ok(Redirect::to("/child/home").into_response());
    }
    let tpl = SetPinTemplate {
        display_name: c.display_name,
        for_new_parent: matches!(q.for_new_parent.as_deref(), Some("1") | Some("true")),
    };
    Ok(Html(tpl.render()?).into_response())
}

#[derive(Template)]
#[template(path = "pages/parent/system.html")]
struct ParentSystemTemplate {
    display_name: String,
}

/// `GET /parent/system` — cron jobs + yt-dlp + cache management.
pub async fn parent_system(current: Option<CurrentAccount>) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentSystemTemplate {
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
#[template(path = "pages/parent/preview.html")]
struct ParentPreviewTemplate {
    display_name: String,
    /// `"video"`, `"channel"`, or `"playlist"`.
    kind: String,
    resource_id: String,
}

/// `GET /parent/preview/:kind/:id` — parent-side preview shell that
/// hosts the appropriate player/grid component pointed at
/// `/api/preview/...` (which bypasses the allowlist).
pub async fn parent_preview(
    current: Option<CurrentAccount>,
    Path((kind, resource_id)): Path<(String, String)>,
) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            if !matches!(kind.as_str(), "video" | "channel" | "playlist") {
                return Ok(Redirect::to("/parent/home").into_response());
            }
            let tpl = ParentPreviewTemplate {
                display_name: c.display_name,
                kind,
                resource_id,
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
#[template(path = "pages/parent/activity.html")]
struct ParentActivityTemplate {
    display_name: String,
}

/// `GET /parent/activity` — watch-activity dashboard (Phase 17).
pub async fn parent_activity(current: Option<CurrentAccount>) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentActivityTemplate {
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
#[template(path = "pages/parent/playlists.html")]
struct ParentPlaylistsTemplate {
    display_name: String,
}

/// `GET /parent/playlists` — family playlist manager (Phase 18).
pub async fn parent_playlists(current: Option<CurrentAccount>) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentPlaylistsTemplate {
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
#[template(path = "pages/parent/playlist.html")]
struct ParentPlaylistTemplate {
    display_name: String,
    playlist_id: i64,
}

/// `GET /parent/playlist/:id` — single family playlist editor.
pub async fn parent_playlist(
    current: Option<CurrentAccount>,
    Path(playlist_id): Path<i64>,
) -> AppResult<Response> {
    match current {
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            let tpl = ParentPlaylistTemplate {
                display_name: c.display_name,
                playlist_id,
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
#[template(path = "pages/child/bookmarks.html")]
struct ChildBookmarksTemplate {
    display_name: String,
}

/// `GET /child/bookmarks` — list of saved bookmarks across videos.
pub async fn child_bookmarks(current: Option<CurrentAccount>) -> AppResult<Response> {
    require_child(current, |c| {
        let tpl = ChildBookmarksTemplate {
            display_name: c.display_name,
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/video-unavailable.html")]
struct ChildVideoUnavailableTemplate {
    display_name: String,
    video_id: String,
    /// Optional friendly message — set when extraction errored vs the
    /// generic "not allowlisted" path.
    message: Option<String>,
}

/// Render the friendly "this video is unavailable" page. Exposed so
/// other handlers (e.g. on yt-dlp extraction failure) can route to it
/// directly.
pub fn render_video_unavailable(
    display_name: String,
    video_id: String,
    message: Option<String>,
) -> AppResult<Response> {
    let tpl = ChildVideoUnavailableTemplate {
        display_name,
        video_id,
        message,
    };
    Ok(Html(tpl.render()?).into_response())
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

#[derive(Template)]
#[template(path = "pages/child/channels.html")]
struct ChildChannelsTemplate {
    display_name: String,
}

/// `GET /child/channels` — list of subscribed channels for the child.
pub async fn child_channels(current: Option<CurrentAccount>) -> AppResult<Response> {
    require_child(current, |c| {
        let tpl = ChildChannelsTemplate {
            display_name: c.display_name,
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/channel.html")]
struct ChildChannelTemplate {
    display_name: String,
    channel_id: String,
}

/// `GET /child/channel/:channelId` — single channel page.
pub async fn child_channel(
    current: Option<CurrentAccount>,
    Path(channel_id): Path<String>,
) -> AppResult<Response> {
    require_child(current, |c| {
        let tpl = ChildChannelTemplate {
            display_name: c.display_name,
            channel_id: channel_id.clone(),
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/playlists.html")]
struct ChildPlaylistsTemplate {
    display_name: String,
}

/// `GET /child/playlists`.
pub async fn child_playlists(current: Option<CurrentAccount>) -> AppResult<Response> {
    require_child(current, |c| {
        let tpl = ChildPlaylistsTemplate {
            display_name: c.display_name,
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/playlist.html")]
struct ChildPlaylistTemplate {
    display_name: String,
    /// Local primary-key id (string-encoded).
    playlist_id: String,
    /// `"child"` for child-owned playlists, `"family"` for family
    /// playlists. Lit-side decides which API to call.
    playlist_kind: String,
}

/// `GET /child/playlist/:id`.
///
/// `:id` may be a bare integer (child-owned playlist) or `family:<id>`
/// (family playlist). The shape is forwarded verbatim to the Lit
/// component.
pub async fn child_playlist(
    current: Option<CurrentAccount>,
    Path(playlist_id): Path<String>,
) -> AppResult<Response> {
    require_child(current, |c| {
        let (playlist_kind, raw_id) = if let Some(rest) = playlist_id.strip_prefix("family:") {
            ("family".to_string(), rest.to_string())
        } else {
            ("child".to_string(), playlist_id.clone())
        };
        let tpl = ChildPlaylistTemplate {
            display_name: c.display_name,
            playlist_id: raw_id,
            playlist_kind,
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/search.html")]
struct ChildSearchTemplate {
    display_name: String,
    q: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchPageQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// `GET /child/search` — server-rendered shell for child search. The
/// real result rendering happens in `<hometube-search-results>` once
/// the query is sent to `/api/search`.
pub async fn child_search(
    current: Option<CurrentAccount>,
    Query(q): Query<SearchPageQuery>,
) -> AppResult<Response> {
    require_child(current, |c| {
        let tpl = ChildSearchTemplate {
            display_name: c.display_name,
            q: q.q.clone().unwrap_or_default(),
            kind: q.kind.clone().unwrap_or_else(|| "all".to_string()),
        };
        Ok(Html(tpl.render()?).into_response())
    })
    .await
}

#[derive(Template)]
#[template(path = "pages/child/video.html")]
struct ChildVideoTemplate {
    display_name: String,
    video_id: String,
    /// Up-next source context, in `playlist:ID` / `channel:ID` /
    /// `video:ID` shape. May be empty.
    from: String,
    /// `true` when access control denies playback. The template renders
    /// a friendly "unavailable" page rather than the player.
    unavailable: bool,
    /// Initial seek position in seconds. `0` means "start from the
    /// beginning"; the template only emits a `start-at` attribute when
    /// this is non-zero.
    start_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct VideoPageQuery {
    #[serde(default)]
    pub from: Option<String>,
    /// Optional initial seek position in seconds (driven by the
    /// bookmarks list — clicking a bookmark navigates to
    /// `?t=<seconds>`).
    #[serde(default, rename = "t")]
    pub t: Option<i64>,
}

/// `GET /child/video/:videoId`.
///
/// Resolves allowlist + extraction status up front. On extraction
/// failure we render a friendly `video-unavailable.html` rather than
/// a 500, and we record a `ytdlp_failure` notification (deduplicated
/// in-process) so parents see something happened.
pub async fn child_video(
    State(state): State<AppState>,
    current: Option<CurrentAccount>,
    Path(video_id): Path<String>,
    Query(q): Query<VideoPageQuery>,
) -> AppResult<Response> {
    let Some(c) = current else {
        return Ok(Redirect::to("/").into_response());
    };
    if !matches!(c.account_type, AccountType::Child) {
        return Ok(Redirect::to("/parent/home").into_response());
    }

    let cache = VideoCache::new();
    let extract = cache
        .get_or_extract(&state.db, &state.config, &video_id)
        .await;

    match extract {
        Ok(result) => {
            let allowed = can_child_view(
                &state.db,
                c.id,
                &video_id,
                result.channel_id.as_deref(),
                &[],
            )
            .await
            .unwrap_or(false);
            let tpl = ChildVideoTemplate {
                display_name: c.display_name,
                video_id,
                from: q.from.unwrap_or_default(),
                unavailable: !allowed,
                start_at: q.t.unwrap_or(0).max(0),
            };
            Ok(Html(tpl.render()?).into_response())
        }
        Err(err) => {
            // yt-dlp failure or other extraction issue. Notify parents
            // (deduped) and show the friendly page.
            tracing::warn!(%video_id, error = %err, "video extraction failed; rendering unavailable page");
            let _ = crate::services::notifications::dispatch_ytdlp_failure_deduped(
                &state.db,
                &video_id,
                &err.to_string(),
            )
            .await;
            render_video_unavailable(
                c.display_name,
                video_id,
                Some(
                    "We couldn't load this video right now. Please try again later."
                        .to_string(),
                ),
            )
        }
    }
}

async fn require_child<F>(current: Option<CurrentAccount>, render: F) -> AppResult<Response>
where
    F: FnOnce(CurrentAccount) -> AppResult<Response>,
{
    match current {
        Some(c) if matches!(c.account_type, AccountType::Child) => render(c),
        Some(c) if matches!(c.account_type, AccountType::Parent) => {
            Ok(Redirect::to("/parent/home").into_response())
        }
        _ => Ok(Redirect::to("/").into_response()),
    }
}
