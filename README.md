# HomeTube

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

A self-hosted YouTube frontend for kids that gives parents fine-grained
control over what content their children can access, while syncing
children's activity (likes, subscriptions) back to their real YouTube
accounts.

## Status

Beta. Phases 0–19 of the implementation plan are complete: the app is
feature-complete for self-hosting, and the remaining work is hardening
+ release engineering.

## Highlights

- **Parent-controlled allowlists** of channels and individual videos,
  with parental preview before allowlisting
- **Profile switcher with PIN protection** for parent accounts
- **Two-way YouTube sync** — likes and subscriptions round-trip to the
  child's real Google account
- **Daily-limit enforcement** with per-day-of-week schedules + audio
  fade-out wind-down
- **No ads, no comments, no algorithmic recommendations** by design
- **Sleep timer + wind-down overlay** — fades out audio and pauses
  playback at expiry
- **Captions + audio-only mode** — server-side WebVTT conversion via
  yt-dlp; one-tap toggle to listen without video
- **Watch-activity dashboard** — daily / weekly / monthly summary,
  bar-chart of last-30-day totals, top channels, full history, and
  search log
- **Parent notifications** — bell + dropdown for yt-dlp failures,
  sync errors, and system updates
- **Server-side caching** — yt-dlp metadata + on-disk DASH segment cache
  with LRU eviction and parent-tunable size
- **Self-hosted, single-binary deployment** behind a single Docker
  image

## Architecture

- **Backend**: Rust + Axum + SQLite (sqlx) + askama templates + yt-dlp
- **Frontend**: Lit web components + Web Awesome + vidstack player,
  bundled with Vite
- **Routing**: Multi-page app — Axum serves HTML, components hydrate
  per-page
- **Proxy**: `/api/proxy/segment`, `/api/proxy/audio`, and
  `/api/proxy/thumbnail/:videoId` are gated behind a per-account /
  per-IP token bucket (200 req/min, refilled continuously) to keep the
  server's egress predictable

The implementation plan (architecture diagrams, design decisions, and
phase-by-phase TODOs) lives under [`plans/`](plans/):

- [`plans/1778451852595-cosmic-panda.md`](plans/1778451852595-cosmic-panda.md)
  — the original 20-phase implementation plan.
- [`plans/1778483706981-followup-gaps.md`](plans/1778483706981-followup-gaps.md)
  — follow-up tasks (T-1 through T-11) that close partial-implementation
  gaps and divergences from the original plan.

See the `Architecture` section of the source code's inline doc comments
for a tour of the major modules.

## Development

Requirements:

- [`rustup`](https://rustup.rs/) (toolchain pinned via `rust-toolchain.toml`)
- [`cargo-watch`](https://github.com/watchexec/cargo-watch) — `cargo install cargo-watch`
- [`nvm`](https://github.com/nvm-sh/nvm) (Node version pinned via `.nvmrc`)
- [`tilt`](https://tilt.dev/) for the dev environment
- `yt-dlp` on `PATH`
- `ffmpeg` on `PATH` (used by yt-dlp for caption conversion)

```bash
nvm use
cd frontend && npm install && cd ..
tilt up
```

App runs at <http://localhost:3000>.

## Deployment

```bash
cd docker && docker compose up -d
```

This starts two containers:
- **app** — the HomeTube server on port 3000
- **pot-server** — a PO (Proof-of-Origin) token server that helps yt-dlp
  bypass YouTube's bot detection. Runs automatically in the background;
  no configuration needed.

Then open <http://localhost:3000> and follow the setup wizard.

See [`docs/deployment.md`](docs/deployment.md) for a full deploy
walkthrough, backups, and reverse-proxy notes, and
[`docs/google-cloud-setup.md`](docs/google-cloud-setup.md) for the
step-by-step on creating the Google Cloud project that the setup
wizard asks for.

## Known limitations

- **yt-dlp dependency** — Stream extraction may break temporarily when
  YouTube updates. The daily auto-update cron job mitigates this, but
  brief outages may occur. Failed extractions surface in the parent
  notification bell.
- **YouTube Data API quotas** — 10,000 units/day per project. With
  ~3 children on the default hourly sync schedule, expect 720–1,440
  units/day; well within the limit but tight if you also use the
  parent search heavily.
- **Video proxy bandwidth** — All bytes flow through the server. On a
  LAN this is a non-issue; remote use requires upstream bandwidth
  (~5–8 Mbps per 1080p stream).
- **Offline downloads** — OPFS storage varies by browser/device; Safari
  in private browsing disables OPFS entirely. Background Fetch is
  Chromium-only.
- **Single family** — One instance = one family. No multi-tenancy.

## License

[AGPL-3.0](LICENSE) — self-hostable, modifications must remain open
source, including for network-accessible deployments.
