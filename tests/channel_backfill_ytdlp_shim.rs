//! End-to-end integration test for the channel-backfill subprocess
//! layer.
//!
//! Wires `Config::ytdlp_path` at a tiny shell script that emits canned
//! yt-dlp `--flat-playlist` JSON output and exercises the real
//! `ytdlp::flat_playlist_channel` parser + the
//! `channel_backfill::apply_backfill_entries` DB layer in sequence,
//! proving the subprocess → channel_videos pipeline works without
//! ever talking to YouTube.

use std::io::Write;

use hometube::config::Config;
use hometube::services::ytdlp::{self, FlatPlaylistTunables};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;

async fn setup_db() -> SqlitePool {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// Write a shell script that mimics `yt-dlp --flat-playlist -j` by
/// emitting a fixed sequence of JSON lines on stdout and exiting 0.
/// Returns the path to the script.
fn write_ytdlp_shim(json_lines: &[&str]) -> std::path::PathBuf {
    let nonce: u64 = rand::random();
    let mut path = std::env::temp_dir();
    path.push(format!("hometube-ytdlp-shim-{nonce:x}.sh"));

    let mut f = std::fs::File::create(&path).unwrap();
    // Use printf, not echo, to control newlines precisely.
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "# Ignore every argument — we always emit the same canned output.").unwrap();
    for line in json_lines {
        // Escape single quotes so the JSON survives embedding inside
        // the shell single-quoted string.
        let escaped = line.replace('\'', r#"'\''"#);
        writeln!(f, "printf '%s\\n' '{escaped}'").unwrap();
    }
    writeln!(f, "exit 0").unwrap();
    drop(f);

    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn config_with_ytdlp(path: &std::path::Path) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: "sqlite::memory:".into(),
        ytdlp_path: path.to_string_lossy().into_owned(),
        static_dir: "./frontend/dist".into(),
        cache_dir: "./data/cache".into(),
    }
}

#[tokio::test]
async fn flat_playlist_parses_canned_jsonl_output() {
    let shim = write_ytdlp_shim(&[
        r#"{"id":"vA","title":"Alpha","upload_date":"20240101","duration":120.0,"view_count":500,"channel":"Channel One","channel_id":"UCfake"}"#,
        r#"{"id":"vB","title":"Beta","upload_date":"20240615","duration":null,"view_count":null,"channel":"Channel One","channel_id":"UCfake"}"#,
        // Empty line — verify the parser tolerates blanks.
        "",
        r#"{"id":"vC","title":"Gamma","upload_date":null,"channel":"Channel One","channel_id":"UCfake"}"#,
    ]);
    let cfg = config_with_ytdlp(&shim);

    let tunables = FlatPlaylistTunables {
        timeout: std::time::Duration::from_secs(10),
        sleep_requests_s: 0,
        sleep_interval_s: 0,
        max_sleep_interval_s: 0,
    };

    let result = ytdlp::flat_playlist_channel(&cfg, "UCfake", &tunables)
        .await
        .unwrap();

    let _ = std::fs::remove_file(&shim);

    assert_eq!(result.entries.len(), 3, "expected three parsed entries");
    assert_eq!(result.entries[0].video_id, "vA");
    assert_eq!(result.entries[0].title.as_deref(), Some("Alpha"));
    assert_eq!(result.entries[0].upload_date.as_deref(), Some("20240101"));
    assert_eq!(result.entries[0].duration, Some(120.0));
    assert_eq!(result.entries[0].view_count, Some(500));

    assert_eq!(result.entries[1].video_id, "vB");
    assert_eq!(result.entries[1].duration, None);

    assert_eq!(result.entries[2].video_id, "vC");
    assert!(result.entries[2].upload_date.is_none());
}

