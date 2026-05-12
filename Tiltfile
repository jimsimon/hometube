# HomeTube Tilt configuration.
#
# Runs:
#   - cargo-watch on the Rust backend
#   - Vite in watch mode for the frontend
#   - sqlx migrate when the migrations/ directory changes
#   - a sanity check for yt-dlp on PATH

config.define_string_list("to-run", args=True)
cfg = config.parse()

# Backend: rebuild and restart on Rust source or template changes.
local_resource(
    'backend',
    serve_cmd='cargo watch -w src -w templates -w migrations -x run',
    serve_env={
        'POT_SERVER_URL': 'http://127.0.0.1:4416',
    },
    deps=['src/', 'templates/', 'Cargo.toml', 'migrations/'],
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
# port 4416 (same as docker-compose.yml).
local_resource(
    'pot-server',
    serve_cmd='docker run --rm -p 4416:4416 brainicism/bgutil-ytdlp-pot-provider:latest',
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
