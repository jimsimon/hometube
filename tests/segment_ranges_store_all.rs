//! Coverage for `segment_ranges::store_all`, the single-transaction
//! batch write used when a manifest is synthesized. Per-row autocommits
//! for several hundred ranged formats were holding SQLite's write lock
//! for tens of seconds per playback.

mod common;

use common::boot;
use hometube::services::segment_ranges::{lookup_all, store_all, BoxRanges, ByteRange};

fn sample_ranges() -> BoxRanges {
    BoxRanges {
        init: ByteRange {
            start: 32,
            end: 511,
        },
        index: ByteRange {
            start: 512,
            end: 4095,
        },
    }
}

fn inputs(ids: &[&str]) -> Vec<(String, String)> {
    ids.iter()
        .map(|id| ((*id).to_string(), format!("https://example/{id}")))
        .collect()
}

/// `store_all` writes every row atomically and is safe to call again
/// with the same (or an updated) set; untouched rows survive.
#[tokio::test]
async fn store_all_persists_every_row() {
    let app = boot().await;
    let mut updated = sample_ranges();
    updated.index.end = 8191;

    let rows = vec![
        ("137".to_string(), sample_ranges()),
        ("248".to_string(), sample_ranges()),
    ];
    store_all(&app.pool, "vid", &rows).await;
    let map = lookup_all(&app.pool, "vid", &inputs(&["137", "248", "251"])).await;
    assert_eq!(map.get("137"), Some(&sample_ranges()));
    assert_eq!(map.get("248"), Some(&sample_ranges()));
    assert!(!map.contains_key("251"));

    store_all(&app.pool, "vid", &[("137".to_string(), updated)]).await;
    let map = lookup_all(&app.pool, "vid", &inputs(&["137", "248"])).await;
    assert_eq!(map.get("137"), Some(&updated));
    assert_eq!(map.get("248"), Some(&sample_ranges()));

    store_all(&app.pool, "vid", &[]).await; // no-op, must not error
}

/// Re-storing ranges must not wipe the `total_bytes` the segment store
/// records on the same row (a plain `INSERT OR REPLACE` would).
#[tokio::test]
async fn store_all_preserves_total_bytes() {
    let app = boot().await;
    store_all(&app.pool, "vid", &[("137".to_string(), sample_ranges())]).await;
    sqlx::query(
        "UPDATE format_box_ranges SET total_bytes = 123456 \
         WHERE video_id = 'vid' AND format_id = '137'",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    store_all(&app.pool, "vid", &[("137".to_string(), sample_ranges())]).await;
    let total: Option<i64> = sqlx::query_scalar(
        "SELECT total_bytes FROM format_box_ranges \
         WHERE video_id = 'vid' AND format_id = '137'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(total, Some(123456));
}
