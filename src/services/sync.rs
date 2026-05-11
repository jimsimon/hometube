//! YouTube outbound-sync helpers.
//!
//! When a child performs an action in HomeTube (subscribe, like, create
//! playlist, add a video to a playlist, …) the change is written
//! immediately to SQLite with a `sync_status` of `pending_*`, and a
//! background task calls into one of the helpers in this module to
//! reconcile the change with YouTube. On success the row's
//! `sync_status` is updated to `synced`; on failure (after a few
//! retries) it is moved to `error` so the UI can surface a retry
//! affordance.
//!
//! All helpers:
//!
//! 1. Refresh the account's access token via
//!    [`crate::services::oauth::refresh_if_expired`] before making a
//!    YouTube call.
//! 2. Use exponential backoff (1s, 4s, 16s) up to 3 attempts.
//! 3. Are total — they update the row to `error` on terminal failure
//!    and never panic.

use std::time::Duration;

use reqwest::{Client, Method};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::services::oauth::refresh_if_expired;

/// YouTube Data API base URL.
const API_BASE: &str = "https://www.googleapis.com/youtube/v3";

/// Backoff delays between retries (in milliseconds). The first attempt
/// happens immediately; subsequent attempts wait these durations. With
/// two entries here we get a maximum of three attempts total.
const BACKOFF_MS: &[u64] = &[1_000, 4_000];

/// Action passed to [`push_playlist_item_change`].
#[derive(Debug, Clone, Copy)]
pub enum PlaylistItemAction {
    /// Insert a single `videoId` into the playlist at the next position.
    Add,
    /// Remove a single `videoId` from the playlist.
    Remove,
    /// Update the position of every video in the local playlist on
    /// YouTube to match the local row order.
    Reorder,
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Reconcile a single `child_subscriptions` row with YouTube.
///
/// Looks at the row's current `sync_status` + `is_deleted` flag:
///
/// - `pending_push` → call `subscriptions.insert`, store the returned
///   resource ID, mark `synced`.
/// - `pending_delete` → call `subscriptions.delete` using the stored
///   `youtube_subscription_id`, mark `synced` (and leave the row in
///   place with `is_deleted=1` for history).
/// - anything else → no-op.
pub async fn push_subscription_change(
    pool: &SqlitePool,
    account_id: i64,
    channel_id: &str,
) -> AppResult<()> {
    let row: Option<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT sync_status, is_deleted, youtube_subscription_id \
         FROM child_subscriptions \
         WHERE child_account_id = ? AND channel_id = ?",
    )
    .bind(account_id)
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    let Some((status, is_deleted, sub_id)) = row else {
        debug!(account_id, %channel_id, "no subscription row to push");
        return Ok(());
    };

    let outcome = if status == "pending_push" && is_deleted == 0 {
        retry_push(|| async { subscription_insert(pool, account_id, channel_id).await }).await
    } else if status == "pending_delete" {
        let Some(sub_id) = sub_id.clone() else {
            // Nothing to delete on YouTube — collapse to local-only.
            mark_subscription_synced(pool, account_id, channel_id, None).await?;
            return Ok(());
        };
        retry_push(|| async { subscription_delete(pool, account_id, &sub_id).await })
            .await
            .map(|_| String::new())
    } else {
        return Ok(());
    };

    match outcome {
        Ok(yt_id) if status == "pending_push" => {
            mark_subscription_synced(pool, account_id, channel_id, Some(&yt_id)).await?;
        }
        Ok(_) => {
            mark_subscription_synced(pool, account_id, channel_id, sub_id.as_deref()).await?;
        }
        Err(err) => {
            warn!(account_id, %channel_id, %err, "subscription sync failed");
            mark_sync_error(pool, "child_subscriptions", "channel_id", channel_id, account_id)
                .await?;
        }
    }
    Ok(())
}

