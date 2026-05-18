# Deploying HomeTube

HomeTube runs as a three-container Docker Compose stack:

| Container | Image | Purpose |
|---|---|---|
| `app` | `ghcr.io/jimsimon/hometube` | Rust backend + Lit/Vite frontend |
| `discovery` | `ghcr.io/jimsimon/hometube-discovery` | youtubei.js sidecar for search + metadata |
| `pot-server` | `brainicism/bgutil-ytdlp-pot-provider` | Proof-of-Origin tokens for yt-dlp |

The app needs both sidecars at runtime — the discovery service replaces the YouTube Data API for search/channel/playlist lookups, and the PO-token server lets yt-dlp bypass YouTube's bot detection. None of the three are optional.

After first boot, all configuration (parent accounts, child profiles, allowlists) is collected through an in-app setup wizard. There is no config file to edit.

## Compose deploy

The reference stack is [`docker/docker-compose.yml`](../docker/docker-compose.yml).

```bash
HOMETUBE_DATA=/srv/hometube docker compose -f docker/docker-compose.yml up -d
```

`HOMETUBE_DATA` is the only required variable — it's the host path that backs the app's SQLite database, segment cache, and updated yt-dlp binary. Everything else (image registry, version, host port, runtime UID/GID) has a sensible default that you can override; see the comment header at the top of the compose file for the full list.

Open the app at <http://localhost:30000> once the containers report healthy.

## TrueNAS Scale

The compose file is designed to paste into a TrueNAS Scale (ElectricEel 24.10+) **Custom App**.

1. Create a dataset for persistent state, e.g. `tank/apps/hometube/data`, and `chown` it to `568:568` (the TrueNAS `apps` user).
2. **Apps → Discover Apps → Custom App** → paste the compose file.
3. Set `HOMETUBE_DATA` to the dataset path you created. Override `HOMETUBE_PORT` if 30000 is taken on your box.
4. Deploy. The app reports healthy via `/api/health`; both sidecars have their own healthchecks gating the app's startup.

Snapshots, replication, and backup happen at the ZFS layer on the data dataset — no application-aware backup hook needed.

## First-run walkthrough

1. Open the app. The wizard redirects you to `/setup`.
2. **Welcome.** Click "Begin".
3. **Create parent account.** Pick a username and a 4–6 digit PIN. This account becomes the first parent.
4. **Invite family (optional).** Add more parents or children now, or skip and add them later from the parent dashboard's *Family* tab.
5. **Done.** You land on `/parent/home`.

After setup, every visit to `/` lands on a profile picker. Children switch with one tap; parents enter their PIN.

## Health checks

All three containers declare healthchecks:

- `app` — `GET /api/health`, which runs `SELECT 1` against SQLite.
- `discovery` — `GET /health` on the sidecar's internal HTTP server.
- `pot-server` — `GET /ping` on the bgutil service.

`app` will not start until `discovery` and `pot-server` both report healthy, so a healthy `app` container implies the whole stack is up.

## Backups

The entire app state lives under your `HOMETUBE_DATA` path on the host. The two sidecars are stateless — destroy and recreate them freely.

The cleanest backup is filesystem-level: snapshot or `tar` the data path while the app container is stopped.

```bash
docker compose -f docker/docker-compose.yml stop app
tar czf hometube-$(date +%F).tar.gz -C "$HOMETUBE_DATA" .
docker compose -f docker/docker-compose.yml start app
```

On TrueNAS, replace the tar step with a ZFS snapshot — atomic, instant, and replication-friendly:

```bash
zfs snapshot tank/apps/hometube/data@$(date +%F)
```

If you only care about the SQLite database and don't mind re-warming the segment cache on next play, `$HOMETUBE_DATA/app.db` is the only file you need to copy.

## Updating

```bash
docker compose -f docker/docker-compose.yml pull
docker compose -f docker/docker-compose.yml up -d
```

Database migrations run automatically on app startup. Take a snapshot first if you want a clean rollback path — see [Migrations and rollback](#migrations-and-rollback) below.

## Migrations and rollback

Migrations are embedded into the backend binary via `sqlx::migrate!` and run on every startup. They are **forward-only by convention** — there are no `.down.sql` files, and `sqlx`'s embedded runner has no `revert()` API.

If a migration breaks something, the recovery path is:

- **Schema problem you can fix forward.** Write a new migration that corrects it, deploy.
- **Data corruption or a migration you can't unwind.** Restore from a backup snapshot of `$HOMETUBE_DATA`.

This is the same reason ZFS snapshots before upgrades are worth the discipline — they give you instant atomic rollback for free.

## Reverse proxies / HTTPS

The app speaks plain HTTP on its container port. Put it behind nginx, Caddy, Traefik, or your reverse proxy of choice and let that proxy terminate TLS. Pass the `Host` header through unchanged.

## Resource sizing

A family of three browsing 1080p with the 50 GB segment cache uses around 1 GB of RAM under load and a few GB of CPU-seconds per hour. SQLite plus the cache dominates disk usage — size the data dataset to your `cache_max_size` plus a few hundred MB of headroom.

The sidecars are tiny: ~50 MB RAM each at idle, more under search load.
