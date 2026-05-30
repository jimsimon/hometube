# syntax=docker/dockerfile:1.24
#
# HomeTube — multi-stage Dockerfile.
#
# Stage 1 (frontend): build the Vite/Lit ES modules into static assets
# Stage 2 (backend) : compile the Rust binary (askama templates compile
#                     into the binary at build time)
# Stage 3 (runtime) : minimal Debian image with the binary + assets +
#                     yt-dlp pre-installed.

# ---------------------------------------------------------------------------
# Stage 1: frontend assets
# ---------------------------------------------------------------------------
FROM node:24-alpine AS frontend
WORKDIR /app/frontend

# Copy package manifests first so Docker can cache the npm install layer.
COPY frontend/package.json frontend/package-lock.json* ./
RUN npm ci || npm install

# Copy the rest of the frontend tree and build.
COPY frontend/ ./
RUN npm run build

# ---------------------------------------------------------------------------
# Stage 2: Rust backend
# ---------------------------------------------------------------------------
FROM rust:1.96-slim-bookworm AS backend
WORKDIR /app

# Build dependencies for sqlx + reqwest.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy only the manifest first to take advantage of layer caching for
# the dependency build; then copy sources.
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
COPY templates/ ./templates/
COPY migrations/ ./migrations/

RUN cargo build --release --locked \
    && strip target/release/hometube || true

# ---------------------------------------------------------------------------
# Stage 3: runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Runtime dependencies:
#   - ca-certificates: for HTTPS to Google + googlevideo.com
#   - curl          : healthcheck + yt-dlp + deno downloads below
#   - python3       : yt-dlp's runtime requirement (single-binary works
#                     in many cases but a Python interpreter is the
#                     supported fallback)
#   - unzip         : extracting the bgutil PO token plugin + deno zip
#
# Note: bookworm ships nodejs 18.x, which is below yt-dlp's minimum of
# Node 20 for the `[jsc]` challenge framework. We install Deno instead
# (yt-dlp's recommended runtime, enabled by default in the extractor).
# See https://github.com/yt-dlp/yt-dlp/wiki/EJS for the full matrix of
# supported runtimes and minimum versions.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl python3 unzip \
    && rm -rf /var/lib/apt/lists/*

# Install Deno — yt-dlp's recommended JS runtime for solving YouTube's
# n-parameter and signature challenges (via the bundled yt-dlp-ejs
# component). Without a supported runtime, yt-dlp prunes every
# signature-ciphered DASH format and bails out with
#   "Requested format is not available".
#
# We grab the pre-built `deno` binary directly from the official
# release zip (no `denort` — the slim runtime variant won't suffice).
# Pinned via DENO_VERSION below so image rebuilds are deterministic; bump
# this with conscious intent and verify against
# https://github.com/denoland/deno/releases.
ARG DENO_VERSION=v2.5.4
RUN ARCH="$(dpkg --print-architecture)" \
    && case "${ARCH}" in \
         amd64) DENO_TARGET="x86_64-unknown-linux-gnu" ;; \
         arm64) DENO_TARGET="aarch64-unknown-linux-gnu" ;; \
         *) echo "Unsupported arch for Deno: ${ARCH}" >&2; exit 1 ;; \
       esac \
    && curl -fsSL \
        "https://github.com/denoland/deno/releases/download/${DENO_VERSION}/deno-${DENO_TARGET}.zip" \
        -o /tmp/deno.zip \
    && unzip -o /tmp/deno.zip -d /usr/local/bin/ \
    && chmod +x /usr/local/bin/deno \
    && rm /tmp/deno.zip \
    && deno --version

# Copy the built binary and static assets.
COPY --from=backend /app/target/release/hometube /usr/local/bin/hometube
COPY --from=frontend /app/frontend/dist /app/static
COPY migrations/ /app/migrations/

# Download the latest yt-dlp release. The cron `ytdlp_update` job
# refreshes this on schedule once the app is running.
RUN curl -fsSL \
        https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
        -o /usr/local/bin/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp

# Install the bgutil PO token provider plugin for yt-dlp. This plugin
# automatically fetches Proof-of-Origin tokens from the pot-server
# sidecar container to bypass YouTube's bot detection.
#
# yt-dlp's plugin loader scans `<plugin-dir>/<package>/yt_dlp_plugins/`
# — without the package-wrapper subdirectory the loader silently
# ignores the plugin. The release zip lays out the inner tree (`yt_dlp_plugins/`)
# so we extract it under a `bgutil/` wrapper.
RUN PLUGIN_VERSION=$(curl -fsSL "https://api.github.com/repos/Brainicism/bgutil-ytdlp-pot-provider/releases/latest" \
        | grep -oE '"tag_name":[[:space:]]*"[^"]*"' | head -1 | sed -E 's/.*"([^"]+)"$/\1/') \
    && curl -fsSL \
        "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/${PLUGIN_VERSION}/bgutil-ytdlp-pot-provider.zip" \
        -o /tmp/pot-plugin.zip \
    && mkdir -p /usr/local/share/yt-dlp-plugins/bgutil \
    && unzip -o /tmp/pot-plugin.zip -d /usr/local/share/yt-dlp-plugins/bgutil/ \
    && rm /tmp/pot-plugin.zip

# Create a non-root user (UID/GID 568 matches the TrueNAS Scale `apps`
# user, but works on any host). The data directory is created and
# chown'd so the container can write to a freshly-mounted empty volume
# without manual setup.
RUN groupadd --system --gid 568 hometube \
    && useradd --system --uid 568 --gid 568 --home /app --shell /usr/sbin/nologin hometube \
    && mkdir -p /app/data/database /app/data/tools /app/data/cache \
    && chown -R 568:568 /app

WORKDIR /app
USER 568:568
EXPOSE 3000
# Three independent volumes so operators can place each on different
# storage tiers (e.g. database + tools on SSD, cache on spinning disk
# or a NAS). Each path is self-contained — the app never reads across
# them, so they can live on entirely separate filesystems.
VOLUME ["/app/data/database", "/app/data/tools", "/app/data/cache"]

ENV STATIC_DIR=/app/static \
    YTDLP_PATH=/usr/local/bin/yt-dlp \
    YTDLP_PLUGIN_DIR=/usr/local/share/yt-dlp-plugins \
    YTDLP_COOKIES_PATH=/app/data/tools/cookies.txt \
    POT_SERVER_URL=http://pot-server:4416 \
    DISCOVERY_SIDECAR_URL=http://discovery:3000 \
    DATABASE_PATH=/app/data/database/app.db \
    CACHE_DIR=/app/data/cache

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:3000/api/health || exit 1

ENTRYPOINT ["hometube"]
