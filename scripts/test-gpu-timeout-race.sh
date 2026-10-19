#!/usr/bin/env bash
# Builds the Vulkan wait-timeout setter test under ThreadSanitizer and runs it: concurrent
# setters must not race. No GPU is needed.
set -euo pipefail
cd "$(dirname "$0")/.."
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
read -r -a ffmpeg_cflags <<< "$(pkg-config --cflags libavutil)"
"${CC:-cc}" -std=c11 -D_XOPEN_SOURCE=700 -O1 -g -fsanitize=thread -fPIE -pie \
  -Ibindings/alpha2-build-support/include -Ibindings/alpha2-native-gpu/include "${ffmpeg_cflags[@]}" \
  bindings/alpha2-native-gpu/c/vulkan.c \
  bindings/alpha2-native-gpu/tests/timeout_config.c \
  -l:libvulkan.so.1 -pthread -o "$scratch/timeout-config"
# Avoid ASLR placing libraries inside GCC TSan's reserved shadow range.
TSAN_OPTIONS=halt_on_error=1 setarch "$(uname -m)" -R "$scratch/timeout-config"
echo 'GPU timeout configuration: ThreadSanitizer passed'
