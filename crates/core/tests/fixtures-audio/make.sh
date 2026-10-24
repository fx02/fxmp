#!/bin/sh
# Extra decoder/player fixtures (kept out of tests/fixtures, which tests count).
set -e
cd "$(dirname "$0")"
FF=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
$FF -y -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 -c:a flac mono.flac
