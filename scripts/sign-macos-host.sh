#!/bin/sh
#   sign-macos-host.sh <archive-root> <identity> [<entitlements>]
set -eu

usage="usage: sign-macos-host.sh <archive-root> <identity> [<entitlements>]"
root=${1:?$usage}
identity=${2:?$usage}
entitlements=${3:-}

fail() {
  echo "sign-macos-host.sh: $*" >&2
  exit 1
}

[ "$(uname -s)" = Darwin ] || fail "codesign runs on macOS only"
[ -x "$root/bin/alpha2-host" ] || fail "$root/bin/alpha2-host is not there; pass the unpacked archive"
[ -d "$root/lib" ] || fail "$root/lib is not there; pass the unpacked archive"
[ -z "$entitlements" ] || [ -f "$entitlements" ] || fail "$entitlements is not a file"
if [ "$identity" = - ]; then
  timestamp=--timestamp=none
else
  timestamp=--timestamp
fi

sign() {
  codesign --force --sign "$identity" --options runtime "$timestamp" "$@" ||
    fail "codesign failed: $*"
}

find "$root/lib" -type f -name '*.dylib' | grep -q . || fail "$root/lib holds no dylib"
find "$root/lib" -type f -name '*.dylib' \
  -exec codesign --force --sign "$identity" --options runtime "$timestamp" {} + ||
  fail "codesign failed on a dylib in $root/lib"
if [ -x "$root/bin/alpha2" ]; then
  sign "$root/bin/alpha2"
fi
if [ -n "$entitlements" ]; then
  sign --entitlements "$entitlements" "$root/bin/alpha2-host"
else
  sign "$root/bin/alpha2-host"
fi

find "$root/lib" -type f -name '*.dylib' -exec codesign --verify --deep --strict {} + ||
  fail "a dylib in $root/lib does not verify"
for signed in "$root/bin/alpha2" "$root/bin/alpha2-host"; do
  [ -e "$signed" ] || continue
  codesign --verify --deep --strict "$signed" || fail "$signed does not verify"
done

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
le32() {
  # shellcheck disable=SC2059
  printf "$(printf '\\%03o\\%03o\\%03o\\%03o' $(($1 % 256)) $(($1 / 256 % 256)) \
    $(($1 / 65536 % 256)) $(($1 / 16777216)))"
}
header='{"protocolVersion":2,"requestId":"signed","operation":"capabilities"}'
{ le32 ${#header}; le32 0; printf '%s' "$header"; } > "$scratch/request"
if ! "$root/bin/alpha2-host" serve < "$scratch/request" > "$scratch/answer" 2> "$scratch/serve.log" ||
  ! grep -aq '"requestId":"signed"' "$scratch/answer" || ! grep -aq '"ok":true' "$scratch/answer"; then
  fail "the signed alpha2-host gave no capability report:
$(cat "$scratch/serve.log")"
fi
echo "signed and verified $root with $identity${entitlements:+ and $entitlements}; alpha2-host answers capabilities"
