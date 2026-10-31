#!/usr/bin/env bash
# Builds the alpha2-host and alpha2 wheels from a bundled archive and checks the client finds the
# host in a fresh virtual environment.
#   build-wheels.sh <alpha2-VERSION-TARGET-bundled.tar.gz|.zip> <out-dir> <check-venv>
set -euo pipefail

archive=${1:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
out=${2:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
venv=${3:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
workspace=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

uv run --no-project --python 3.12 "$workspace/python/alpha2-host/build_wheel.py" "$archive" "$out"

# Run from the package directory; a bare --manifest-path would skip pyproject.toml.
maturin_args=(build --release --out "$out")
(cd "$workspace/python/alpha2" && uvx maturin@1.15.0 "${maturin_args[@]}")
ls -l "$out"

uv venv --python 3.11 "$venv"
uv pip install --python "$venv" \
  "truststore>=0.10; sys_platform == 'darwin' or sys_platform == 'win32'" \
  "certifi>=2024.2.2; sys_platform == 'linux'"
uv pip install --python "$venv" --no-index --find-links "$out" alpha2
python="$venv/bin/python"
[ -d "$venv/Scripts" ] && python="$venv/Scripts/python"
host=$("$python" -c '
import pathlib, alpha2, alpha2_host
found = alpha2.resolve_host_binary()
assert found == pathlib.Path(alpha2_host.host_path()), found
print(found)')
{ "$host" 2>&1 || true; } | grep "usage: alpha2-host"
