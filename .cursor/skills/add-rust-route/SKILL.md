---
name: add-rust-route
description: Add a new HTTP endpoint to the HomeTube Rust/Axum backend. Covers creating or extending a route module under src/routes/, registering it in the right access-gated sub-router in src/routes/mod.rs, returning typed errors, and putting reusable logic in src/services/. Use when adding any backend API or page route.
---

# Add a Rust/Axum route

Routes are organized one module per domain under `src/routes/`. Handlers are
thin; shared business logic lives in `src/services/`. All modules are composed
in `src/routes/mod.rs`.

## Steps

1. **Pick or create the module.** Reuse an existing domain module under
   `src/routes/` (e.g. `likes.rs`, `feed.rs`) when the endpoint belongs to it.
   For a brand-new domain, add `src/routes/<domain>.rs` and declare it in
   `src/routes/mod.rs` with `pub mod <domain>;` (keep the list alphabetical).

2. **Write the handler.** Follow `src/routes/likes.rs`:
   - Extract state with `State(state): State<AppState>`; the DB pool is
     `state.db`.
   - For authenticated routes, take `current: CurrentAccount`
     (`crate::middleware::auth::CurrentAccount`) to get the signed-in account.
   - Use `Path(..)`, `Json(..)`, query extractors as needed.
   - Return `AppResult<T>` (alias for `Result<T, AppError>` from
     `crate::error`). Use `AppError::NotFound`, `Forbidden`, `BadRequest(..)`,
     etc.; `sqlx::Error` and `reqwest::Error` convert via `?` automatically.
   - Add a `/// METHOD /path` doc comment (e.g. `/// POST /api/likes/{video_id}`).

3. **Register the route** in `src/routes/mod.rs` `router()` inside the correct
   sub-router so the right middleware gate applies:
   - `parent_only` — wrapped with `require_parent` (admin APIs).
   - `child_routes` / `child_only` — wrapped with `require_child`.
   - `video_routes` / `proxy_routes` — playback, open to both roles (handlers
     enforce allowlist via `crate::services::access::can_child_view`).
   - `auth_routes`, `setup_routes`, `page_routes` — auth/setup/HTML pages.

   Example:

   ```rust
   .route("/api/likes/{video_id}", post(likes::like).delete(likes::unlike))
   ```

   Note: path params use `{name}` (Axum 0.8 syntax).

4. **Extract heavy logic to `src/services/`** if it's non-trivial or reused
   (e.g. yt-dlp calls, cache, feed). Keep the handler focused on
   request/response shaping.

5. **Add an integration test** under `tests/` — see the
   `write-rust-integration-test` skill. There are no unit tests inside `src/`.

## Verify

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
