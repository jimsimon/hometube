# HomeTube

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

A self-hosted YouTube frontend for kids that gives parents fine-grained
control over what content their children can access — allowlisted
channels and videos, no ads, no comments, no algorithmic
recommendations, and no Google account required for the children.

## Status

Beta. The core implementation plan is complete and the app is
feature-complete for self-hosting; ongoing work is hardening, polish,
and incremental features tracked through PRs.

## Highlights

- **Parent-controlled allowlists** of channels and individual videos,
  with a parental preview screen before allowlisting
- **PIN-based authentication** — no Google account required for
  parents or children; profile switching into a parent requires the
  parent PIN
- **No ads, no comments, no algorithmic recommendations** by design
- **Per-child Liked Videos** — children can like videos from a child
  profile and revisit them on a dedicated page (local-only; nothing is
  sent to YouTube)
- **Per-child Hidden Videos** — kids can hide individual videos from
  their feeds without losing the allowlist entry
- **Continue Watching + Watch Again rows** on the child dashboard
- **Channel archive sync** — full-history backfill of allowlisted
  channels with on-disk thumbnail caching, rate-limited and shelved on
  repeated errors
- **RSS-driven New Videos feed** — channel uploads are picked up by a
  background refresher (RSS first, sidecar fallback) so the child feed
  stays fresh without per-page yt-dlp calls
- **Sleep timer + wind-down overlay** — fades audio out and pauses
  playback when the timer expires
- **Captions + audio-only mode** — server-side WebVTT conversion via
  yt-dlp; one-tap toggle to listen without video
- **Chromecast support** plus multi-language audio and 360° video
  playback
- **Watch-activity dashboard** — daily / weekly / monthly summary,
  bar-chart of last-30-day totals, top channels, full watch history,
  and search log
- **Offline downloads** — OPFS-backed local storage for trips and
  patchy Wi-Fi (Chromium-only Background Fetch where supported)
- **Parent notifications** — bell + dropdown for yt-dlp failures,
  sidecar errors, and system updates, with optional forwarding to a
  self-hosted notification service
- **Server-side caching** — yt-dlp metadata + on-disk DASH segment
  cache with LRU eviction, per-video clear, eviction audit log, and a
  parent-tunable size budget
- **Self-hosted, three-container deployment** — one Docker Compose
  stack, no third-party cloud dependencies

## Architecture

- **Backend**: Rust + Axum + SQLite (sqlx) + askama templates + yt-dlp
- **Frontend**: Lit web components + Web Awesome + Shaka Player,
  bundled with Vite
- **Routing**: Multi-page app — Axum serves HTML, components hydrate
  per-page
- **Discovery sidecar**: a small Node service wrapping
  [`youtubei.js`](https://github.com/LuanRT/YouTube.js) for search,
  channel lookups, and video metadata. Replaces the YouTube Data API
  entirely, so there is no Google Cloud project to create and no API
  quota to manage.
- **PO-token sidecar**: `bgutil-ytdlp-pot-provider` runs alongside
  yt-dlp to bypass YouTube's bot-detection challenges.
- **Proxy**: `/api/proxy/segment`, `/api/proxy/audio`, and
  `/api/proxy/thumbnail/:videoId` are gated behind a per-account /
  per-IP token bucket (200 req/min, refilled continuously) to keep the
  server's egress predictable.
- **Persistent state** is split across three independent directories
  so each can live on different storage:
  - `data/database/` — SQLite (small, fsync-heavy; SSD recommended)
  - `data/tools/` — yt-dlp binary + `cookies.txt`
  - `data/cache/` — DASH segment cache + thumbnail cache (large,
    regeneratable)

The original implementation plan (architecture diagrams, design
decisions, phase-by-phase TODOs) and follow-up gap-closure plan live
under [`plans/`](plans/). See the `Architecture` section of the source
code's inline doc comments for a tour of the major modules.

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
cd sidecar/discovery && npm install && cd ../..
tilt up
```

App runs at <http://localhost:3000>.

## Deployment

```bash
cd docker && docker compose up -d
```

The Compose stack runs three containers:

- **app** — the HomeTube server on port 3000 (host port configurable
  via `HOMETUBE_PORT`)
- **discovery** — the youtubei.js sidecar; not exposed to the host,
  reached only by `app` on the internal Docker network
- **pot-server** — `bgutil-ytdlp-pot-provider`; helps yt-dlp clear
  YouTube's bot-detection challenges. No configuration needed.

All three services are required. The stack also supports TrueNAS Scale
(ElectricEel 24.10+) as a Custom App; see the comments at the top of
`docker/docker-compose.yml` for the dataset layout.

Then open <http://localhost:3000> and follow the setup wizard — it
asks for a parent name and PIN, nothing else.

See [`docs/deployment.md`](docs/deployment.md) for the full deploy
walkthrough, healthcheck details, backup notes, and reverse-proxy
guidance.

## Known limitations

- **yt-dlp dependency** — Stream extraction may break temporarily when
  YouTube updates. The daily auto-update cron mitigates this, but
  brief outages may occur. Failed extractions surface in the parent
  notification bell.
- **Video proxy bandwidth** — All bytes flow through the server. On a
  LAN this is a non-issue; remote use requires upstream bandwidth
  (~5–8 Mbps per 1080p stream).
- **Offline downloads** — OPFS storage availability varies by
  browser/device; Safari in private browsing disables OPFS entirely.
  Background Fetch is Chromium-only.
- **Single family** — One instance = one family. No multi-tenancy.

## License

[AGPL-3.0](LICENSE) — self-hostable, modifications must remain open
source, including for network-accessible deployments.
