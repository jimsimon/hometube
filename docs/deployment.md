# Deploying HomeTube

HomeTube ships as a single Docker image. It's zero-config: every
runtime setting (Google credentials, parent accounts, allowlists) is
collected via the in-app setup wizard the first time you open the app.

## One-command deploy

```bash
docker run -p 3000:3000 -v hometube-data:/app/data \
  ghcr.io/jimsimon/hometube:latest
```

That's it. Then open <http://localhost:3000>.

The named volume (`hometube-data`) holds the SQLite database, the
on-disk segment cache, and any updated yt-dlp binary. Keep that volume
around between restarts and you keep all of your family's data.

## Compose

A reference Compose file lives at [`docker/docker-compose.yml`](../docker/docker-compose.yml):

```bash
cd docker && docker compose up -d
```

It uses the same named volume and exposes port 3000.

## First-run walkthrough

1. Visit <http://localhost:3000>. The app redirects you to `/setup`.
2. Step 1 — **Welcome.** Click "Begin".
3. Step 2 — **Google Cloud credentials.** Paste in your OAuth client ID
   + secret + YouTube Data API key. The wizard auto-fills the redirect
   URI based on the host you're connecting from. See
   [Google Cloud project setup](google-cloud-setup.md) for how to get
   those values.
4. Step 3 — **Sign in with Google.** The wizard opens Google's consent
   screen and brings you back. The first signed-in account is the
   first parent.
5. Step 4 — **Set a parent PIN.** Pick 4–6 numeric digits. You'll be
   asked for this every time you switch into your parent profile from
   the picker, but never for browsing on your own profile.
6. Step 5 — **Add family members (optional).** You can add more
   parents or children now or skip ahead and add them later from the
   parent dashboard's *Family* tab.
7. Step 6 — **Done.** You're in. The app drops you on `/parent/home`.

After setup, every visit to `/` lands on the profile picker (Netflix
style) until someone picks a profile. Children switch with one tap;
parents enter their PIN.

## Health checks

The image declares a `HEALTHCHECK` that hits `/api/health`. The route
runs a `SELECT 1` against SQLite before returning `ok`, so a healthy
container is one that can serve HTTP and read its database.

## Backups

HomeTube's entire state lives in `/app/data` (SQLite database +
segment cache + yt-dlp binary). To back up:

```bash
# Stop the container first so SQLite isn't being written.
docker stop hometube

# Copy the volume contents somewhere safe.
docker run --rm -v hometube-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/hometube-$(date +%F).tar.gz -C /data .

# Restart.
docker start hometube
```

Restoring is the inverse — stop the container, untar into the volume,
start it again.

If you only care about the database (and don't mind re-warming the
segment cache), you can copy `/app/data/app.db` directly with
`docker cp`.

## Updating

```bash
docker pull ghcr.io/jimsimon/hometube:latest
docker stop hometube && docker rm hometube
docker run -d --name hometube -p 3000:3000 \
  -v hometube-data:/app/data ghcr.io/jimsimon/hometube:latest
```

Migrations run automatically on startup.

## Reverse proxies / HTTPS

HomeTube speaks plain HTTP on port 3000 inside the container. Put it
behind nginx, Caddy, Traefik, or your reverse proxy of choice and let
that proxy terminate TLS. Make sure the proxy passes the `Host` header
through unchanged — the setup wizard uses `Host` to suggest a default
OAuth redirect URI.

## Resource sizing

A family of three browsing 1080p with the 50 GB segment cache uses on
the order of 1 GB of RAM under load and a few GB of CPU-seconds per
hour. SQLite plus the cache eats most of the disk; size the volume to
your `cache_max_size` plus a few hundred MB of headroom.
