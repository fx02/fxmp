#!/usr/bin/env bash
# Regenerates the test fixtures: 1 s 440 Hz stereo tone at 44.1 kHz with tags.
set -euo pipefail
cd "$(dirname "$0")"
FFMPEG="${FFMPEG:-ffmpeg}"
command -v "$FFMPEG" >/dev/null || FFMPEG=/opt/homebrew/bin/ffmpeg
SRC="sine=frequency=440:duration=1:sample_rate=44100"
TAGS=(-metadata artist="Test Artist" -metadata title="Test Title" -metadata genre="Trance; Chillout")
gen() { "$FFMPEG" -hide_banner -loglevel error -y -f lavfi -i "$SRC" -ac 2 "$@"; }
gen "${TAGS[@]}" -c:a libmp3lame -b:a 64k -id3v2_version 3 tone.mp3
gen "${TAGS[@]}" -metadata REPLAYGAIN_TRACK_GAIN="-6.50 dB" -c:a flac tone.flac
# libvorbis is absent from this ffmpeg build; the native (experimental) encoder emits valid Vorbis.
gen "${TAGS[@]}" -c:a vorbis -strict -2 -b:a 64k tone.ogg
head -c 2048 /dev/urandom > broken.mp3
