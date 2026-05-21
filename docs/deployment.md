# Deploying HomeTube

HomeTube runs as a three-container Docker Compose stack:

| Container | Image | Purpose |
|---|---|---|
| `app` | `ghcr.io/jimsimon/hometube` | Rust backend + Lit/Vite frontend |
| `discovery` | `ghcr.io/jimsimon/hometube-discovery` | youtubei.js sidecar for search + metadata |
| `pot-server` | `brainicism/bgutil-ytdlp-pot-provider` | Proof-of-Origin tokens for yt-dlp |

The app needs both sidecars at runtime — the discovery service replaces the YouTube Data API for search/channel lookups, and the PO-token server lets yt-dlp bypass YouTube's bot detection. None of the three are optional.

After first boot, all configuration (parent accounts, child profiles, allowlists) is collected through an in-app setup wizard. There is no config file to edit.

## Compose deploy

The reference stack is [`docker/docker-compose.yml`](../docker/docker-compose.yml).

```bash
HOMETUBE_DATABASE=/srv/hometube/database \
HOMETUBE_TOOLS=/srv/hometube/tools \
HOMETUBE_CACHE=/srv/hometube/cache \
  docker compose -f docker/docker-compose.yml up -d
```

Persistent state is split across three independent host paths so each can live on different storage:

| Variable | Container path | Contents | Sizing / placement |
|---|---|---|---|
| `HOMETUBE_DATABASE` | `/app/data/database` | SQLite (`app.db` + WAL/SHM) | Small, fsync-heavy. Put on SSD/NVMe. |
| `HOMETUBE_TOOLS` | `/app/data/tools` | `yt-dlp` binary, `cookies.txt` | Small, latency-sensitive. Put on SSD. |
| `HOMETUBE_CACHE` | `/app/data/cache` | DASH segment cache | Large, regeneratable. Fine on spinning disk or NAS. |

The app never reads across the three — they can sit on entirely separate filesystems. If you don't care about the split, point all three at subdirectories of a single dataset.

Everything else (image registry, version, host port, runtime UID/GID) has a sensible default; see the comment header at the top of the compose file for the full list.

Open the app at <http://localhost:30000> once the containers report healthy.

## TrueNAS Scale

The compose file is designed to paste into a TrueNAS Scale (ElectricEel 24.10+) **Custom App**.

1. Create datasets for persistent state. A typical split takes advantage of two pools: `tank/ssd/hometube/database` and `tank/ssd/hometube/tools` on a flash pool, plus `tank/bulk/hometube/cache` on a spinning-rust pool. `chown` each to `568:568` (the TrueNAS `apps` user). If you only have one pool, three sibling datasets under it work fine too.
2. **Apps → Discover Apps → Custom App** → paste the compose file.
3. Set `HOMETUBE_DATABASE`, `HOMETUBE_TOOLS`, and `HOMETUBE_CACHE` to the dataset paths you created. Override `HOMETUBE_PORT` if 30000 is taken on your box.
4. Deploy. The app reports healthy via `/api/health`; both sidecars have their own healthchecks gating the app's startup.

Snapshots, replication, and backup happen at the ZFS layer per dataset — no application-aware backup hook needed.

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

The app's persistent state lives across the three host paths you mapped (`HOMETUBE_DATABASE`, `HOMETUBE_TOOLS`, `HOMETUBE_CACHE`). The two sidecars are stateless — destroy and recreate them freely.

Backup priorities differ by tier:

- **`HOMETUBE_DATABASE`** — irreplaceable. Back this up.
- **`HOMETUBE_TOOLS`** — small and useful but reconstructable: yt-dlp self-updates on a cron, and `cookies.txt` can be re-uploaded through the app. Worth including but not critical.
- **`HOMETUBE_CACHE`** — entirely regeneratable on next play. Skip it.

Filesystem-level snapshots are the cleanest approach:

```bash
docker compose -f docker/docker-compose.yml stop app
tar czf hometube-db-$(date +%F).tar.gz -C "$HOMETUBE_DATABASE" .
tar czf hometube-tools-$(date +%F).tar.gz -C "$HOMETUBE_TOOLS" .
docker compose -f docker/docker-compose.yml start app
```

On TrueNAS, snapshot the relevant datasets — atomic, instant, and replication-friendly:

```bash
zfs snapshot tank/ssd/hometube/database@$(date +%F)
zfs snapshot tank/ssd/hometube/tools@$(date +%F)
```

If you only care about the SQLite database, `$HOMETUBE_DATABASE/app.db` is the only file you need to copy.

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
- **Data corruption or a migration you can't unwind.** Restore from a backup snapshot of `$HOMETUBE_DATABASE`.

This is the same reason ZFS snapshots before upgrades are worth the discipline — they give you instant atomic rollback for free.