async fn subscription_insert(
    pool: &SqlitePool,
    account_id: i64,
    channel_id: &str,
) -> AppResult<String> {
    let token = refresh_if_expired(pool, account_id).await?;
    let body = json!({
        "snippet": {
            "resourceId": {
                "kind": "youtube#channel",
                "channelId": channel_id,
            },
        },
    });
    let res: Value = youtube_call(
        Method::POST,
        "/subscriptions",
        &[("part", "snippet")],
        &token,
        Some(&body),
    )
    .await?;
    let id = res
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("YouTube did not return subscription id")))?
        .to_string();
    Ok(id)
}

async fn subscription_delete(
    pool: &SqlitePool,
    account_id: i64,
    youtube_subscription_id: &str,
) -> AppResult<()> {
    let token = refresh_if_expired(pool, account_id).await?;
    let _: Value = youtube_call(
        Method::DELETE,
        "/subscriptions",
        &[("id", youtube_subscription_id)],
        &token,
        None,
    )
    .await?;
    Ok(())
}

async fn mark_subscription_synced(
    pool: &SqlitePool,
    account_id: i64,
    channel_id: &str,
    youtube_subscription_id: Option<&str>,
) -> AppResult<()> {
    if let Some(id) = youtube_subscription_id {
        sqlx::query(
            "UPDATE child_subscriptions \
             SET sync_status = 'synced', youtube_subscription_id = ?, updated_at = unixepoch() \
             WHERE child_account_id = ? AND channel_id = ?",
        )
        .bind(id)
        .bind(account_id)
        .bind(channel_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE child_subscriptions \
             SET sync_status = 'synced', updated_at = unixepoch() \
             WHERE child_account_id = ? AND channel_id = ?",
        )
        .bind(account_id)
        .bind(channel_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------------

/// Reconcile a single `child_playlists` row with YouTube based on its
/// current `sync_status`.
pub async fn push_playlist_change(
    pool: &SqlitePool,
    account_id: i64,
    playlist_id: i64,
) -> AppResult<()> {
    let row: Option<(String, i64, Option<String>, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT sync_status, is_deleted, youtube_playlist_id, title, description, is_own \
         FROM child_playlists WHERE id = ? AND child_account_id = ?",
    )
    .bind(playlist_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    let Some((status, is_deleted, yt_id, title, description, is_own)) = row else {
        return Ok(());
    };
    if is_own == 0 {
        // Library imports of YouTube-owned playlists are read-only.
        return Ok(());
    }

    let outcome = match status.as_str() {
        "pending_create" if is_deleted == 0 => {
            retry_push(|| async {
                playlist_create(pool, account_id, &title, description.as_deref()).await
            })
            .await
        }
        "pending_update" if is_deleted == 0 => {
            let Some(yt_id) = yt_id.clone() else {
                return Ok(());
            };
            retry_push(|| async {
                playlist_update(pool, account_id, &yt_id, &title, description.as_deref()).await
            })
            .await
            .map(|_| yt_id.clone())
        }
        "pending_delete" => {
            let Some(yt_id) = yt_id.clone() else {
                mark_playlist_synced(pool, playlist_id, None).await?;
                return Ok(());
            };
            retry_push(|| async { playlist_delete(pool, account_id, &yt_id).await })
                .await
                .map(|_| yt_id.clone())
        }
        _ => return Ok(()),
    };

    match outcome {
        Ok(new_yt_id) if status == "pending_create" => {
            mark_playlist_synced(pool, playlist_id, Some(&new_yt_id)).await?;
        }
        Ok(_) => {
            mark_playlist_synced(pool, playlist_id, yt_id.as_deref()).await?;
        }
        Err(err) => {
            warn!(account_id, playlist_id, %err, "playlist sync failed");
            sqlx::query(
                "UPDATE child_playlists SET sync_status = 'error', updated_at = unixepoch() \
                 WHERE id = ?",
            )
            .bind(playlist_id)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

async fn playlist_create(
    pool: &SqlitePool,
    account_id: i64,
    title: &str,
    description: Option<&str>,
) -> AppResult<String> {
    let token = refresh_if_expired(pool, account_id).await?;
    let body = json!({
        "snippet": {
            "title": title,
            "description": description.unwrap_or(""),
        },
        "status": { "privacyStatus": "private" },
    });
    let res: Value = youtube_call(
        Method::POST,
        "/playlists",
        &[("part", "snippet,status")],
        &token,
        Some(&body),
    )
    .await?;
    res.get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| AppError::Other(anyhow::anyhow!("YouTube did not return playlist id")))
}

async fn playlist_update(
    pool: &SqlitePool,
    account_id: i64,
    youtube_playlist_id: &str,
    title: &str,
    description: Option<&str>,
) -> AppResult<()> {
    let token = refresh_if_expired(pool, account_id).await?;
    let body = json!({
        "id": youtube_playlist_id,
        "snippet": {
            "title": title,
            "description": description.unwrap_or(""),
        },
    });
    let _: Value = youtube_call(
        Method::PUT,
        "/playlists",
        &[("part", "snippet")],
        &token,
        Some(&body),
    )
    .await?;
    Ok(())
}

async fn playlist_delete(
    pool: &SqlitePool,
    account_id: i64,
    youtube_playlist_id: &str,
) -> AppResult<()> {
    let token = refresh_if_expired(pool, account_id).await?;
    let _: Value = youtube_call(
        Method::DELETE,
        "/playlists",
        &[("id", youtube_playlist_id)],
        &token,
        None,
    )
    .await?;
    Ok(())
}

async fn mark_playlist_synced(
    pool: &SqlitePool,
    playlist_id: i64,
    youtube_playlist_id: Option<&str>,
) -> AppResult<()> {
    if let Some(id) = youtube_playlist_id {
        sqlx::query(
            "UPDATE child_playlists \
             SET sync_status = 'synced', youtube_playlist_id = ?, updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(id)
        .bind(playlist_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE child_playlists SET sync_status = 'synced', updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(playlist_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Playlist items
// ---------------------------------------------------------------------------

/// Add / remove / reorder a single video inside a child playlist.
///
/// `Add`/`Remove` operate on the row matching `(playlist_id, video_id)`.
/// `Reorder` walks the entire playlist and pushes a `playlistItems.update`
/// for each item — the row's local `position` value is treated as the
/// authoritative source.
///
/// **Caveat:** YouTube's `playlistItems.update` rejects arbitrary
/// `position` values for "system" playlists like `LL` (likes) and `WL`
/// (watch-later). Only user-created playlists support reorder.
pub async fn push_playlist_item_change(
    pool: &SqlitePool,
    account_id: i64,
    playlist_id: i64,
    video_id: &str,
    action: PlaylistItemAction,
) -> AppResult<()> {
    // Look up the YouTube ID + ownership flag.
    let pl_row: Option<(Option<String>, i64)> = sqlx::query_as(
        "SELECT youtube_playlist_id, is_own FROM child_playlists \
         WHERE id = ? AND child_account_id = ?",
    )
    .bind(playlist_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    let Some((yt_id, is_own)) = pl_row else {
        return Ok(());
    };
    let Some(yt_playlist) = yt_id else {
        // Playlist hasn't been created on YouTube yet; nothing to push.
        return Ok(());
    };
    if is_own == 0 {
        // Read-only library entry.
        return Ok(());
    }

    let res = match action {
        PlaylistItemAction::Add => {
            retry_push(|| async {
                playlist_item_insert(pool, account_id, &yt_playlist, video_id).await
            })
            .await
        }
        PlaylistItemAction::Remove => {
            retry_push(|| async {
                playlist_item_delete(pool, account_id, &yt_playlist, video_id).await
            })
            .await
            .map(|_| String::new())
        }
        PlaylistItemAction::Reorder => {
            retry_push(|| async { playlist_items_reorder(pool, account_id, playlist_id).await })
                .await
                .map(|_| String::new())
        }
    };

    if let Err(err) = res {
        warn!(account_id, playlist_id, %video_id, %err, "playlist-item sync failed");
        sqlx::query(
            "UPDATE child_playlists SET sync_status = 'error', updated_at = unixepoch() \
             WHERE id = ?",
        )
        .bind(playlist_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn playlist_item_insert(
    pool: &SqlitePool,
    account_id: i64,
    youtube_playlist_id: &str,
    video_id: &str,
) -> AppResult<String> {
    let token = refresh_if_expired(pool, account_id).await?;
    let body = json!({
        "snippet": {
            "playlistId": youtube_playlist_id,
            "resourceId": {
                "kind": "youtube#video",
                "videoId": video_id,
            },
        },
    });
    let res: Value = youtube_call(
        Method::POST,
        "/playlistItems",
        &[("part", "snippet")],
        &token,
        Some(&body),
    )
    .await?;
    Ok(res
        .get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default())
}

async fn playlist_item_delete(
    pool: &SqlitePool,
    account_id: i64,
    youtube_playlist_id: &str,
    video_id: &str,
) -> AppResult<()> {
    let token = refresh_if_expired(pool, account_id).await?;
    // Look up the playlist-item ID by listing the playlist and finding
    // a match. `playlistItems.delete` requires the resource ID, not the
    // video ID.
    let list: Value = youtube_call(
        Method::GET,
        "/playlistItems",
        &[
            ("part", "snippet,contentDetails"),
            ("playlistId", youtube_playlist_id),
            ("maxResults", "50"),
        ],
        &token,
        None,
    )
    .await?;
    let item_id = list
        .get("items")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .find(|item| {
            item.pointer("/contentDetails/videoId")
                .and_then(|v| v.as_str())
                == Some(video_id)
                || item.pointer("/snippet/resourceId/videoId").and_then(|v| v.as_str())
                    == Some(video_id)
        })
        .and_then(|item| item.get("id").and_then(|v| v.as_str()).map(String::from));
    let Some(item_id) = item_id else {
        // Already absent on YouTube — treat as success.
        return Ok(());
    };

    let _: Value = youtube_call(
        Method::DELETE,
        "/playlistItems",
        &[("id", item_id.as_str())],
        &token,
        None,
    )
    .await?;
    Ok(())
}

async fn playlist_items_reorder(
    pool: &SqlitePool,
    account_id: i64,
    playlist_id: i64,
) -> AppResult<()> {
    let token = refresh_if_expired(pool, account_id).await?;
    let yt_id: Option<String> = sqlx::query_scalar(
        "SELECT youtube_playlist_id FROM child_playlists WHERE id = ?",
    )
    .bind(playlist_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let Some(yt_playlist) = yt_id else {
        return Ok(());
    };

    // Pull the current local ordering.
    let local: Vec<(String, i64)> = sqlx::query_as(
        "SELECT video_id, position FROM child_playlist_videos \
         WHERE playlist_id = ? ORDER BY position",
    )
    .bind(playlist_id)
    .fetch_all(pool)
    .await?;

    // Pull the current YouTube ordering — we need the playlist-item IDs
    // to call `update`.
    let list: Value = youtube_call(
        Method::GET,
        "/playlistItems",
        &[
            ("part", "snippet,contentDetails"),
            ("playlistId", yt_playlist.as_str()),
            ("maxResults", "50"),
        ],
        &token,
        None,
    )
    .await?;
    let items = list
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // For each local row, find the matching YouTube playlist-item and
    // PUT it with the new position.
    for (video_id, position) in local {
        let Some(yt_item) = items.iter().find(|item| {
            item.pointer("/contentDetails/videoId")
                .and_then(|v| v.as_str())
                == Some(video_id.as_str())
        }) else {
            continue;
        };
        let item_id = yt_item.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if item_id.is_empty() {
            continue;
        }
        let body = json!({
            "id": item_id,
            "snippet": {
                "playlistId": yt_playlist,
                "resourceId": {
                    "kind": "youtube#video",
                    "videoId": video_id,
                },
                "position": position,
            },
        });
        let _: Value = youtube_call(
            Method::PUT,
            "/playlistItems",
            &[("part", "snippet")],
            &token,
            Some(&body),
        )
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Likes
// ---------------------------------------------------------------------------

/// Sync a single like row with YouTube via `videos.rate`.
///
/// `pending_push` → `rating=like`. `pending_delete` → `rating=none`.
pub async fn push_like_change(
    pool: &SqlitePool,
    account_id: i64,
    video_id: &str,
) -> AppResult<()> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT sync_status, is_deleted FROM video_likes \
         WHERE child_account_id = ? AND video_id = ?",
    )
    .bind(account_id)
    .bind(video_id)
    .fetch_optional(pool)
    .await?;
    let Some((status, _)) = row else {
        return Ok(());
    };

    let rating = match status.as_str() {
        "pending_push" => "like",
        "pending_delete" => "none",
        _ => return Ok(()),
    };

    let res = retry_push(|| async {
        let token = refresh_if_expired(pool, account_id).await?;
        let _: Value = youtube_call(
            Method::POST,
            "/videos/rate",
            &[("id", video_id), ("rating", rating)],
            &token,
            None,
        )
        .await?;
        Ok(())
    })
    .await;

    match res {
        Ok(()) => {
            sqlx::query(
                "UPDATE video_likes SET sync_status = 'synced', updated_at = unixepoch() \
                 WHERE child_account_id = ? AND video_id = ?",
            )
            .bind(account_id)
            .bind(video_id)
            .execute(pool)
            .await?;
        }
        Err(err) => {
            warn!(account_id, %video_id, %err, "like sync failed");
            mark_sync_error(pool, "video_likes", "video_id", video_id, account_id).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Issue a single YouTube Data API call as the OAuthed user.
///
/// `query` may include hard-coded keys like `part=snippet`. Bodies are
/// serialised as JSON. The response is parsed as JSON; an empty
/// `Value::Null` is returned for 204-style responses.
async fn youtube_call(
    method: Method,
    path: &str,
    query: &[(&str, &str)],
    access_token: &str,
    body: Option<&Value>,
) -> AppResult<Value> {
    let client = Client::new();
    let url = format!("{API_BASE}{path}");
    let mut req = client.request(method.clone(), &url).bearer_auth(access_token);
    for (k, v) in query {
        req = req.query(&[(k, v)]);
    }
    if let Some(body) = body {
        req = req.json(body);
    }
    let res = req.send().await.map_err(AppError::Http)?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(AppError::Other(anyhow::anyhow!(
            "YouTube API {status}: {body}"
        )));
    }
    if status.as_u16() == 204 {
        return Ok(Value::Null);
    }
    let text = res.text().await.unwrap_or_default();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| AppError::Other(anyhow::anyhow!("YouTube response not JSON: {e}")))
}

/// Retry `op` up to `BACKOFF_MS.len() + 1` times with exponential
/// backoff. Returns the first success or the last error.
async fn retry_push<F, Fut, T>(mut op: F) -> AppResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AppResult<T>>,
{
    let mut last_err: Option<AppError> = None;
    for (attempt, _) in std::iter::once(0u64).chain(BACKOFF_MS.iter().copied()).enumerate() {
        if attempt > 0 {
            let delay = BACKOFF_MS[attempt - 1];
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                debug!(attempt, %err, "push attempt failed");
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AppError::Other(anyhow::anyhow!("retry_push exhausted"))))
}

async fn mark_sync_error(
    pool: &SqlitePool,
    table: &str,
    key: &str,
    key_value: &str,
    account_id: i64,
) -> AppResult<()> {
    let sql = format!(
        "UPDATE {table} SET sync_status = 'error', updated_at = unixepoch() \
         WHERE child_account_id = ? AND {key} = ?"
    );
    sqlx::query(&sql)
        .bind(account_id)
        .bind(key_value)
        .execute(pool)
        .await?;
    Ok(())
}
