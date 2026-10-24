#!/bin/sh
# Extra decoder/player fixtures (kept out of tests/fixtures, which tests count).
set -e
cd "$(dirname "$0")"
FF=${FFMPEG:-/opt/homebrew/bin/ffmpeg}
$FF -y -f lavfi -i "sine=frequency=440:duration=1:sample_rate=44100" -ac 1 -c:a flac mono.flac
# Player fixtures: 20 s mono tones, one distinct frequency each (identified by Goertzel).
for f in 500 1500 2500 3500 4500 5500 6500; do
  $FF -y -f lavfi -i "sine=frequency=$f:duration=20:sample_rate=16000" -ac 1 -c:a flac -compression_level 12 "tone$f.flac"
done
