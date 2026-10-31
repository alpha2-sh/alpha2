#!/usr/bin/env bash
# Install journeys in containers, from a packaging job's outputs: the linux-x64 bundled archive,
# the Python wheels and the `npm pack` tarball of @alpha2-sh/node, all in <artifacts-dir>. A local
# mirror serves the archive; each case runs in Docker with the host's network and `timeout 300`,
# and prints PASS, FAIL or SKIP with its name. Exits 1 when a case fails.
#   install-journeys.sh <artifacts-dir>
# ALPHA2_PUBLISHED_VERSION names the release whose checksums the no-CA case fetches from GitHub
# (default: the latest release).
set -uo pipefail

artifacts=$(cd "${1:?usage: install-journeys.sh <artifacts-dir>}" && pwd)
workspace=$(cd "$(dirname "$0")/../.." && pwd)
archive=$(ls "$artifacts"/alpha2-*-linux-x64-bundled.tar.gz 2>/dev/null | head -1)
tarball=$(ls "$artifacts"/alpha2-sh-node-*.tgz 2>/dev/null | head -1)
[ -n "$archive" ] || { echo "no linux-x64 bundled archive in $artifacts" >&2; exit 2; }
[ -n "$tarball" ] || { echo "no @alpha2-sh/node tarball in $artifacts" >&2; exit 2; }
version=$(basename "$archive" | sed 's/^alpha2-\(.*\)-linux-x64-bundled\.tar\.gz$/\1/')

