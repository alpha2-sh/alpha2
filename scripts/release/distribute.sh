#!/usr/bin/env bash
# Produces a target's release files (archive and wheels pinning its SHA-256) in
# target/alpha2-dist/<target>/. Must run on the target's own platform.
#   distribute.sh <target> <alpha2-VERSION-TARGET-bundled.tar.gz|.zip>
set -euo pipefail

usage="usage: distribute.sh <target> <alpha2-VERSION-TARGET-bundled.tar.gz|.zip>"
target=${1:?$usage}
archive=${2:?$usage}
workspace=$(cd "$(dirname "$0")/../.." && pwd)

name=$(basename "$archive")
case "$name" in
  alpha2-*-"$target"-bundled.tar.gz | alpha2-*-"$target"-bundled.zip) ;;
  *) echo "$name is not a bundled $target archive; $usage" >&2; exit 2 ;;
esac
version=${name#alpha2-}
version=${version%-"$target"-bundled.*}

out="$workspace/target/alpha2-dist/$target"
rm -rf "$out"
mkdir -p "$out/wheels"
published="$out/$name"
cp "$archive" "$published"

# Anything that changes the archive's bytes, such as signing, must happen before this point.
if command -v sha256sum > /dev/null; then
  sha256=$(sha256sum < "$published" | cut -d ' ' -f 1)
else
  sha256=$(shasum -a 256 < "$published" | cut -d ' ' -f 1)
fi
echo "$name $sha256"

pins="$workspace/python/alpha2/src/alpha2/_host_archives.json"
check=$(mktemp -d)
trap 'rm -f "$pins"; rm -rf "$check"' EXIT
printf '{"version": "%s", "archives": {"%s": "%s"}}\n' "$version" "$target" "$sha256" > "$pins"

"$workspace/scripts/release/build-wheels.sh" "$published" "$out/wheels" "$check/venv"

python="$check/venv/bin/python"
[ -d "$check/venv/Scripts" ] && python="$check/venv/Scripts/python"
(cd "$check" && "$python" -c '
import sys
from alpha2.install import _pinned_archive_sha256
version, target, sha256 = sys.argv[1:]
assert _pinned_archive_sha256(version, target) == sha256, "the alpha2 wheel does not pin the archive"
' "$version" "$target" "$sha256")
echo "distributed $target in $out"
