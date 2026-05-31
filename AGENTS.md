# AGENTS.md

Guidance for AI coding agents working in the HomeTube repository.

## Overview

HomeTube is a self-hosted, parent-controlled YouTube frontend for kids. A
PIN-gated parent admin allowlists channels and individual videos; children get
an ad-free, comment-free, recommendation-free UI with no Google account
required.

## Architecture

Rust/Axum + SQLite serves Askama HTML, JSON APIs, and static assets, and proxies
video segments via yt-dlp. The frontend is a Lit/Vite **multi-page app** (each
page hydrates its own Lit components — it is not an SPA). Two Node sidecars
support it: a `youtubei.js` discovery service (search/metadata) and a bgutil
PO-token provider (yt-dlp bot-detection bypass).

## Tech stack

- **Backend**: Rust 2021, Axum 0.8, sqlx 0.9 (SQLite), Askama 0.16, Tokio
- **Frontend**: TypeScript (strict), Lit 3, Vite 8, Web Awesome, Shaka Player,
  Three.js (360°), Workbox PWA
- **Node**: 24 (see `.nvmrc`)
- **Tests**: Vitest (browser mode) for frontend, Playwright for E2E, Rust
  integration tests under `tests/`

## Layout

- `src/{routes,services,models,db,middleware}` — Rust library crate
  - `routes/` — one module per domain, composed in `src/routes/mod.rs`
  - `services/` — business logic
- `templates/` — Askama HTML (parent/child pages, partials)
- `migrations/` — sqlx migrations (`NNN_*.sql`), auto-applied at startup
- `frontend/src/{components,services}` — Lit components and TS services with
  colocated `*.test.ts`
- `tests/` — Rust integration tests (harness in `tests/common/mod.rs`)
- `e2e/` — Playwright specs
- `sidecar/discovery/` — Node discovery sidecar
- `docker/` — app Dockerfile + production compose

## Key commands

```bash
# Install
nvm use && cd frontend && npm install && cd ../sidecar/discovery && npm install && cd ../..

# Dev (backend :3000, frontend watch, discovery :3001, pot :4416)
tilt up   # http://localhost:3000

# Frontend checks (run in frontend/)
npm run lint && npm run format:check && npm run typecheck && npm test

# Backend checks (repo root)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test

# E2E (release build with test-login, server running)
cargo build --release --features test-login
cd e2e && npm install && npx playwright test
```

These mirror the CI gates in `.github/workflows/ci.yml` and the pre-commit hook
in `.husky/pre-commit`.

## Conventions

- **Rust**: library crate plus integration tests under `tests/` (no `#[test]`
  modules in `src/`). Add a domain route file under `src/routes/`, register it
  in `src/routes/mod.rs`, and keep logic in `src/services/`. Return typed errors
  via `src/error`.
- **Frontend**: custom elements are `hometube-*` (kebab-case) in
  `kebab-case.ts` files under `frontend/src/components/`. TypeScript strict mode;
  use `.js` suffixes on relative imports. Colocate tests as `*.test.ts`.
- **Migrations**: add the next `NNN_*.sql` in `migrations/`; they run
  automatically at startup via `sqlx::migrate!`.

## Gotchas

- The `test-login` cargo feature exposes E2E-only routes and **must never ship
  to production** (`#[cfg(feature = "test-login")]`).
- Linting/formatting uses **oxlint/oxfmt** (frontend) and **cargo clippy/fmt**
  (backend) — there is no ESLint, Prettier, Ruff, Black, or mypy.
- There is **no root `package.json`**; Node lives under `frontend/`, `e2e/`, and
  `sidecar/discovery/`.
- Coverage gates: Rust `--fail-under-lines 80`; frontend thresholds
  (80/80/75/70) in `frontend/vitest.config.ts`.
