---
name: add-sqlx-migration
description: Add a new SQLite schema migration to HomeTube. Covers the migrations/ numbering convention, SQLite-friendly DDL conventions, how migrations auto-apply at startup and in tests, and keeping models/queries in sync. Use when changing the database schema.
---

# Add a sqlx migration

HomeTube applies migrations from `migrations/` at startup via
`sqlx::migrate!("./migrations")` in `src/db/mod.rs` (`migrate()`), and the test
harness runs the same migrator against in-memory SQLite. There is no separate
"run migrations" command — they apply automatically.

## Steps

1. **Create the next file** `migrations/NNN_<snake_case_description>.sql`.
   Numbering is zero-padded three digits and strictly sequential — find the
   current highest (e.g. `026_cron_jobs_derived.sql`) and use the next number.
   Do NOT renumber or edit already-shipped migrations; they are immutable once
   merged.

2. **Write SQLite-compatible DDL.** Follow `migrations/022_thumbnail_cache.sql`:
   - Lead with a comment block explaining the why (and reference related
     services/migrations).
   - Use `INTEGER` unix timestamps with `DEFAULT (unixepoch())` for times.
   - Add `CHECK` constraints where useful; create indexes explicitly
     (`CREATE INDEX idx_<table>_<cols> ...`).
   - Remember SQLite's limited `ALTER TABLE` (it can add columns but not drop
     or alter them in older patterns); for complex changes, the existing
     migrations use table-rebuild patterns — check a nearby migration that does
     a similar change.
   - Foreign keys are enabled (`foreign_keys(true)`), so declare `REFERENCES`
     and rely on them.

3. **Keep code in sync:**
   - Update or add structs in `src/models/` and any `sqlx::FromRow` types /
     hand-written SQL in `src/routes/` and `src/services/` that touch the
     changed tables.
   - If tests seed the affected tables, update the helpers in
     `tests/common/mod.rs` (e.g. `seed_video`, `allowlist_video`).

4. **No SQLx offline cache to regenerate** — the project uses runtime-checked
   queries (`sqlx::query`, `query_as`), not the compile-time `query!` macros,
   so there is no `.sqlx/` to refresh.

## Verify

```bash
cargo test        # boots the migrator against in-memory SQLite
cargo run         # applies against the real data/database/ on startup
```

A clean `cargo test` confirms the migration applies and the schema audit in
`src/db/mod.rs` runs without error.
