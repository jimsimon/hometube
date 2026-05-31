---
name: write-rust-integration-test
description: Write a Rust integration test for a HomeTube backend route using the shared tests/common harness (in-memory SQLite + axum_test::TestServer, pre-signed session cookies, fixture seed helpers, wiremock for external HTTP). Use when adding or changing any backend route or service that needs test coverage.
---

# Write a Rust integration test

HomeTube has NO `#[test]` modules inside `src/`. All Rust tests are integration
tests under `tests/`, each top-level file compiling to its own binary. The
shared harness lives in `tests/common/mod.rs` and is pulled in with
`mod common;`.

## Steps

1. **Pick the file.** Add to the existing `tests/<domain>.rs` that matches the
   route (e.g. likes/subscriptions/blocked live in `tests/likes_subs_blocked.rs`),
   or create a new `tests/<domain>.rs`. Start a new file with `mod common;`.

2. **Boot an app.** Use a harness constructor from `tests/common/mod.rs`:
   - `boot()` — empty app, `setup_complete = false`, no accounts (setup-flow
     tests).
   - `boot_setup_complete(AccountType::Parent | Child)` — provisioned app with a
     signed session cookie for that role already in the jar.
   - `boot_with_parent_and_child(role)` — seeds both a parent and a child, signs
     in as `role` (use for parent/child gating tests).

   Each returns a `TestApp { server, pool, key, parent_id, child_id, cache_dir }`.

3. **Seed fixtures** via the helpers (they delegate to production upsert
   helpers so schema changes stay in sync): `seed_video`, `seed_channel`,
   `allowlist_video`, `allowlist_channel`, `seed_like`, `seed_watch_history`,
   `seed_blocked`, `seed_offline_download`, `seed_channel_video`. Use
   `app.pool` directly for custom SQL.

4. **Drive requests** with `app.server` (`axum_test::TestServer`):

   ```rust
   #[tokio::test]
   async fn like_creates_row_without_body() {
       let (app, _auth) = boot_with_parent_and_child(AccountType::Child).await;
       let res = app.server.post("/api/likes/vid-liked").await;
       assert_eq!(res.status_code(), StatusCode::OK);
       let body: serde_json::Value = res.json();
       assert_eq!(body["video_id"], "vid-liked");
   }
   ```

   Use `.json(&json!({...}))` for bodies. The session cookie is added
   automatically by the `boot_*` helpers; mint extra ones with
   `mint_session_cookie(&app, account_id)` if needed.

5. **Mock external HTTP** (discovery sidecar, etc.) with `wiremock` when a path
   calls out; see `tests/youtube_mocked.rs`. Unmocked external calls degrade
   gracefully in the harness, so only mock when asserting on the response.

## Coverage gate

CI runs `cargo llvm-cov --fail-under-lines 80`. New route logic must keep line
coverage at or above 80%.

## Verify

```bash
cargo test
# optional, matches CI:
cargo llvm-cov --fail-under-lines 80   # requires cargo-llvm-cov
```
