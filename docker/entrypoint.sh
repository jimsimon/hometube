#!/bin/sh
set -eu

BUNDLED_YTDLP=/usr/local/lib/hometube/yt-dlp
DEFAULT_YTDLP_PATH=/app/data/tools/yt-dlp
YTDLP_TARGET=${YTDLP_PATH:-$DEFAULT_YTDLP_PATH}

# A bind mount hides files baked into /app/data/tools, so seed the default
# target at container startup instead of putting the bootstrap binary there
# during the image build. The copy runs as the same unprivileged user as
# HomeTube, leaving both the file and its directory writable by the updater.
if [ "$YTDLP_TARGET" = "$DEFAULT_YTDLP_PATH" ]; then
    if [ ! -e "$YTDLP_TARGET" ]; then
        mkdir -p "${YTDLP_TARGET%/*}"
        TEMP_YTDLP="${YTDLP_TARGET}.bootstrap.$$"
        trap 'rm -f "$TEMP_YTDLP"' 0 HUP INT TERM
        cp "$BUNDLED_YTDLP" "$TEMP_YTDLP"
        chmod 0755 "$TEMP_YTDLP"
        mv "$TEMP_YTDLP" "$YTDLP_TARGET"
        trap - 0 HUP INT TERM
    elif [ ! -x "$YTDLP_TARGET" ]; then
        echo "HomeTube: $YTDLP_TARGET exists but is not executable" >&2
        exit 1
    fi
fi

exec /usr/local/bin/hometube "$@"
