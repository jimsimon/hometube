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
FROM rust:1.95-slim AS backend
WORKDIR /app

# Build dependencies for sqlx + reqwest.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy only the manifest first to take advantage of layer caching for
# the dependency build; then copy sources.
COPY Cargo.toml Cargo.lock* ./
COPY src/ ./src/
COPY templates/ ./templates/
COPY migrations/ ./migrations/

RUN cargo build --release \
    && strip target/release/hometube || true

# ---------------------------------------------------------------------------
# Stage 3: runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# Runtime dependencies:
#   - ca-certificates: for HTTPS to Google + googlevideo.com
#   - curl          : healthcheck + yt-dlp download below
#   - python3       : yt-dlp's runtime requirement (single-binary works
#                     in many cases but a Python interpreter is the
#                     supported fallback)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl python3 unzip \
    && rm -rf /var/lib/apt/lists/*

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
RUN PLUGIN_VERSION=$(curl -fsSL "https://api.github.com/repos/Brainicism/bgutil-ytdlp-pot-provider/releases/latest" \
        | grep -o '"tag_name":"[^"]*"' | head -1 | cut -d'"' -f4) \
    && curl -fsSL \
        "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/${PLUGIN_VERSION}/bgutil-ytdlp-pot-provider.zip" \
        -o /tmp/pot-plugin.zip \
    && mkdir -p /usr/local/share/yt-dlp-plugins \
    && unzip -o /tmp/pot-plugin.zip -d /usr/local/share/yt-dlp-plugins/ \
    && rm /tmp/pot-plugin.zip

WORKDIR /app
EXPOSE 3000
VOLUME ["/app/data"]

ENV STATIC_DIR=/app/static \
    YTDLP_PATH=/usr/local/bin/yt-dlp \
    YTDLP_PLUGIN_DIR=/usr/local/share/yt-dlp-plugins \
    POT_SERVER_URL=http://pot-server:4416 \
    DATABASE_PATH=/app/data/app.db

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://localhost:3000/api/health || exit 1

ENTRYPOINT ["hometube"]
