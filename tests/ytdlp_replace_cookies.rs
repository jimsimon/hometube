//! `ytdlp::replace_cookies` must leave the `app_config` row and
//! `cookies.txt` in agreement whichever side fails.
//!
//! Own binary: it rewrites the process-wide cookies file and bumps the
//! cookie generation, both of which would disturb sibling tests.

mod common;

use common::boot;
use hometube::services::setup::{get_config_value, set_config_value, KEY_YTDLP_COOKIES};
use hometube::services::ytdlp;

/// The tests share one cookies file and the process-wide generation.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const OLD: &str = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\told\t1\n";
const NEW: &str = "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tFALSE\t0\tnew\t2\n";

/// Seed both sides with `OLD` so each test starts from a consistent jar.
async fn seed_old(pool: &sqlx::SqlitePool) {
    set_config_value(pool, KEY_YTDLP_COOKIES, OLD)
        .await
        .unwrap();
    ytdlp::sync_cookies_to_disk(Some(OLD)).unwrap();
}

/// Permission bits of `path`.
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Happy path: both sides move to the new jar and the generation advances.
#[tokio::test]
async fn replace_updates_file_and_row_together() {
    let _serial = SERIAL.lock().await;
    let app = boot().await;
    seed_old(&app.pool).await;
    let before = ytdlp::cookie_generation();

    ytdlp::replace_cookies(&app.pool, Some(NEW)).await.unwrap();

    assert_eq!(
        get_config_value(&app.pool, KEY_YTDLP_COOKIES)
            .await
            .unwrap(),
        Some(NEW.to_string())
    );
    assert_eq!(
        std::fs::read_to_string(ytdlp::cookies_file_path()).unwrap(),
        NEW
    );
    assert_eq!(mode_of(&ytdlp::cookies_file_path()), 0o600);
    assert_ne!(ytdlp::cookie_generation(), before);

    ytdlp::replace_cookies(&app.pool, None).await.unwrap();
    assert_eq!(
        get_config_value(&app.pool, KEY_YTDLP_COOKIES)
            .await
            .unwrap(),
        None
    );
    assert!(!ytdlp::cookies_file_path().exists());
}

/// If the database write fails after the file was rewritten, the file is
/// restored to the jar the row still holds, and the error is reported.
#[tokio::test]
async fn failed_row_write_restores_the_previous_file() {
    let _serial = SERIAL.lock().await;
    let app = boot().await;
    seed_old(&app.pool).await;
    sqlx::query(
        "CREATE TRIGGER reject_cookie_update BEFORE UPDATE ON app_config \
         WHEN NEW.key = 'ytdlp_cookies' \
         BEGIN SELECT RAISE(ABORT, 'rejected by test trigger'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let err = ytdlp::replace_cookies(&app.pool, Some(NEW))
        .await
        .expect_err("database failure must surface");
    assert!(
        err.to_string().contains("rejected by test trigger"),
        "unexpected error: {err}"
    );

    sqlx::query("DROP TRIGGER reject_cookie_update")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        get_config_value(&app.pool, KEY_YTDLP_COOKIES)
            .await
            .unwrap(),
        Some(OLD.to_string())
    );
    assert_eq!(
        std::fs::read_to_string(ytdlp::cookies_file_path()).unwrap(),
        OLD,
        "cookies.txt must be rolled back to match the row"
    );
}

/// Same for removal: a failed row delete puts the file back.
#[tokio::test]
async fn failed_row_delete_restores_the_previous_file() {
    let _serial = SERIAL.lock().await;
    let app = boot().await;
    seed_old(&app.pool).await;
    sqlx::query(
        "CREATE TRIGGER reject_cookie_delete BEFORE DELETE ON app_config \
         WHEN OLD.key = 'ytdlp_cookies' \
         BEGIN SELECT RAISE(ABORT, 'rejected by test trigger'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    ytdlp::replace_cookies(&app.pool, None)
        .await
        .expect_err("database failure must surface");

    sqlx::query("DROP TRIGGER reject_cookie_delete")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        get_config_value(&app.pool, KEY_YTDLP_COOKIES)
            .await
            .unwrap(),
        Some(OLD.to_string())
    );
    assert_eq!(
        std::fs::read_to_string(ytdlp::cookies_file_path()).unwrap(),
        OLD
    );
}

