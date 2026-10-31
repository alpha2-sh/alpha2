# A current Mesa for the release gate's RADV run: Vulkan Video on AMD GPUs with Mesa's RADV driver,
# which the host machine's own Mesa may predate. The test executables are built on the host and
# run in this image with the repository mounted read-only (radv-container.sh).
FROM ubuntu:24.04
RUN apt-get update \
  && apt-get install --yes --no-install-recommends mesa-vulkan-drivers libvulkan1 vulkan-tools \
  && rm -rf /var/lib/apt/lists/*
