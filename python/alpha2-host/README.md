# alpha2-host wheels

`build_wheel.py` repackages a bundled native release archive (what `scripts/build-native-release.sh`
writes and the release job uploads) as the `alpha2-host` platform wheel the `alpha2` Python package
depends on:

```sh
python python/alpha2-host/build_wheel.py alpha2-0.4.2-linux-x64-bundled.tar.gz dist/
# dist/alpha2_host-0.4.2-py3-none-manylinux_2_35_x86_64.whl
```

- The archive goes in as it is, under the `alpha2_host` package: `bin/`, `lib/`, `licenses/`,
  `compliance/`. Nothing is rebuilt or relinked, and auditwheel is not used: the executable loads
  FFmpeg from `$ORIGIN/../lib` (`@loader_path/../lib` on macOS), and those libraries stay shared
  so they remain replaceable under the LGPL.
- A wheel holds no symlinks: each library is stored under the name its symlinks give the loader
  (`libavutil.so.60`), and the unversioned development links are left out.
- The platform tag is read from the binaries: the newest `GLIBC_` symbol version they need on
  Linux (`manylinux_2_<n>`), the Mach-O minimum OS on macOS, `win_amd64` on Windows. A Linux
  archive built on Ubuntu 22.04 is `manylinux_2_35`, which older distributions cannot install.
- `alpha2_host.host_path()` is what the `alpha2` package looks up, after `ALPHA2_HOST_PATH` and
  before the shared cache; it uses the wheel only when `alpha2_host.__version__` is its own version.
- The wheel's version is Python's spelling of the archive's (`0.5.0-rc.1` is `0.5.0rc1`); the
  package's `__version__` keeps the archive's. Rebuilding the same archive gives the same bytes
  (entries are dated `SOURCE_DATE_EPOCH`, default 1980-01-01).

Its tests are `python/alpha2/tests/test_host_wheel.py`.