/// Re-uploading the jar already on record changes nothing, and in
/// particular must not advance the cookie generation (which would make
/// every extraction in flight decline to cache).
#[tokio::test]
async fn unchanged_upload_does_not_advance_the_generation() {
    let _serial = SERIAL.lock().await;
    let app = boot().await;
    seed_old(&app.pool).await;
    let before = ytdlp::cookie_generation();

    ytdlp::replace_cookies(&app.pool, Some(OLD)).await.unwrap();

    assert_eq!(ytdlp::cookie_generation(), before);
    assert_eq!(
        std::fs::read_to_string(ytdlp::cookies_file_path()).unwrap(),
        OLD
    );

    // Deleting when nothing is stored is likewise a no-op.
    ytdlp::replace_cookies(&app.pool, None).await.unwrap();
    let after_delete = ytdlp::cookie_generation();
    ytdlp::replace_cookies(&app.pool, None).await.unwrap();
    assert_eq!(ytdlp::cookie_generation(), after_delete);
}

/// The row is the source of truth; if `cookies.txt` has gone missing, a
/// same-content upload re-derives it (owner-only) without advancing the
/// generation. A file that is present is left alone, since it may hold
/// yt-dlp's rotated session cookies.
#[tokio::test]
async fn unchanged_upload_restores_a_missing_file_but_keeps_a_present_one() {
    let _serial = SERIAL.lock().await;
    let app = boot().await;
    seed_old(&app.pool).await;
    let path = ytdlp::cookies_file_path();
    std::fs::remove_file(&path).unwrap();
    let before = ytdlp::cookie_generation();

    ytdlp::replace_cookies(&app.pool, Some(OLD)).await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD);
    assert_eq!(mode_of(&path), 0o600);
    assert_eq!(ytdlp::cookie_generation(), before);

    let rotated = format!("{OLD}.youtube.com\tTRUE\t/\tFALSE\t0\tROTATED\t9\n");
    std::fs::write(&path, &rotated).unwrap();
    ytdlp::replace_cookies(&app.pool, Some(OLD)).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        rotated,
        "a present file must not be clobbered by a same-content upload"
    );
    assert_eq!(ytdlp::cookie_generation(), before);
}

/// If the file can't be written, the transition stops before the row
/// write: both sides still hold the previous jar and the canonical file
/// is not partially rewritten.
#[tokio::test]
async fn failed_file_write_leaves_both_sides_untouched() {
    use std::os::unix::fs::PermissionsExt;

    let _serial = SERIAL.lock().await;
    let app = boot().await;
    // A private directory we're allowed to make read-only (the shared
    // fixture path lives directly under the system temp dir).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let path = dir.join("cookies.txt");
    let shared_path = std::env::var("YTDLP_COOKIES_PATH").unwrap();
    unsafe { std::env::set_var("YTDLP_COOKIES_PATH", path.to_str().unwrap()) };
    seed_old(&app.pool).await;
    // Read-only directory: the staged sibling file can't be created.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(dir.join("probe"), b"").is_ok() {
        // Running as root; directory permissions aren't enforced.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("YTDLP_COOKIES_PATH", &shared_path) };
        return;
    }

    let outcome = ytdlp::replace_cookies(&app.pool, Some(NEW)).await;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe { std::env::set_var("YTDLP_COOKIES_PATH", &shared_path) };
    outcome.expect_err("file write failure must surface");

    assert_eq!(
        get_config_value(&app.pool, KEY_YTDLP_COOKIES)
            .await
            .unwrap(),
        Some(OLD.to_string())
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD);
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".new."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging files left behind: {leftovers:?}"
    );
}
