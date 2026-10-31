#!/bin/sh
# Alpha2FixtureEmojiColr.ttf and Alpha2FixtureEmojiBitmap.ttf: Noto Color Emoji 2.051 (OFL, no
# reserved font name) COLRv1 and CBDT builds, subset and renamed, keeping the joining ligatures.
set -eu
usage='usage: font-emoji.sh /path/to/Noto-COLRv1.ttf /path/to/NotoColorEmoji.ttf <fonts directory>'
colr=${1:?$usage}
cbdt=${2:?$usage}
fonts=${3:?$usage}
here="$(cd "$(dirname "$0")" && pwd)"
# U+1F441 and U+2764 are text presentation by default.
unicodes=U+1F600,U+1F441,U+2764,U+1F44D,U+1F3FD,U+1F468,U+1F469,U+1F467,U+1F1F0,U+1F1F7,U+0031,U+20E3,U+200D,U+FE0F,U+0020
subset() {
  pyftsubset "$1" --unicodes="$unicodes" --layout-features='*' --output-file="$2" --name-IDs='*' \
    --no-hinting
  python3 - "$2" "$3" <<'PY'
import sys
from fontTools.ttLib import TTFont
font = TTFont(sys.argv[1], recalcTimestamp=False)
family = sys.argv[2]
postscript = family.replace(" ", "") + "-Regular"
names = {1: family, 3: postscript, 4: family + " Regular", 6: postscript, 16: family}
for record in font["name"].names:
    if record.nameID in names:
        record.string = names[record.nameID]
font.save(sys.argv[1])
PY
  sha256sum "$2"
}
subset "$colr" "$fonts/Alpha2FixtureEmojiColr.ttf" "Alpha2 Fixture Emoji Colr"
subset "$cbdt" "$fonts/Alpha2FixtureEmojiBitmap.ttf" "Alpha2 Fixture Emoji Bitmap"
python3 "$here/font-color.py" "$fonts"
sha256sum "$fonts/Alpha2FixtureColor.ttf"
