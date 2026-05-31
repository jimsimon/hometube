# HomeTube Tilt configuration.
#
# Runs:
#   - cargo-watch on the Rust backend
#   - Vite in watch mode for the frontend
#   - sqlx migrate when the migrations/ directory changes
#   - a sanity check for yt-dlp on PATH
#   - one-shot install of the bgutil PO token plugin for yt-dlp

config.define_string_list("to-run", args=True)
cfg = config.parse()

# bgutil PO token plugin for yt-dlp. Without this plugin yt-dlp can't
# fetch Proof-of-Origin tokens from the pot-server sidecar and YouTube
# rejects player API requests with "Sign in to confirm you're not a
# bot" — even when full auth cookies are supplied. The plugin is
# installed automatically inside the production Docker image; in local
# dev we mirror that here. The download is skipped when the marker
# file already exists so subsequent `tilt up` invocations are no-ops.
local_resource(
    'ytdlp-pot-plugin',
    cmd='''
        set -e
        DEST="./yt-dlp-plugins"
        MARKER="$DEST/.installed"
        if [ -f "$MARKER" ]; then
            echo "bgutil plugin already installed at $DEST"
            exit 0
        fi
        VERSION=$(curl -fsSL "https://api.github.com/repos/Brainicism/bgutil-ytdlp-pot-provider/releases/latest" \\
            | grep -oE '"tag_name":[[:space:]]*"[^"]*"' | head -1 | sed -E 's/.*"([^"]+)"$/\\1/')
        # yt-dlp expects plugins in <plugin-dir>/<package>/yt_dlp_plugins/...
        # The release zip lays out the inner yt_dlp_plugins/ tree, so we
        # extract under a package-wrapper subdir (`bgutil/`) — without
        # this yt-dlp silently ignores the plugin and the verbose log
        # shows "Plugin directories: none".
        echo "Installing bgutil-ytdlp-pot-provider $VERSION into $DEST/bgutil"
        rm -rf "$DEST/bgutil"
        mkdir -p "$DEST/bgutil"
        TMP=$(mktemp)
        curl -fsSL "https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/$VERSION/bgutil-ytdlp-pot-provider.zip" -o "$TMP"
        unzip -o "$TMP" -d "$DEST/bgutil"
        rm "$TMP"
        echo "$VERSION" > "$MARKER"
    ''',
    labels=['deps'],
)

# Discovery sidecar: youtubei.js-powered search/metadata service.
local_resource(
    'discovery',
    serve_cmd='node server.js',
    serve_dir='sidecar/discovery',
    serve_env={'PORT': '3001'},
    deps=['sidecar/discovery/server.js', 'sidecar/discovery/package.json'],
    labels=['server'],
    readiness_probe=probe(
        period_secs=2,
        http_get=http_get_action(port=3001, path="/health"),
    ),
)

# Backend: rebuild and restart on Rust source or template changes.
local_resource(
    'backend',
    serve_cmd='cargo watch -w src -w templates -w migrations -x run',
    serve_env={
        'POT_SERVER_URL': 'http://127.0.0.1:4416',
        'DISCOVERY_SIDECAR_URL': 'http://127.0.0.1:3001',
        'YTDLP_COOKIES_PATH': './data/tools/cookies.txt',
        # Point yt-dlp at the locally-installed bgutil plugin. The
        # path matches the one created by the `ytdlp-pot-plugin`
        # resource above, relative to the project root (which is
        # cargo-watch's CWD).
        'YTDLP_PLUGIN_DIR': './yt-dlp-plugins',
    },
    deps=['src/', 'templates/', 'Cargo.toml', 'migrations/'],
    resource_deps=['ytdlp-pot-plugin', 'discovery'],
    labels=['server'],
    readiness_probe=probe(
        period_secs=2,
        http_get=http_get_action(port=3000, path="/api/health"),
    ),
)

# Frontend: rebuild component bundles on file save. The build output is
# served by the Rust backend from `frontend/dist/`.
local_resource(
    'frontend',
    serve_cmd='npm run dev',
    deps=['frontend/src/', 'frontend/vite.config.ts', 'frontend/tsconfig.json'],
    serve_dir='frontend',
    labels=['client'],
)

# PO token server: generates proof-of-origin tokens so yt-dlp can
# bypass YouTube's bot detection. Runs the bgutil Docker sidecar on
# port 4416 (same as the deployed compose stack).
local_resource(
    'pot-server',
    # `docker run` only attaches a client to a daemon-owned container, so
    # Tilt's teardown (SIGTERM then SIGKILL of this process) doesn't stop
    # the container — it orphans it, leaking port 4416 and blocking the
    # next `tilt up`. Three things keep restarts clean:
    #   - `docker rm -f` reaps any orphan from a prior run before we
    #     re-bind 4416 (this is what actually breaks the leak cycle).
    #   - `--name` makes that orphan deterministically removable.
    #   - `exec` hands Tilt's signal to the docker client (not the shell)
    #     and `--init` runs tini as PID 1 so the container stops promptly.
    serve_cmd='''
        docker rm -f hometube-pot-server >/dev/null 2>&1 || true
        exec docker run --rm --init --name hometube-pot-server \\
            -p 4416:4416 brainicism/bgutil-ytdlp-pot-provider:latest
    ''',
    labels=['deps'],
    readiness_probe=probe(
        period_secs=5,
        http_get=http_get_action(port=4416, path="/ping"),
    ),
)

# Verify yt-dlp is available so missing-binary failures surface
# immediately rather than at first video play.
local_resource(
    'ytdlp-check',
    cmd='yt-dlp --version || (echo "ERROR: yt-dlp not found on PATH – install it: https://github.com/yt-dlp/yt-dlp#installation" && exit 1)',
    labels=['deps'],
)
