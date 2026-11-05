#!/usr/bin/env bash
# Regenerates tests/corpus and tests/fonts. Steps run in this order (recovery cuts a media file):
#
#   media        `ffmpeg` on PATH (MANIFEST.md hashes come from FFmpeg N-119331). FFMPEG_ENCODERS
#                (libx264, libx265, libaom, libvpx, libopus, libvorbis, libmp3lame) adds the
#                encoded files.
#   recovery     FFMPEG_ENCODERS (libx264, libvpx-vp9) and the ffprobe beside it.
#   extensions   python3 with Pillow 9+ built with WebP animation.
#   fonts        DEJAVU_SANS (DejaVuSans.ttf 2.37), BLACK_HAN_SANS (BlackHanSans-Regular.ttf 1.001),
#                fontTools.
#   emoji-fonts  NOTO_EMOJI_COLRV1 (Noto-COLRv1.ttf), NOTO_EMOJI_CBDT (NotoColorEmoji.ttf) from
#                noto-emoji v2.051, fontTools.
#   weight-fonts fontTools only.
#
# Usage: generate.sh [--out DIR] [STEP...]
#
# With no STEP, steps whose tools are missing are skipped with a reason; a named STEP must run.
# --out DIR writes DIR/corpus and DIR/fonts. Not generated: captions/, extensions/label.svg,
# extensions/shapes.svg and cross_topology_parity.png.
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
    media | recovery | extensions | fonts | emoji-fonts | weight-fonts) ;;
    *) echo "error: unknown step '$step' (media, recovery, extensions, fonts, emoji-fonts, weight-fonts)" >&2; exit 2 ;;
  esac
done

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
  for generator in extensions.py extensions-exr.py extensions-lottie.py extensions-pdf.py; do
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

if wanted weight-fonts "$(python3 -c 'import fontTools' 2>/dev/null || echo 'fontTools')"; then
  mkdir -p "$fonts"
  python3 "$here/font-weights.py" "$fonts"
fi
