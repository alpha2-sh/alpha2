#!/usr/bin/env bash
# Runs fuzz targets in parallel with nightly cargo-fuzz, each for SECONDS (default 1800); all
# targets by default. Seeds come from test files at run time; ALPHA2_FUZZ_SEEDS=<dir> adds
# <dir>/<target>/*. ALPHA2_FUZZ_TIMEOUT (s) and ALPHA2_FUZZ_RSS_MB limits count as findings.
#
#   scripts/fuzz.sh [SECONDS] [TARGET...]
set -euo pipefail
cd "$(dirname "$0")/.."

seconds=${1:-1800}
shift || true
targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
  mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' fuzz/Cargo.toml | grep -vx alpha2-fuzz)
fi
timeout=${ALPHA2_FUZZ_TIMEOUT:-20}
rss=${ALPHA2_FUZZ_RSS_MB:-2560}
corpus=tests/corpus/extensions

projects() {
  node --input-type=module -e '
    import fs from "node:fs";
    import { expandRepeats } from "./scripts/lib/validation-corpus.mjs";
    const [dir, compositions] = process.argv.slice(1);
    const write = (name, project) =>
      fs.writeFileSync(`${dir}/${name}.json`, typeof project === "string" ? project : JSON.stringify(project));
    for (const corpus of ["project-validation", "lint", "used-sources", "edit"]) {
      const cases = expandRepeats(JSON.parse(fs.readFileSync(`tests/cases/${corpus}-corpus.json`, "utf8")));
      cases.forEach((test, index) => write(`${corpus}-${index}`, test.project ?? test.json));
    }
    const conformance = JSON.parse(fs.readFileSync("tests/cases/surface-conformance.json", "utf8"));
    for (const [name, project] of Object.entries(conformance.projects)) write(`conformance-${name}`, project);
    for (const file of fs.readdirSync("tests/cases").filter((name) => name.endsWith("-project.json"))) {
      fs.copyFileSync(`tests/cases/${file}`, `${dir}/${file}`);
    }
    if (compositions) {
      const base = JSON.parse(fs.readFileSync("tests/cases/lint-corpus.json", "utf8"))[0].project;
      const issues = JSON.parse(fs.readFileSync("tests/cases/composition-issues-corpus.json", "utf8"));
      issues.cases.forEach((test, index) =>
        write(`composition-issues-${index}`, {
          ...base,
          sources: test.sources,
          compositions: test.compositions,
          timeline: { tracks: [] },
        }));
    }' "$1" "${2:-}"
}

