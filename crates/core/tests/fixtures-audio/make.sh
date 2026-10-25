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
# Seek fixtures: 6 s, 500 Hz for the first 3 s then 1500 Hz (the switch locates the position).
STEP="sine=frequency=500:duration=3:sample_rate=22050[a];sine=frequency=1500:duration=3:sample_rate=22050[b];[a][b]concat=n=2:v=0:a=1"
$FF -y -filter_complex "$STEP" -ac 1 -c:a flac step.flac
$FF -y -filter_complex "$STEP" -ac 1 -c:a libmp3lame -b:a 48k step.mp3
# The native (experimental) Vorbis encoder only does stereo.
$FF -y -filter_complex "$STEP" -ac 2 -c:a vorbis -strict -2 -b:a 64k step.ogg
