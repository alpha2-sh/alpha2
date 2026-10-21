#!/usr/bin/env bash
# Regenerates the test corpus twice in scratch directories (`tests/corpus/generate/generate.sh`,
# every step whose tools are present) and checks it writes the same bytes each time and, under the
# manifest's FFmpeg, the bytes `tests/corpus/MANIFEST.md` lists. A file that is not reproducible
# cannot tell a changed generator from a changed file.
#
# With FFMPEG_ENCODERS set, the files needing other encoders are generated too and everything is
# checked run against run only. DEJAVU_SANS and BLACK_HAN_SANS add the fixture fonts, and
# NOTO_EMOJI_COLRV1 and NOTO_EMOJI_CBDT the color ones.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
corpus="$root/tests/corpus"
pinned="N-119331"
version="$(ffmpeg -hide_banner -version | head -n 1 | awk '{print $3}')"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

generate() {
  bash "$corpus/generate/generate.sh" --out "$1"
  (cd "$1" && find . -type f -printf '%P\n' | LC_ALL=C sort | xargs sha256sum) >"$1.sha256"
}

generate "$scratch/first"
generate "$scratch/second"

status=0
if ! diff -u "$scratch/first.sha256" "$scratch/second.sha256"; then
  echo "error: generate.sh wrote different bytes in two runs (above)" >&2
  status=1
fi

if [[ -n "${FFMPEG_ENCODERS:-}" || "$version" != "$pinned"* ]]; then
  echo "note: ffmpeg $version${FFMPEG_ENCODERS:+ with FFMPEG_ENCODERS}; checked run against run only (MANIFEST.md needs $pinned alone)" >&2
  exit "$status"
fi

# The manifest's hashes (the parity frames' line aside) against the media outside extensions/.
declare -A listed
while read -r hash name; do
  listed["$name"]="$hash"
done < <(awk '/^```text/{inside=1; next} /^```/{inside=0} inside && $2 !~ /\*/' "$corpus/MANIFEST.md")

checked=0
while read -r hash name; do
  [[ "$name" == corpus/* ]] || continue
  name="${name#corpus/}"
  [[ "$name" == parity-frames/* || "$name" == extensions/* ]] && continue
  expected="${listed[$name]:-}"
  if [[ -z "$expected" ]]; then
    echo "error: $name is generated but has no hash in MANIFEST.md" >&2
    status=1
  elif [[ "$expected" != "$hash" ]]; then
    echo "error: $name regenerates as $hash, MANIFEST.md lists $expected" >&2
    status=1
  fi
  checked=$((checked + 1))
done <"$scratch/first.sha256"

frames="$(cd "$scratch/first/corpus" && cat parity-frames/*.png | sha256sum | awk '{print $1}')"
listed_frames="$(awk '/parity-frames\/\*\.png/{print $1}' "$corpus/MANIFEST.md")"
if [[ "$frames" != "$listed_frames" ]]; then
  echo "error: parity-frames/*.png regenerate as $frames, MANIFEST.md lists $listed_frames" >&2
  status=1
fi

echo "checked $checked files against MANIFEST.md with ffmpeg $version"
exit "$status"
