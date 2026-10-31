#!/usr/bin/env bash
# Writes the release pins into the @alpha2-sh/node tarball: dist/host-archives.json maps each
# release target to the SHA-256 of its bundled archive in <archives-dir> (as distribute.sh left
# them), and the package is packed again into <out-dir> under its own name.
#   pin-node.sh <alpha2-sh-node-VERSION.tgz> <archives-dir> <out-dir>
# Every target's archive must be there: an installer on a platform left out would fall back to the
# checksums.sha256 a mirror serves.
set -euo pipefail

usage="usage: pin-node.sh <alpha2-sh-node-VERSION.tgz> <archives-dir> <out-dir>"
tarball=${1:?$usage}
archives=${2:?$usage}
out=${3:?$usage}
mkdir -p "$out"
out=$(cd "$out" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar -xzf "$tarball" -C "$work"
[ -d "$work/package/dist" ] || { echo "$tarball has no package/dist" >&2; exit 1; }

node - "$work/package" "$archives" <<'EOF'
const { createHash } = require("node:crypto");
const { readFileSync, writeFileSync } = require("node:fs");
const { join } = require("node:path");

const [packageDirectory, archives] = process.argv.slice(2);
const { version } = JSON.parse(readFileSync(join(packageDirectory, "package.json"), "utf8"));
const pins = { version, archives: {} };
for (const [target, extension] of [
  ["linux-x64", "tar.gz"],
  ["linux-arm64", "tar.gz"],
  ["macos-arm64", "tar.gz"],
  ["win-x64", "zip"],
]) {
  const name = `alpha2-${version}-${target}-bundled.${extension}`;
  const bytes = readFileSync(join(archives, name));
  pins.archives[target] = createHash("sha256").update(bytes).digest("hex");
  console.log(`${name} ${pins.archives[target]}`);
}
writeFileSync(
  join(packageDirectory, "dist", "host-archives.json"),
  `${JSON.stringify(pins, null, 2)}\n`,
);
EOF

(cd "$work/package" && npm pack --ignore-scripts --pack-destination "$out")
pinned="$out/$(basename "$tarball")"
tar -tzf "$pinned" | grep -x package/dist/host-archives.json > /dev/null ||
  { echo "$pinned does not hold package/dist/host-archives.json" >&2; exit 1; }
echo "pinned $pinned"