#[tokio::test]
async fn flat_playlist_surfaces_subprocess_failure() {
    // Shim that exits non-zero with an error on stderr.
    let nonce: u64 = rand::random();
    let mut path = std::env::temp_dir();
    path.push(format!("hometube-ytdlp-fail-shim-{nonce:x}.sh"));
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "echo 'Sign in to confirm you are not a bot' 1>&2").unwrap();
    writeln!(f, "exit 1").unwrap();
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();

    let cfg = config_with_ytdlp(&path);
    let tunables = FlatPlaylistTunables {
        timeout: std::time::Duration::from_secs(10),
        sleep_requests_s: 0,
        sleep_interval_s: 0,
        max_sleep_interval_s: 0,
    };

    let err = ytdlp::flat_playlist_channel(&cfg, "UCfake", &tunables)
        .await
        .expect_err("expected the shim to surface a failure");
    let msg = err.to_string();
    assert!(
        msg.contains("Sign in"),
        "stderr tail must surface in the error message; got: {msg}"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn flat_playlist_tolerates_unparseable_lines_among_valid_entries() {
    // Real-world yt-dlp output can include progress/info lines mixed
    // with the JSON entries. The parser should skip the bad lines and
    // surface the valid ones.
    let shim = write_ytdlp_shim(&[
        r#"{"id":"vA","title":"Alpha","channel":"X","channel_id":"UCx"}"#,
        "not valid json",
        r#"[Generic] Failure — bullet point line"#,
        r#"{"id":"vB","title":"Beta","channel":"X","channel_id":"UCx"}"#,
    ]);
    let cfg = config_with_ytdlp(&shim);

    let result = ytdlp::flat_playlist_channel(
        &cfg,
        "UCx",
        &FlatPlaylistTunables {
            timeout: std::time::Duration::from_secs(10),
            sleep_requests_s: 0,
            sleep_interval_s: 0,
            max_sleep_interval_s: 0,
        },
    )
    .await
    .unwrap();
    let _ = std::fs::remove_file(&shim);

    let ids: Vec<&str> = result.entries.iter().map(|e| e.video_id.as_str()).collect();
    assert_eq!(ids, vec!["vA", "vB"], "bad lines must be skipped");
}

#[tokio::test]
async fn full_subprocess_to_db_pipeline_populates_channel_videos() {
    // This test exercises the full chain: spawn the shim, parse the
    // output, run the entries through the same DB layer the production
    // backfill loop uses. We hit `apply_backfill_entries` directly
    // rather than `run_backfill_for` so the test doesn't need to
    // emulate the loop's atomic claim step.
    use hometube::services::channel_backfill::{self, BackfillConfig};
    use hometube::services::feed_cache;

    let shim = write_ytdlp_shim(&[
        r#"{"id":"vA","title":"Alpha","upload_date":"20240101","channel":"Channel One","channel_id":"UCpipe"}"#,
        r#"{"id":"vB","title":"Beta","upload_date":"20240615","channel":"Channel One","channel_id":"UCpipe"}"#,
    ]);
    let cfg = config_with_ytdlp(&shim);

    let pool = setup_db().await;
    feed_cache::upsert_channel(&pool, "UCpipe").await.unwrap();

    let result = ytdlp::flat_playlist_channel(
        &cfg,
        "UCpipe",
        &FlatPlaylistTunables {
            timeout: std::time::Duration::from_secs(10),
            sleep_requests_s: 0,
            sleep_interval_s: 0,
            max_sleep_interval_s: 0,
        },
    )
    .await
    .unwrap();
    let _ = std::fs::remove_file(&shim);

    // Drive the DB-side apply step (which is what the production loop
    // calls after a successful subprocess). The pub wrapper is
    // `#[doc(hidden)]`-flagged and intended only for this test.
    channel_backfill::apply_backfill_entries(
        &pool,
        "UCpipe",
        1_000,
        &result.entries,
        &BackfillConfig::default(),
    )
    .await
    .unwrap();

    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_videos WHERE channel_id = 'UCpipe'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 2);

    let titles: Vec<String> =
        sqlx::query_scalar("SELECT title FROM channel_videos WHERE channel_id = 'UCpipe' ORDER BY video_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(titles, vec!["Alpha", "Beta"]);
}
