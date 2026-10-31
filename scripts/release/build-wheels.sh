#!/usr/bin/env bash
# Builds the two Python wheels of one release target from its bundled archive into <out-dir>, then
# installs them into a new virtual environment at <check-venv> and checks that the client finds the
# host the alpha2-host wheel carries, which starts with the libraries beside it. The packaging job
# runs it on every build, and distribute.sh on the archive a release publishes.
#   build-wheels.sh <alpha2-VERSION-TARGET-bundled.tar.gz|.zip> <out-dir> <check-venv>
# `alpha2-host` repackages the archive as it is. `alpha2` is the abi3 extension module with the
# client, which depends on it; it carries the release pins when
# python/alpha2/src/alpha2/_host_archives.json exists (distribute.sh writes it).
set -euo pipefail

archive=${1:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
out=${2:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
venv=${3:?usage: build-wheels.sh <archive> <out-dir> <check-venv>}
workspace=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$out"
out=$(cd "$out" && pwd)

uv run --no-project --python 3.12 "$workspace/python/alpha2-host/build_wheel.py" "$archive" "$out"

# Arguments of the alpha2 wheel's maturin build. From the package directory: maturin takes the
# name, metadata and crate from its pyproject.toml, which a bare --manifest-path would skip.
maturin_args=(build --release --out "$out")
(cd "$workspace/python/alpha2" && uvx maturin@1.15.0 "${maturin_args[@]}")
ls -l "$out"

# From the two wheels (and the trust packages alpha2 depends on), `pip install alpha2` brings the
# host it pins, and the client finds it inside the alpha2-host package.
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
