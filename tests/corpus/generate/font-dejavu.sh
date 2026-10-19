#!/bin/sh
# Alpha2Fixture.ttf: an unmodified copy of DejaVu Sans 2.37.
set -eu
source_font=${1:?usage: font-dejavu.sh /path/to/DejaVuSans.ttf <fonts directory>}
fonts=${2:?usage: font-dejavu.sh /path/to/DejaVuSans.ttf <fonts directory>}
install -m 0644 "$source_font" "$fonts/Alpha2Fixture.ttf"
sha256sum "$fonts/Alpha2Fixture.ttf"
