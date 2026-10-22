#!/usr/bin/env bash
# Prints the Android versionCode for a LADEX version. Android only installs an update with a higher code, so each
# pre-release must rank above the one before: 3.1.0-alpha.2 -> 30100102, 3.1.0-rc.1 -> 30100301, 3.1.0 -> 30100900.
set -euo pipefail

version="${1:?usage: android-version-code.sh <version>}"
if [[ ! "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(-(alpha|beta|rc)\.([0-9]+))?$ ]]; then
  echo "android-version-code.sh: $version is not MAJOR.MINOR.PATCH with an optional -alpha.N, -beta.N or -rc.N" >&2
  exit 1
fi
major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" patch="${BASH_REMATCH[3]}"
stage="${BASH_REMATCH[5]}" stage_number="${BASH_REMATCH[6]}"

# Each part gets two digits, so larger numbers would collide with the next part.
for part in "$minor" "$patch" "${stage_number:-0}"; do
  if (( 10#$part > 99 )); then
    echo "android-version-code.sh: $version has a part above 99, which the version code cannot hold" >&2
    exit 1
  fi
done

case "$stage" in
  alpha) stage_code=$(( 100 + 10#$stage_number )) ;;
  beta) stage_code=$(( 200 + 10#$stage_number )) ;;
  rc) stage_code=$(( 300 + 10#$stage_number )) ;;
  *) stage_code=900 ;;
esac

echo $(( 10#$major * 10000000 + 10#$minor * 100000 + 10#$patch * 1000 + stage_code ))