work=$(mktemp -d)
mirror_pid=
cleanup() {
  [ -n "$mirror_pid" ] && kill "$mirror_pid" 2>/dev/null
  # Containers ran as root, so their files in the work directory are root's.
  docker run --rm -v "$work:/work" alpine:3 rm -rf /work/cases >/dev/null 2>&1
  rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/mirror/v$version" "$work/cases"
cp "$archive" "$work/mirror/v$version/"
(cd "$work/mirror/v$version" && sha256sum -- *.tar.gz > checksums.sha256)
node "$workspace/scripts/packaging/serve-mirror.mjs" "$work/mirror" --log "$work/mirror.log" \
  > "$work/mirror.url" &
mirror_pid=$!
for _ in $(seq 50); do [ -s "$work/mirror.url" ] && break; sleep 0.1; done
mirror=$(head -1 "$work/mirror.url")
[ -n "$mirror" ] || { echo "the mirror did not start" >&2; exit 2; }
echo "mirror $mirror serves alpha2 $version"

failed=0
# run_case <name> <image> <script> [docker run arguments]: the script runs under sh in the image,
# with the artifacts at /artifacts, the repository at /workspace (read-only) and MIRROR set.
run_case() {
  local name=$1 image=$2 script=$3
  shift 3
  docker pull -q "$image" > /dev/null
  echo "image $image $(docker image inspect --format '{{index .RepoDigests 0}}' "$image")"
  local log="$work/cases/$name.log"
  if timeout 300 docker run --rm --network host \
    -v "$artifacts:/artifacts:ro" -v "$workspace:/workspace:ro" -v "$work/cases:/cases" \
    -e MIRROR="$mirror" -e VERSION="$version" "$@" "$image" sh -c "$script" > "$log" 2>&1; then
    echo "PASS $name"
  else
    echo "FAIL $name"
    sed 's/^/  /' "$log"
    failed=1
  fi
}

node_client='mkdir -p /client && tar -xzf /artifacts/alpha2-sh-node-*.tgz -C /client'
cli=/client/package/dist/cli.js

# The bundled host installs as root, its files are root's, and it exports on the CPU renderer.
run_case node22-slim node:22-slim "set -e; $node_client
ALPHA2_HOST_CACHE_DIR=/cache ALPHA2_HOST_DOWNLOAD_URL=\$MIRROR node $cli install
host=/cache/alpha2-\$VERSION-linux-x64-bundled/bin/alpha2-host
test \"\$(stat -c %u:%g \$host)\" = 0:0
cd /workspace && \$host export --renderer cpu --project tests/cases/host-parity-complex-project.json \\
  --asset video=tests/corpus/parity-rgb.mov --asset still=tests/corpus/parity-image.png \\
  --out /tmp/out.mp4 --video-codec mpeg4-part2 > /tmp/result.json
grep -q '\"frames\"' /tmp/result.json"

# Through a forward proxy (an http: mirror goes through HTTP_PROXY), and a refused proxy names
# the cause and the remedy.
run_case node22-slim-proxy node:22-slim "set -e; $node_client
node /workspace/scripts/packaging/forward-proxy.mjs --log /tmp/proxy.log > /tmp/proxy.url &
while [ ! -s /tmp/proxy.url ]; do sleep 0.1; done
HTTP_PROXY=\$(head -1 /tmp/proxy.url) ALPHA2_HOST_CACHE_DIR=/cache ALPHA2_HOST_DOWNLOAD_URL=\$MIRROR \\
  node $cli install
grep -q \"GET \$MIRROR/v\$VERSION/alpha2-\$VERSION-linux-x64-bundled.tar.gz\" /tmp/proxy.log
if HTTPS_PROXY=http://127.0.0.1:9 ALPHA2_HOST_CACHE_DIR=/cache2 node $cli install 2> /tmp/err; then
  exit 1
fi
cat /tmp/err
grep -q ECONNREFUSED /tmp/err && grep -q HTTPS_PROXY /tmp/err && grep -q ALPHA2_HOST_DOWNLOAD_URL /tmp/err"

# musl is refused before anything is downloaded.
before=$(wc -l < "$work/mirror.log" 2>/dev/null || echo 0)
run_case alpine node:22-alpine "$node_client
if ALPHA2_HOST_CACHE_DIR=/cache ALPHA2_HOST_DOWNLOAD_URL=\$MIRROR node $cli install 2> /tmp/err; then
  exit 1
fi
cat /tmp/err
grep -q \"this system's C library is musl (Alpine and other musl distributions are not supported)\" /tmp/err"
after=$(wc -l < "$work/mirror.log" 2>/dev/null || echo 0)
if [ "$before" != "$after" ]; then
  echo "FAIL alpine-no-download (the mirror saw requests)"
  failed=1
fi

# glibc 2.34: pip refuses the manylinux_2_35 host wheel, and the client refuses naming the floor.
run_case almalinux9 almalinux:9 "set -e
dnf install -y -q python3.11 python3.11-pip > /dev/null
python3.11 -m venv /v
if /v/bin/pip install -q --no-index --find-links /artifacts alpha2 2> /tmp/pip; then exit 1; fi
/v/bin/pip install -q --no-index --find-links /artifacts --no-deps alpha2
if ALPHA2_HOST_CACHE_DIR=/cache ALPHA2_HOST_DOWNLOAD_URL=\$MIRROR /v/bin/python -m alpha2 install 2> /tmp/err; then
  exit 1
fi
cat /tmp/err
grep -q 'needs glibc 2.35 or later; this system has glibc 2.34' /tmp/err"

# Runs once the floor is glibc 2.28 (the manylinux_2_28 build).
echo "SKIP almalinux8 (the glibc floor is 2.35)"

# No CA store in the image: the client verifies with certifi's roots.
published=${ALPHA2_PUBLISHED_VERSION:-$(curl -fsSI https://github.com/alpha2-sh/alpha2/releases/latest |
  sed -n 's|^location: .*/tag/v\([^[:space:]]*\).*|\1|ip')}
uv=$(command -v uv)
if [ -z "$uv" ] || [ -z "$published" ]; then
  echo "SKIP ubuntu2404-noca (needs uv on PATH and a published release)"
else
  run_case ubuntu2404-noca ubuntu:24.04 "set -e
test ! -e /etc/ssl/certs/ca-certificates.crt
cp /uv-binary /usr/local/bin/uv
uv venv -q --python 3.11 /v
uv pip install -q --python /v --no-index --find-links /artifacts --no-deps alpha2
uv pip install -q --python /v certifi
/v/bin/python -c 'from alpha2.install import _open
with _open(\"https://github.com/alpha2-sh/alpha2/releases/download/v$published/checksums.sha256\", \"host\", None) as r:
    assert b\"alpha2-\" in r.read()'" -v "$uv:/uv-binary:ro"
fi
exit "$failed"
