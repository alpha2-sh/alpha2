#!/usr/bin/env bash
# Runs fuzz targets (fuzz/fuzz_targets/*.rs), all at once, each for SECONDS (default 1800), with
# nightly cargo-fuzz. Without TARGETs it runs every target fuzz/Cargo.toml lists.
#
#   scripts/fuzz.sh [SECONDS] [TARGET...]
#
# Seeds are written, never committed: each target's corpus directory (fuzz/corpus/<target>, ignored
# by git) starts from the test files it reads (tests/corpus/extensions, the project documents under
# tests/cases/, tests/corpus/captions with LF and CRLF line ends, the layouts of
# tests/golden/layout_measure.json, and its string values for layout_values); evaluate,
# audio_render, and vector_raster start empty. ALPHA2_FUZZ_SEEDS=<dir> adds <dir>/<target>/* to a target's seeds.
#
# An input that takes longer than ALPHA2_FUZZ_TIMEOUT seconds (default 20) or more than
# ALPHA2_FUZZ_RSS_MB megabytes (default 2560) is a finding, as a panic is. Findings land in
# fuzz/artifacts/<target>/ and logs in fuzz/logs/<target>.log.
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

# Writes the project documents under tests/ into directory $1; with `compositions`, also each
# composition-issues case as a project.
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
    project_from_json | project_lint) projects "$dir" ;;
    composition_plan) projects "$dir" compositions ;;
    caption_import)
      for file in tests/corpus/captions/*; do
        case $file in *.srt) format=0 ;; *) format=1 ;; esac
        { printf %s "$format"; cat "$file"; } >"$dir/$(basename "$file")"
        # The same cues with CRLF line ends: git checks the seeds out with LF.
        { printf %s "$format"; sed 's/$/\r/' "$file"; } >"$dir/crlf-$(basename "$file")"
      done
      ;;
    layout_resolve)
      # Each pinned layout behind the header the target reads: width and height (u16 LE) and the
      # time in hundreds of ticks (i16 LE).
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
      # Every string value of the pinned layouts: lengths, shorthands, track lists, grid lines, hex.
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
# libFuzzer keeps the slowest input past 10 s: within the timeout, but worth running natively.
ls fuzz/artifacts/*/slow-unit-* 2>/dev/null | sed 's/^/slow (not a finding): /' || true
exit $status
