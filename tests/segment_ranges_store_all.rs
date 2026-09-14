//! Coverage for `segment_ranges::store_all`, the single-transaction
//! batch write used when a manifest is synthesized. Per-row autocommits
//! for several hundred ranged formats were holding SQLite's write lock
//! for tens of seconds per playback.

mod common;

use common::boot;
use hometube::services::segment_ranges::{
    lookup_all, store_all, BoxRanges, ByteRange, RangePersistMemo,
};

/// Arbitrary but valid init/index byte ranges for one format.
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

/// Build `(format_id, url)` pairs for the memo from bare format ids.
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
    store_all(&app.pool, "vid", &rows).await.unwrap();
    let map = lookup_all(&app.pool, "vid", &inputs(&["137", "248", "251"])).await;
    assert_eq!(map.get("137"), Some(&sample_ranges()));
    assert_eq!(map.get("248"), Some(&sample_ranges()));
    assert!(!map.contains_key("251"));

    store_all(&app.pool, "vid", &[("137".to_string(), updated)])
        .await
        .unwrap();
    let map = lookup_all(&app.pool, "vid", &inputs(&["137", "248"])).await;
    assert_eq!(map.get("137"), Some(&updated));
    assert_eq!(map.get("248"), Some(&sample_ranges()));

    store_all(&app.pool, "vid", &[]).await.unwrap(); // no-op
}

/// Re-storing ranges must not wipe the `total_bytes` the segment store
/// records on the same row (a plain `INSERT OR REPLACE` would).
#[tokio::test]
async fn store_all_preserves_total_bytes() {
    let app = boot().await;
    store_all(&app.pool, "vid", &[("137".to_string(), sample_ranges())])
        .await
        .unwrap();
    sqlx::query(
        "UPDATE format_box_ranges SET total_bytes = 123456 \
         WHERE video_id = 'vid' AND format_id = '137'",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    store_all(&app.pool, "vid", &[("137".to_string(), sample_ranges())])
        .await
        .unwrap();
    let total: Option<i64> = sqlx::query_scalar(
        "SELECT total_bytes FROM format_box_ranges \
         WHERE video_id = 'vid' AND format_id = '137'",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(total, Some(123456));
}

/// A failed batch surfaces as an error (so the caller can retry) and
/// writes nothing: the transaction rolls back as a unit, so a row that
/// was inserted before the failing one does not survive.
#[tokio::test]
async fn store_all_reports_failure_and_writes_nothing() {
    let app = boot().await;
    // Make the second row's insert fail after the first has already been
    // written inside the same transaction.
    sqlx::query(
        "CREATE TRIGGER reject_248 BEFORE INSERT ON format_box_ranges \
         WHEN NEW.format_id = '248' \
         BEGIN SELECT RAISE(ABORT, 'rejected by test trigger'); END",
    )
    .execute(&app.pool)
    .await
    .unwrap();

    let err = store_all(
        &app.pool,
        "vid",
        &[
            ("137".to_string(), sample_ranges()),
            ("248".to_string(), sample_ranges()),
        ],
    )
    .await
    .expect_err("a failing row must be reported, not swallowed");
    assert!(
        err.to_string().contains("rejected by test trigger"),
        "unexpected error: {err}"
    );

    sqlx::query("DROP TRIGGER reject_248")
        .execute(&app.pool)
        .await
        .unwrap();
    let stored: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM format_box_ranges WHERE video_id = 'vid'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        stored, 0,
        "the row inserted before the failure must roll back"
    );
}

/// The memo hands out a token for a new set, suppresses duplicates while
/// a write is in flight or after it committed, and releases the set
/// again when the write is aborted so the next request retries.
#[test]
fn persist_memo_retries_after_abort_and_suppresses_after_commit() {
    let memo = RangePersistMemo::default();
    let mut rows = vec![
        ("248".to_string(), sample_ranges()),
        ("137".to_string(), sample_ranges()),
    ];

    let token = memo.begin("vid", &mut rows).expect("first claim wins");
    // Sorted in place so the fingerprint is order-independent.
    assert_eq!(rows[0].0, "137");
    // Same set, different input order: in flight, so suppressed.
    let mut reordered = vec![
        ("137".to_string(), sample_ranges()),
        ("248".to_string(), sample_ranges()),
    ];
    assert_eq!(memo.begin("vid", &mut reordered), None);

    // The write failed: the next request must be allowed to retry.
    memo.abort("vid", token);
    let retry = memo
        .begin("vid", &mut rows)
        .expect("abort releases the set");
    assert_eq!(retry, token, "same set, same fingerprint");

    // Committed: suppressed from now on for this set...
    memo.commit("vid", retry);
    assert_eq!(memo.begin("vid", &mut rows), None);
    // ...and a stale abort for an already-committed token is a no-op.
    memo.abort("vid", retry);
    assert_eq!(memo.begin("vid", &mut rows), None);

    // A different set for the same video is new work, and other videos
    // are independent.
    let mut changed = vec![("137".to_string(), sample_ranges())];
    assert!(memo.begin("vid", &mut changed).is_some());
    assert!(memo.begin("other", &mut rows).is_some());
}

/// A newer set claimed while an older write is still in flight must not
/// be clobbered when the older write finally commits or aborts.
#[test]
fn persist_memo_ignores_outcomes_of_superseded_writes() {
    let memo = RangePersistMemo::default();
    let mut old = vec![("137".to_string(), sample_ranges())];
    let mut new = vec![
        ("137".to_string(), sample_ranges()),
        ("248".to_string(), sample_ranges()),
    ];

    let old_token = memo.begin("vid", &mut old).unwrap();
    let new_token = memo
        .begin("vid", &mut new)
        .expect("a different set supersedes");
    assert_ne!(old_token, new_token);

    memo.commit("vid", old_token); // stale: must not mark the new set persisted
    memo.abort("vid", old_token); // stale: must not release the new set
    assert_eq!(memo.begin("vid", &mut new), None, "new set still in flight");

    memo.commit("vid", new_token);
    assert_eq!(memo.begin("vid", &mut new), None);
}
