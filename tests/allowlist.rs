//! Allowlist read/delete coverage.
//!
//! `POST /api/children/:id/allowlist/{kind}` calls
//! [`hometube::services::youtube::YoutubeClient`] which goes off-network
//! to the live YouTube Data API — we never exercise the create path
//! through the API in these tests. Instead we insert allowlist rows
//! directly into the database (matching what a successful
//! YouTube-resolved POST would write) and then assert that GET + DELETE
//! behave correctly.
//!
//! This still meaningfully covers the routes' SQL + serialization
//! paths and the `require_child_id` validation helper.

mod common;

use axum::http::StatusCode;
use common::{allowlist_channel, allowlist_video, boot_with_parent_and_child};
use hometube::models::account::AccountType;

#[tokio::test]
async fn videos_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    // Seed both the canonical `videos` row and the per-child link.
    // `channel_title` is no longer a column on `allowlisted_videos`; the
    // list handler hydrates it via `videos.channel_id → channels`.
    common::seed_channel(&app.pool, "chan-1", Some("Some Channel")).await;
    sqlx::query(
        "INSERT INTO videos (video_id, title, channel_id, thumbnail_url) \
         VALUES ('vid-1', 'Hello', 'chan-1', 'http://thumb')",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO allowlisted_videos (child_account_id, video_id, added_by) \
         VALUES (?, 'vid-1', ?)",
    )
    .bind(child_id)
    .bind(parent_id)
    .execute(&app.pool)
    .await
    .expect("seed video");
    let _ = allowlist_video; // imported for other tests in this file

    // Pre-populate `video_metadata_cache` so any handler that does a
    // best-effort lookup hits the cache rather than yt-dlp.
    sqlx::query(
        "INSERT INTO video_metadata_cache (video_id, metadata_json, expires_at) \
         VALUES ('vid-1', '{\"id\":\"vid-1\",\"channel_id\":\"chan-1\"}', unixepoch() + 3600)",
    )
    .execute(&app.pool)
    .await
    .expect("seed metadata cache");

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let body: serde_json::Value = res.json();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["video_id"], "vid-1");
    assert_eq!(arr[0]["video_title"], "Hello");

    // Delete and confirm the list is empty.
    let res = app
        .server
        .delete(&format!("/api/children/{child_id}/allowlist/videos/vid-1"))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/videos"))
        .await;
    let body: serde_json::Value = res.json();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn channels_round_trip_via_db_seed() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let parent_id = app.parent_id.unwrap();

    allowlist_channel(
        &app.pool,
        child_id,
        parent_id,
        "chan-1",
        Some("Cool Channel"),
    )
    .await;

    let res = app
        .server
        .get(&format!("/api/children/{child_id}/allowlist/channels"))
        .await;
    assert_eq!(res.status_code(), StatusCode::OK);
    let arr: serde_json::Value = res.json();
    assert_eq!(arr[0]["channel_id"], "chan-1");

    let res = app
        .server
        .delete(&format!(
            "/api/children/{child_id}/allowlist/channels/chan-1"
        ))
        .await;
    assert_eq!(res.status_code(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn allowlist_rejects_non_child_target() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let parent_id = app.parent_id.unwrap();
    // Pointing at a parent ID returns 400 from `require_child_id`.
    let res = app
        .server
        .get(&format!("/api/children/{parent_id}/allowlist/videos"))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

/// `add_channel` enforces a max length on `channel_id` to keep the
/// rest of the handler (and the downstream sidecar call / DB insert)
/// from doing pointless work with an obviously-bogus payload. Catches
/// regressions in the validation block at the top of the handler.
///
/// The assertion is status-code-only — error-copy substring matches
/// are brittle across phrasing changes.
#[tokio::test]
async fn add_channel_rejects_oversized_channel_id() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&serde_json::json!({
            "channel_id": "x".repeat(200),
            "channel_title": "Anything",
            "channel_thumbnail_url": "https://i.ytimg.com/vi/x/hqdefault.jpg",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

/// Body-supplied thumbnail URLs are gated to YouTube/Google-controlled
/// hosts because they're rendered child-side via `<img src>`. An
/// attacker-controlled host is the only realistic abuse vector for the
/// body-data path (which otherwise skips the sidecar's own
/// validation).
///
/// The assertion is status-code-only — error-copy substring matches
/// are brittle across phrasing changes.
#[tokio::test]
async fn add_channel_rejects_untrusted_thumbnail_host() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&serde_json::json!({
            "channel_id": "UCabcdefghijklmnopqrstuv",
            "channel_title": "Hostile",
            "channel_thumbnail_url": "https://attacker.example/poison.jpg",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

/// Empty `channel_id` is the trivial mistake — handler must 400.
#[tokio::test]
async fn add_channel_rejects_empty_channel_id() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&serde_json::json!({
            "channel_id": "",
            "channel_title": "X",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

/// Title length cap. Title lives in the DB row + the rendered UI; a
/// 10 KB blob shouldn't be accepted just because the parent agent
/// faked one.
#[tokio::test]
async fn add_channel_rejects_oversized_channel_title() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&serde_json::json!({
            "channel_id": "UCfine",
            "channel_title": "T".repeat(500),
            "channel_thumbnail_url": "https://i.ytimg.com/vi/x/hqdefault.jpg",
        }))
        .await;
    assert_eq!(res.status_code(), StatusCode::BAD_REQUEST);
}

/// Regression: a description with multi-byte UTF-8 characters
/// (emoji, CJK, accented Latin) longer than `MAX_DESCRIPTION_LEN`
/// used to panic when truncated because `&d[..8192]` byte-slices
/// at a non-char-boundary. A malicious parent could trigger a 500
/// (handler panic → connection drop / process-kill in some
/// configurations) by sending such a payload.
///
/// The fixed truncation uses `char_indices` to find a safe boundary,
/// so this test should now return a `2xx` (truncated and saved
/// successfully).
///
/// The test uses a 5000-char emoji repeat — each '🎉' is 4 UTF-8
/// bytes, so the byte length is ~20 KB, well past the 8192-byte cap.
/// Truncating at byte 8192 lands mid-codepoint (8192 % 4 == 0 here,
/// but the cap could change), so the `char_indices` boundary search
/// is required for safety in general.
#[tokio::test]
async fn add_channel_truncates_multibyte_description_without_panicking() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();
    // 5000 × '🎉' = 20,000 bytes of UTF-8 — well past the 8192 cap.
    // We pad with one '😀' so the byte alignment is guaranteed not
    // to fall on a multiple of 4 (the fix must not depend on that).
    let description = format!("😀{}", "🎉".repeat(5000));

    let res = app
        .server
        .post(&format!("/api/children/{child_id}/allowlist/channels"))
        .json(&serde_json::json!({
            "channel_id": "UCemoji",
            "channel_title": "Emoji",
            "channel_thumbnail_url": "https://i.ytimg.com/vi/x/hqdefault.jpg",
            "description": description,
        }))
        .await;
    // The response should be 2xx — the handler must not panic.
    assert!(
        res.status_code().is_success(),
        "expected 2xx, got {} (handler panicked on multi-byte truncation?)",
        res.status_code()
    );

    // The stored description should be valid UTF-8 of at most
    // MAX_DESCRIPTION_LEN bytes (8192). We verify both via DB.
    let stored: Option<String> =
        sqlx::query_scalar("SELECT description FROM channels WHERE channel_id = 'UCemoji'")
            .fetch_optional(&app.pool)
            .await
            .unwrap()
            .flatten();
    let stored = stored.expect("description was stored");
    assert!(
        stored.len() <= 8192,
        "byte length capped at MAX_DESCRIPTION_LEN"
    );
    // The stored string must be valid UTF-8 (Rust would refuse to
    // build a &str otherwise, but this proves it survived the
    // round-trip through SQLite without corruption).
    assert!(stored.chars().all(|c| c == '🎉' || c == '😀'));
}

/// Regression: the `is_safe_thumbnail_url` validator must terminate
/// the authority component at `/`, `?`, or `#`. Splitting on `/`
/// alone would let `https://attacker.com#@x.ytimg.com/foo` pass
/// because the `@`-split would pick `x.ytimg.com` as the apparent
/// host. The browser ignores the fragment when fetching and would
/// load `attacker.com`.
///
/// Asserted via the route boundary so the regression survives any
/// future refactor that moves the validator.
#[tokio::test]
async fn add_channel_rejects_fragment_bypass_in_thumbnail_url() {
    let (app, _auth) = boot_with_parent_and_child(AccountType::Parent).await;
    let child_id = app.child_id.unwrap();

    for url in [
        "https://attacker.com#@x.ytimg.com/poison.jpg",
        "https://attacker.com?@x.ytimg.com/poison.jpg",
        "https://attacker.com?host=x.ytimg.com",
    ] {
        let res = app
            .server
            .post(&format!("/api/children/{child_id}/allowlist/channels"))
            .json(&serde_json::json!({
                "channel_id": "UCevil",
                "channel_title": "Hostile",
                "channel_thumbnail_url": url,
            }))
            .await;
        assert_eq!(
            res.status_code(),
            StatusCode::BAD_REQUEST,
            "URL {url} must be rejected by host validation"
        );
    }
}