seed() {
  local target=$1 dir=fuzz/corpus/$1
  mkdir -p "$dir"
  case $target in
    extension_svg) cp "$corpus"/*.svg "$dir"/ ;;
    extension_animated) cp "$corpus"/*.gif "$corpus"/*.webp "$corpus"/*.png "$dir"/ ;;
    extension_exr) cp "$corpus"/*.exr "$dir"/ ;;
    still_decode) cp tests/corpus/*.jpg tests/corpus/*.png "$corpus"/*.webp "$dir"/ ;;
    container_mp4)
      find tests/corpus -maxdepth 1 \( -name '*.mp4' -o -name '*.mov' -o -name '*.m4a' \) -size -64k \
        -exec cp {} "$dir"/ \;
      cp tests/cases/container-mp4-fragmented.mp4 "$dir"/
      ;;
    container_matroska)
      # Header: the seek target (i64), then the file.
      find tests/corpus tests/cases -maxdepth 1 \( -name '*.mkv' -o -name '*.webm' \) -size -64k \
        -print0 | while IFS= read -r -d '' file; do
        { head -c 8 /dev/zero; cat "$file"; } >"$dir/$(basename "$file")"
      done
      ;;
    container_audio)
      # Header: the prefix selector (0, none) and the seek target (i64), then the file.
      for file in tests/corpus/*.wav tests/corpus/*.mp3 tests/corpus/*.aac tests/corpus/*.flac \
        tests/corpus/*.ogg; do
        { head -c 9 /dev/zero; cat "$file"; } >"$dir/$(basename "$file")"
      done
      ;;
    extension_pdf) cp "$corpus"/*.pdf "$dir"/ ;;
    extension_latex)
      # A style byte, then the formula.
      index=0
      for formula in 'x^2' '\frac{a}{b}' '\sqrt[3]{x}' '\begin{pmatrix}1&2\\3&4\end{pmatrix}' \
        '\sum_{i=0}^n i' '\widehat{ABC}' '\color{red}x' '\text{if } x'; do
        printf '%s%s' "$index" "$formula" >"$dir/formula-$index"
        index=$((index + 1))
      done
      ;;
    extension_lottie)
      cp "$corpus"/*.lottie "$dir"/
      for file in moving split unsupported; do cp "$corpus/$file.json" "$dir/$file.json"; done
      node -e '
        const [cases, dir] = process.argv.slice(1);
        const fs = require("fs");
        JSON.parse(fs.readFileSync(cases, "utf8")).forEach((test, index) => {
          if (test.animation) fs.writeFileSync(`${dir}/case-${index}.json`, JSON.stringify(test.animation));
        });' "$corpus/normalize-cases.json" "$dir"
      ;;
    project_from_json | project_lint | project_data) projects "$dir" ;;
    edit_apply)
      node -e '
        const [table, dir] = process.argv.slice(1);
        const fs = require("fs");
        const { projects, edit } = JSON.parse(fs.readFileSync(table, "utf8"));
        for (const test of edit)
          fs.writeFileSync(`${dir}/${test.name}.json`, JSON.stringify({ project: projects[test.project], request: test.request }));
      ' tests/cases/surface-conformance.json "$dir"
      ;;
    plugin_manifest)
      node -e '
        const [cases, dir] = process.argv.slice(1);
        const fs = require("fs");
        JSON.parse(fs.readFileSync(cases, "utf8")).forEach((test) =>
          fs.writeFileSync(`${dir}/${test.name}.json`, JSON.stringify(test.manifest)));
      ' tests/cases/plugin-manifest-corpus.json "$dir"
      ;;
    plugin_records)
      # Records as docs/plugin-abi.md lays them out: a sample, a word, an interval, and all three.
      node -e '
        const dir = process.argv[1];
        const fs = require("fs");
        const record = (tag, output, body) => {
          const head = Buffer.alloc(8);
          head.writeUInt8(tag, 0);
          head.writeUInt16LE(output, 2);
          head.writeUInt32LE(body.length, 4);
          return Buffer.concat([head, body]);
        };
        const i64 = (value) => { const b = Buffer.alloc(8); b.writeBigInt64LE(BigInt(value)); return b; };
        const u32 = (value) => { const b = Buffer.alloc(4); b.writeUInt32LE(value); return b; };
        const f32 = (value) => { const b = Buffer.alloc(4); b.writeFloatLE(value); return b; };
        const sample = record(1, 0, Buffer.concat([i64(4000), u32(2), u32(2), f32(0.5), f32(0)]));
        const word = record(2, 1, Buffer.concat([i64(0), i64(8000), u32(2), u32(1), Buffer.from("hiA")]));
        const interval = record(3, 2, Buffer.concat([i64(0), i64(4000), u32(1), f32(0.5), u32(5), Buffer.from("quiet")]));
        for (const [name, bytes] of Object.entries({ sample, word, interval, all: Buffer.concat([sample, word, interval]) }))
          fs.writeFileSync(`${dir}/${name}`, bytes);
      ' "$dir"
      ;;
    extract_emits)
      node -e '
        const dir = process.argv[1];
        const fs = require("fs");
        fs.writeFileSync(`${dir}/each-kind.json`, JSON.stringify([
          [{ output: "s", time: 0, values: [0.5, 0.25] }, { output: "i", start: 0, end: 4000, label: "a", score: 0.5 }],
          [{ output: "p", time: 4000, values: [0.1, 0.2] }, { output: "w", start: 0, end: 8000, text: "hi", speaker: "A" }],
          [{ output: "m", time: 8000, values: [1, 2, 30, 1] }],
          [{ output: "i", start: 8000, end: 12000 }],
        ]));
        fs.writeFileSync(`${dir}/lost.json`, JSON.stringify([[{ output: "s", time: 0, values: [null, 1] }]]));
      ' "$dir"
      ;;
    composition_plan) projects "$dir" compositions ;;
    caption_import)
      for file in tests/corpus/captions/*; do
        case $file in *.srt) format=0 ;; *) format=1 ;; esac
        { printf %s "$format"; cat "$file"; } >"$dir/$(basename "$file")"
        # Git checks the seeds out with LF.
        { printf %s "$format"; sed 's/$/\r/' "$file"; } >"$dir/crlf-$(basename "$file")"
      done
      ;;
    layout_resolve)
      # Header: width and height (u16 LE), time in hundreds of ticks (i16 LE).
      node -e '
        const [golden, dir] = process.argv.slice(1);
        const fs = require("fs");
        for (const test of JSON.parse(fs.readFileSync(golden, "utf8")).cases) {
          const head = Buffer.alloc(6);
          head.writeUInt16LE(test.width, 0);
          head.writeUInt16LE(test.height, 2);
          head.writeInt16LE(Math.trunc(test.time / 100), 4);
          fs.writeFileSync(`${dir}/${test.name}`, Buffer.concat([head, Buffer.from(JSON.stringify(test.generator.root))]));
        }' tests/golden/layout_measure.json "$dir"
      ;;
    layout_values)
      node -e '
        const [golden, dir] = process.argv.slice(1);
        const fs = require("fs");
        const strings = new Set();
        const walk = (value) => {
          if (typeof value === "string") strings.add(value);
          else if (value && typeof value === "object") Object.values(value).forEach(walk);
        };
        JSON.parse(fs.readFileSync(golden, "utf8")).cases.forEach((test) => walk(test.generator));
        [...strings].forEach((text, index) => fs.writeFileSync(`${dir}/value-${index}`, text));
      ' tests/golden/layout_measure.json "$dir"
      ;;
    evaluate | audio_render | vector_raster) ;;
    *) echo "unknown target $target" >&2; exit 2 ;;
  esac
  if [ -n "${ALPHA2_FUZZ_SEEDS:-}" ] && [ -d "$ALPHA2_FUZZ_SEEDS/$target" ]; then
    cp "$ALPHA2_FUZZ_SEEDS/$target"/* "$dir"/
  fi
}

mkdir -p fuzz/logs
for target in "${targets[@]}"; do seed "$target"; done
for target in "${targets[@]}"; do cargo +nightly fuzz build -O "$target"; done

pids=()
for target in "${targets[@]}"; do
  mkdir -p "fuzz/artifacts/$target"
  cargo +nightly fuzz run -O "$target" -- \
    -max_total_time="$seconds" -timeout="$timeout" -rss_limit_mb="$rss" -malloc_limit_mb="$rss" \
    -max_len=65536 -print_final_stats=1 >"fuzz/logs/$target.log" 2>&1 &
  pids+=($!)
done

status=0
for index in "${!pids[@]}"; do
  target=${targets[$index]}
  if wait "${pids[$index]}"; then
    echo "$target: no finding ($(grep -o 'stat::number_of_executed_units: *[0-9]*' "fuzz/logs/$target.log" | grep -o '[0-9]*$') runs, slowest $(grep -o 'Slowest unit: [0-9]* s' "fuzz/logs/$target.log" | tail -1 | grep -o '[0-9]* s' || echo 'under 10 s'))"
  else
    echo "$target: FINDING, see fuzz/logs/$target.log and fuzz/artifacts/$target/"
    status=1
  fi
done
# libFuzzer keeps inputs slower than 10 s; not findings, but worth running natively.
ls fuzz/artifacts/*/slow-unit-* 2>/dev/null | sed 's/^/slow (not a finding): /' || true
exit $status
