#!/usr/bin/env bash
# Builds the Android APK (arm64 and armv7, Android 8+) and signs it.
#
#   scripts/build-android.sh [output.apk]
#
# Needs JDK 17, the Android SDK with NDK 27, and a signing key outside the
# repository: FXMP_ANDROID_KEYS (default ~/Documents/PVT/fxmp-keys) holds
# fxmp-release.jks (alias "fxmp") and password.txt. Keep that key: an update
# must be signed with the same one, or Android refuses to install it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
keys="${FXMP_ANDROID_KEYS:-$HOME/Documents/PVT/fxmp-keys}"
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@17}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
export NDK_HOME="${NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}"
export PATH="$JAVA_HOME/bin:$PATH"
build_tools="$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/src-tauri/Cargo.toml" | head -1)"
out="${1:-$root/target/fxmp_${version}_android.apk}"

for f in fxmp-release.jks password.txt; do
  [[ -f "$keys/$f" ]] || { echo "missing $keys/$f" >&2; exit 1; }
done

cd "$root"
cargo tauri android build --apk --target aarch64 --target armv7
unsigned="src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release-unsigned.apk"
aligned="$(mktemp -t fxmp-aligned).apk"
"$build_tools/zipalign" -f -p 4 "$unsigned" "$aligned"
"$build_tools/apksigner" sign --ks "$keys/fxmp-release.jks" --ks-key-alias fxmp \
  --ks-pass "file:$keys/password.txt" --out "$out" "$aligned"
rm -f "$aligned"
"$build_tools/apksigner" verify "$out"
echo "$out"
