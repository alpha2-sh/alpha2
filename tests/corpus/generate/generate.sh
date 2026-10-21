#!/usr/bin/env bash
# Regenerates the test corpus: every generated file in tests/corpus and the fixture fonts in
# tests/fonts. One step per generator, run in this order (recovery cuts a file media writes):
#
#   media       media.sh, with audio-pts.py, no-vui.py, two-stsd.py: the `ffmpeg` on PATH.
#               tests/corpus/MANIFEST.md's hashes come from FFmpeg N-119331. With FFMPEG_ENCODERS
#               (an FFmpeg with libx264, libx265, libaom, libvpx, libopus, libvorbis) it also writes
#               the files that need those encoders.
#   recovery    recovery.py: the damaged-packet fixtures and RECOVERY-MANIFEST.md, encoded by
#               FFMPEG_ENCODERS (libx264, libvpx-vp9) and probed by the ffprobe beside it.
#   extensions  extensions.py, extensions-exr.py, extensions-lottie.py: python3 with Pillow 9+
#               built with WebP animation, into tests/corpus/extensions.
#   fonts       font-dejavu.sh, font-hangul.sh: the source fonts DEJAVU_SANS (DejaVuSans.ttf 2.37)
#               and BLACK_HAN_SANS (BlackHanSans-Regular.ttf 1.001) and fontTools' pyftsubset, into
#               tests/fonts.
#   emoji-fonts font-emoji.sh, font-color.py: the source fonts NOTO_EMOJI_COLRV1 (Noto-COLRv1.ttf)
#               and NOTO_EMOJI_CBDT (NotoColorEmoji.ttf) from noto-emoji v2.051 and fontTools'
#               pyftsubset, into tests/fonts.
#
# Usage: generate.sh [--out DIR] [STEP...]
#
# With no STEP, every step whose tools and inputs are present runs and the others say why they
# were skipped; a STEP that is named runs or fails. --out DIR writes DIR/corpus and DIR/fonts
# instead of tests/corpus and tests/fonts (scripts/check-corpus-reproducible.sh regenerates into
# scratch directories). No generator writes the caption fuzz seeds (captions/),
# extensions/label.svg and extensions/shapes.svg, and cross_topology_parity.png.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
tests="$(cd "$here/../.." && pwd)"
if [[ "${1:-}" == --out ]]; then
  tests="${2:?usage: generate.sh [--out DIR] [STEP...]}"
  shift 2
  mkdir -p "$tests"
  tests="$(cd "$tests" && pwd)"
fi
corpus="$tests/corpus"
fonts="$tests/fonts"
named=("$@")
for step in "${named[@]}"; do
  case "$step" in
    media | recovery | extensions | fonts | emoji-fonts) ;;
    *) echo "error: unknown step '$step' (media, recovery, extensions, fonts, emoji-fonts)" >&2; exit 2 ;;
  esac
done

# 0 when the step runs. A step that was not named is skipped, with the reason, when it cannot run;
# a named one fails.
wanted() {
  local step=$1 missing=$2
  if ((${#named[@]})) && [[ " ${named[*]} " != *" $step "* ]]; then
    return 1
  fi
  [[ -z "$missing" ]] && return 0
  if ((${#named[@]})); then
    echo "error: $step needs $missing" >&2
    exit 1
  fi
  echo "note: skipped $step: it needs $missing" >&2
  return 1
}

pillow_animates_webp() {
  python3 -c 'from PIL import features; raise SystemExit(not features.check("webp_anim"))' 2>/dev/null
}

if wanted media "$(command -v ffmpeg >/dev/null || echo 'ffmpeg on PATH')"; then
  mkdir -p "$corpus"
  bash "$here/media.sh" "$corpus"
fi

if wanted recovery "$([[ -n "${FFMPEG_ENCODERS:-}" ]] || echo 'FFMPEG_ENCODERS')"; then
  mkdir -p "$corpus"
  python3 "$here/recovery.py" "$corpus" "$FFMPEG_ENCODERS"
fi

if wanted extensions "$(pillow_animates_webp || echo 'Pillow with WebP animation')"; then
  mkdir -p "$corpus/extensions"
  for generator in extensions.py extensions-exr.py extensions-lottie.py; do
    (cd "$corpus/extensions" && python3 "$here/$generator")
  done
fi

if wanted fonts "$([[ -n "${DEJAVU_SANS:-}" && -n "${BLACK_HAN_SANS:-}" ]] && command -v pyftsubset >/dev/null || echo 'DEJAVU_SANS, BLACK_HAN_SANS and pyftsubset')"; then
  mkdir -p "$fonts"
  sh "$here/font-dejavu.sh" "$DEJAVU_SANS" "$fonts"
  sh "$here/font-hangul.sh" "$BLACK_HAN_SANS" "$fonts"
fi

if wanted emoji-fonts "$([[ -n "${NOTO_EMOJI_COLRV1:-}" && -n "${NOTO_EMOJI_CBDT:-}" ]] && command -v pyftsubset >/dev/null || echo 'NOTO_EMOJI_COLRV1, NOTO_EMOJI_CBDT and pyftsubset')"; then
  mkdir -p "$fonts"
  sh "$here/font-emoji.sh" "$NOTO_EMOJI_COLRV1" "$NOTO_EMOJI_CBDT" "$fonts"
fi
