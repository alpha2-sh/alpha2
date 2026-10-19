#!/bin/sh
# Alpha2FixtureHangul.ttf: the Hangul syllables below from Black Han Sans 1.001 (OFL, no reserved
# font name), renamed. It has no Latin, digits, or space, so a stack of it and Alpha2Fixture.ttf
# splits every mixed line into runs of both. Needs fontTools (`pip install fonttools`).
set -eu
source_font=${1:?usage: font-hangul.sh /path/to/BlackHanSans-Regular.ttf <fonts directory>}
fonts=${2:?usage: font-hangul.sh /path/to/BlackHanSans-Regular.ttf <fonts directory>}
out="$fonts/Alpha2FixtureHangul.ttf"
pyftsubset "$source_font" --text="안녕하세요한국어자막가나다라마바사아자차카타파하오늘은좋은날입니다" \
  --output-file="$out" --name-IDs='*' --no-hinting
python3 - "$out" <<'PY'
import sys
from fontTools.ttLib import TTFont
font = TTFont(sys.argv[1], recalcTimestamp=False)
names = {1: "Alpha2 Fixture Hangul", 3: "Alpha2FixtureHangul-Regular", 4: "Alpha2 Fixture Hangul Regular", 6: "Alpha2FixtureHangul-Regular"}
for record in font["name"].names:
    if record.nameID in names:
        record.string = names[record.nameID]
font.save(sys.argv[1])
PY
sha256sum "$out"
