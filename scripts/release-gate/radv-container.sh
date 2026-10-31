#!/usr/bin/env bash
# The release gate's RADV run (Linux with an AMD GPU): builds the native media test executables on
# this machine, then runs each in a rootless podman container with a current Mesa
# (mesa.Containerfile), the GPU's device nodes, RADV's video extensions switched on and
# Vulkan Video required. Prints vulkaninfo without and with RADV_PERFTEST first.
#   radv-container.sh
# Run with the release gate's environment (env.sh): the FFmpeg SDK under target/ is on
# LD_LIBRARY_PATH, which the container gets with the read-only repository.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root" || exit 2
if ! command -v podman > /dev/null; then
  echo "FAIL podman is not installed: the RADV run uses rootless podman with /dev/dri"
  exit 1
fi
image=alpha2-gate-mesa:ubuntu-24.04
podman build --tag "$image" --file scripts/release-gate/mesa.Containerfile scripts/release-gate || {
  echo "FAIL the Mesa image did not build"
  exit 1
}

list=$(mktemp)
trap 'rm -f "$list"' EXIT
cargo test --release --locked --no-run --message-format=json --all-features \
  -p alpha2-native -p alpha2-native-gpu -p alpha2-host 2> /dev/null \
  | node -e '
    let text = "";
    process.stdin.on("data", (chunk) => (text += chunk));
    process.stdin.on("end", () => {
      for (const line of text.split("\n")) {
        if (!line.startsWith("{")) continue;
        const message = JSON.parse(line);
        if (message.reason === "compiler-artifact" && message.profile.test && message.executable)
          console.log(`${message.target.name}\t${message.executable}`);
      }
    });' > "$list"
if [ ! -s "$list" ]; then
  echo "FAIL cargo test --no-run listed no test executables"
  exit 1
fi

in_container() {
  podman run --rm --device /dev/dri --group-add keep-groups \
    --volume "$root:$root:ro" --tmpfs "$root/target/tmp" --workdir "$root" \
    --env RADV_PERFTEST=video_decode,video_encode \
    --env VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/radeon_icd.json \
    --env ALPHA2_REQUIRE=vulkan-video,vulkan-video-encode \
    --env ALPHA2_GPU_STRICT=1 \
    --env LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-}" \
    --env ALPHA2_OPENH264_LIBRARY="${ALPHA2_OPENH264_LIBRARY:-}" \
    "$image" "$@"
}

echo "== vulkaninfo without RADV_PERFTEST"
in_container env -u RADV_PERFTEST vulkaninfo --summary
echo "== vulkaninfo with RADV_PERFTEST=video_decode,video_encode"
in_container vulkaninfo --summary

failed=0
while IFS=$'\t' read -r name executable; do
  echo "== $name ($executable)"
  if ! in_container "$executable" --test-threads=1; then
    echo "FAIL $name"
    failed=1
  fi
  if [ "$name" = alpha2_native_gpu ] && [[ "$executable" == */deps/alpha2_native_gpu-* ]]; then
    echo "== $name, the test that needs a Vulkan Video GPU (--ignored)"
    in_container "$executable" --test-threads=1 --ignored creates_one_device_for_wgpu_and_ffmpeg \
      || { echo "FAIL $name --ignored creates_one_device_for_wgpu_and_ffmpeg"; failed=1; }
  fi
done < "$list"
exit "$failed"
