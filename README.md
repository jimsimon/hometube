# HomeTube

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

A self-hosted YouTube frontend for kids that gives parents fine-grained control
over what content their children can access, while syncing children's activity
(likes, subscriptions, playlists) back to their real YouTube accounts.

## Status

Early development. See [`.kilo/plans/`](.kilo/plans/) for the full
implementation plan.

## Highlights

- **Parent-controlled allowlists** of channels, playlists, and individual videos
- **Profile switcher with PIN protection** for parent accounts
- **Two-way YouTube sync** — likes, subscriptions, and playlists round-trip to
  the child's real Google account
- **Daily-limit enforcement** with per-day-of-week schedules
- **No ads, no comments, no algorithmic recommendations** by design
- **Offline downloads** via OPFS for road trips and spotty connections
- **Self-hosted, single-binary deployment** behind a single Docker image

## Architecture

- **Backend**: Rust + Axum + SQLite (sqlx) + askama templates + yt-dlp
- **Frontend**: Lit web components + Web Awesome + vidstack player, bundled
  with Vite
- **Routing**: Multi-page app — Axum serves HTML, components hydrate
  per-page

## Development

Requirements:

- [`rustup`](https://rustup.rs/) (toolchain pinned via `rust-toolchain.toml`)
- [`nvm`](https://github.com/nvm-sh/nvm) (Node version pinned via `.nvmrc`)
- [`tilt`](https://tilt.dev/) for the dev environment
- `yt-dlp` on `PATH`

```bash
nvm use
cd frontend && npm install && cd ..
tilt up
```

App runs at <http://localhost:3000>.

## Deployment

```bash
docker run -p 3000:3000 -v hometube-data:/app/data \
  ghcr.io/jimsimon/hometube:latest
```

Then open <http://localhost:3000> and follow the setup wizard.

See [`docs/deployment.md`](docs/deployment.md) for a full deploy
walkthrough, backups, and reverse-proxy notes, and
[`docs/google-cloud-setup.md`](docs/google-cloud-setup.md) for the
step-by-step on creating the Google Cloud project that the setup
wizard asks for.

## License

[AGPL-3.0](LICENSE) — self-hostable, modifications must remain open source,
including for network-accessible deployments.
