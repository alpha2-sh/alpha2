#!/bin/sh
# Fails unless every DLL that the executables and DLLs in a Windows directory import at load time is
# in that directory or part of Windows. A clean Windows install has only those: a MinGW or MSVC
# runtime the build machine happens to have on PATH is not there.
#   check-pe-imports.sh <dir>
set -eu

dir=${1:?usage: check-pe-imports.sh <dir>}

# DLLs every supported Windows has. The UCRT is a system component (api-ms-win-crt-*); vcruntime140
# and the MinGW runtimes are not. Media Foundation (mfplat, mf, mfreadwrite) is absent on Windows
# N/KN without the Media Feature Pack and on Windows Server without the feature, so it must stay a
# run-time load: an import of it fails the check.
system="kernel32.dll user32.dll gdi32.dll advapi32.dll ole32.dll oleaut32.dll shell32.dll shlwapi.dll
bcrypt.dll bcryptprimitives.dll crypt32.dll secur32.dll ws2_32.dll ntdll.dll userenv.dll version.dll
avicap32.dll winmm.dll
d3d11.dll d3d12.dll dxgi.dll dxva2.dll opengl32.dll dwmapi.dll setupapi.dll cfgmgr32.dll
ucrtbase.dll msvcrt.dll psapi.dll iphlpapi.dll ncrypt.dll propsys.dll comctl32.dll synchronization.dll"

. "$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)/scripts/lib/windows-tools.sh"
llvm_readobj=$(windows_tool llvm-readobj LLVM/bin) || {
  echo "check-pe-imports.sh needs llvm-readobj (LLVM): on PATH or in Program Files/LLVM/bin" >&2
  exit 1
}

readobj() {
  if command -v cygpath >/dev/null 2>&1; then
    "$llvm_readobj" --coff-imports "$(cygpath -w "$1")"
  else
    "$llvm_readobj" --coff-imports "$1"
  fi
}

present=$(find "$dir" -maxdepth 1 -type f \( -iname '*.dll' -o -iname '*.exe' \) -exec basename {} \; \
  | tr '[:upper:]' '[:lower:]')
failed=0
for file in $(find "$dir" -maxdepth 1 -type f \( -iname '*.dll' -o -iname '*.exe' \) | sort); do
  # A command of its own: in a pipeline only the last status counts, so a file llvm-readobj could
  # not read would pass as one that imports nothing.
  listing=$(readobj "$file") || {
    echo "llvm-readobj could not list the imports of $(basename "$file")" >&2
    exit 1
  }
  imports=$(printf '%s\n' "$listing" | sed -n 's/^ *Name: *//p' | tr '[:upper:]' '[:lower:]' | sort -u)
  for import in $imports; do
    case "$import" in
      api-ms-win-* | ext-ms-*) continue ;;
    esac
    for known in $present $system; do
      [ "$import" = "$known" ] && continue 2
    done
    echo "$(basename "$file") imports $import, which is neither shipped beside it nor part of Windows" >&2
    failed=1
  done
done
exit "$failed"
