//! SQL building primitives shared across services.
//!
//! Currently houses one helper: [`row_placeholders`], which builds the
//! `(?,?,...),(?,?,...),...` VALUES tail of a multi-row INSERT, and
//! the [`MAX_BIND_PARAMS`] ceiling that call sites should chunk
//! against. Both are deliberately tiny and audit-friendly so callers
//! can safely wrap the resulting SQL fragment in [`sqlx::AssertSqlSafe`].
//!
//! See `services::channel_backfill::apply_backfill_entries` and
//! `services::feed_cache::upsert_channel_videos` for the canonical call
//! sites; their inline comments describe the per-call-site reasoning
//! for `cols` and chunk sizing.
//!
//! ## Why a shared module?
//!
//! Two near-identical implementations of "build N copies of `(?)`"
//! drifted between `channel_backfill.rs` and `feed_cache.rs`. Both
//! relied on the same `AssertSqlSafe` invariant (the SQL string only
//! ever contains `?`, `,`, `(`, `)` — no caller-provided strings), and
//! both needed to chunk against the SQLite 999-parameter ceiling.
//! Consolidating here means there's one audit surface for that
//! invariant and one constant for the ceiling.
//!
//! ## Why 900 instead of 999?
//!
//! `SQLITE_MAX_VARIABLE_NUMBER` defaults to 999 on builds we ship
//! against, but sqlx itself burns a handful of binds for internal
//! housekeeping on some queries (e.g. RETURNING clauses with multiple
//! rows). 900 leaves comfortable headroom; on a typical batch this is
//! noise in throughput terms.
//!
//! Some Linux distros (notably newer Debian/Ubuntu) ship a SQLite with
//! the default raised to 32766 — we deliberately don't take advantage
//! of that, because a binary built against a high-limit system would
//! crash at runtime if run against an embedded SQLite with the smaller
//! default.

/// Maximum number of bind parameters a single statement should use.
///
/// Conservative cap below SQLite's default `SQLITE_MAX_VARIABLE_NUMBER`
/// of 999 to leave headroom for sqlx-internal binds. See module docs
/// for the rationale.
pub const MAX_BIND_PARAMS: usize = 900;

// Compile-time guard: a future bump must stay below the
// SQLITE_MAX_VARIABLE_NUMBER default (999) that older SQLite builds
// ship with. Otherwise a binary built here would crash at runtime on
// an embedded-SQLite deployment. Const-block assert is evaluated at
// build time, so the failure mode is a clear compile error rather
// than a test that nobody runs.
const _: () = assert!(MAX_BIND_PARAMS < 999);

/// Build a `(?,?,...),(?,?,...),...` placeholder string for a
/// multi-VALUES INSERT with `rows` tuples of `cols` columns each.
///
/// The signature deliberately takes only `usize` arguments — no
/// caller-provided strings — so the result is provably free of any
/// content that could form a SQL injection vector. This is what
/// makes it safe to feed into `sqlx::AssertSqlSafe` at the call
/// sites; a hand-rolled `format!` is easy to "improve" with an
/// interpolated table or column name that silently loses that
/// guarantee.
///
/// Examples:
///
/// * `row_placeholders(3, 1)` → `"(?),(?),(?)"`
/// * `row_placeholders(2, 3)` → `"(?,?,?),(?,?,?)"`
///
/// Panics if `rows == 0` or `cols == 0` — both indicate a bug in the
/// caller (an empty INSERT is not a meaningful operation).
///
/// Also `debug_assert!`s that `rows * cols` stays under
/// [`MAX_BIND_PARAMS`]. Call sites are expected to compute
/// `chunk_size = MAX_BIND_PARAMS / cols` before chunking, so this
/// assertion is a guard against a future caller that forgets the
/// `cols`-aware sizing — release builds will still attempt the bind
/// and SQLite will surface a clear "too many SQL variables" error
/// rather than silently corrupting the request.
pub fn row_placeholders(rows: usize, cols: usize) -> String {
    assert!(
        rows > 0 && cols > 0,
        "row_placeholders: rows and cols must be > 0"
    );
    debug_assert!(
        rows.saturating_mul(cols) <= MAX_BIND_PARAMS,
        "row_placeholders: {rows} * {cols} = {} exceeds MAX_BIND_PARAMS ({MAX_BIND_PARAMS})",
        rows.saturating_mul(cols)
    );
    let one_row = format!(
        "({})",
        std::iter::repeat_n("?", cols).collect::<Vec<_>>().join(",")
    );
    std::iter::repeat_n(one_row.as_str(), rows)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_only_safe_characters() {
        // Single column, single row.
        assert_eq!(row_placeholders(1, 1), "(?)");
        // Single column, multiple rows.
        assert_eq!(row_placeholders(3, 1), "(?),(?),(?)");
        // Multiple columns, single row.
        assert_eq!(row_placeholders(1, 3), "(?,?,?)");
        // Multiple columns, multiple rows.
        assert_eq!(row_placeholders(2, 3), "(?,?,?),(?,?,?)");
        // Contract: only `?`, `,`, `(`, `)` ever appear. This is the
        // load-bearing property that lets call sites wrap the result
        // in `sqlx::AssertSqlSafe` without per-call audit.
        let big = row_placeholders(50, 4);
        assert!(
            big.bytes().all(|b| matches!(b, b'?' | b',' | b'(' | b')')),
            "row_placeholders must emit only ?,()/comma — got {big}"
        );
    }

    #[test]
    #[should_panic(expected = "rows and cols must be > 0")]
    fn panics_on_zero_rows() {
        row_placeholders(0, 1);
    }

    #[test]
    #[should_panic(expected = "rows and cols must be > 0")]
    fn panics_on_zero_cols() {
        row_placeholders(1, 0);
    }
}
