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
    dir='frontend',
    labels=['client'],
)

# Verify yt-dlp is available so missing-binary failures surface
# immediately rather than at first video play.
local_resource(
    'ytdlp-check',
    cmd='yt-dlp --version || (echo "WARNING: yt-dlp not found on PATH" && exit 0)',
    labels=['deps'],
)
