#!/usr/bin/env bash
# Installs an APK on the running emulator, opens it, and checks that LADEX starts and keeps running.
# Leaves its log, the system's errors and a screenshot of the first screen in the output folder.
set -euo pipefail

apk="${1:?usage: android-smoke-test.sh <apk> <output folder>}"
out="${2:?usage: android-smoke-test.sh <apk> <output folder>}"
package=io.github.gshreekar.ladex
# A cold emulator is slow; the app gets this long to log that it started, then this long more to settle.
start_attempts=30
settle_seconds=15

mkdir -p "$out"
adb install -r "$apk"
adb logcat -c
adb shell am start -W -n "$package/.MainActivity"

for _ in $(seq "$start_attempts"); do
  adb logcat -d -s LADEX | grep -q "starting" && break
  sleep 2
done
sleep "$settle_seconds"

adb logcat -d -s LADEX > "$out/ladex.log"
adb logcat -d '*:E' > "$out/errors.log"
adb logcat -d > "$out/logcat-at-start.log"
adb exec-out screencap -p > "$out/first-screen.png"

if ! grep -q "LADEX .* starting" "$out/ladex.log"; then
  echo "::error::LADEX never logged that it started"
  tail -n 60 "$out/errors.log"
  exit 1
fi
if ! adb shell pidof "$package" > /dev/null; then
  echo "::error::LADEX started but is no longer running"
  tail -n 60 "$out/errors.log"
  exit 1
fi
cat "$out/ladex.log"
echo "LADEX started and is still running"
