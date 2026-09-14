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
