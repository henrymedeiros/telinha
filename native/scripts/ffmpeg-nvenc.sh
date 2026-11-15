#!/bin/sh
# FFmpeg 9 (Linux or Windows) with an NVENC that runs on not-so-recent NVIDIA drivers.
#
#   sh scripts/ffmpeg-nvenc.sh linux64|win64
#
# FFmpeg requires the driver for the NVENC API version it was built with. BtbN's
# prebuilt builds always use the newest headers (13.1: driver 610 or newer,
# from May 2026), which leaves out anyone on a driver from a few months ago.
# Here FFmpeg is built with the same BtbN system (Docker, LGPL, shared), only
# swapping the headers for the 12.1 series: driver 530 or newer on Linux, 531
# on Windows. The features
# the app uses (low-latency H.264) have existed since well before that.
#
# Output in .deps/ffmpeg-linux or .deps/ffmpeg-win. Needs Docker (~10 GB image).
set -eu
cd "$(dirname "$0")/.."
D="$PWD/.deps"
T="${1:?usage: ffmpeg-nvenc.sh linux64|win64}"
NVENC_SDK=sdk/12.1
# The commit that branch was at when this was pinned, and BtbN's build scripts
# at one commit: what runs here was looked at once, not whatever is newest.
# If BtbN's image moves on and this build breaks, move BUILDS_COMMIT to their
# current master in a commit that says so.
NVENC_COMMIT=610325d019ad75248aa6e7abe0f4909d2ac6bc69
BUILDS_COMMIT=c9fe5114060e507409f7ee07edfd82efaf25d767
IMAGE=ghcr.io/btbn/ffmpeg-builds/$T-lgpl-shared-9.0:latest
OUT=$D/ffmpeg-$([ "$T" = win64 ] && echo win || echo linux)

if [ "$(git -C "$D/FFmpeg-Builds" rev-parse HEAD 2>/dev/null || true)" != "$BUILDS_COMMIT" ]; then
  rm -rf "$D/FFmpeg-Builds"
  git init -q "$D/FFmpeg-Builds"
  git -C "$D/FFmpeg-Builds" fetch -q --depth 1 https://github.com/BtbN/FFmpeg-Builds "$BUILDS_COMMIT"
  git -C "$D/FFmpeg-Builds" checkout -q FETCH_HEAD
fi
docker pull -q "$IMAGE" >/dev/null
# The base image only has moving tags, so it cannot be pinned without risking
# a pin that upstream deletes; what was used is written down instead.
DIGEST=$(docker image inspect --format '{{index .RepoDigests 0}}' "$IMAGE")
docker tag "$IMAGE" btbn-$T-lgpl-shared-9.0:original
mkdir -p "$D/nvenc-antigo"
cat > "$D/nvenc-antigo/Dockerfile" <<DOCKER
FROM btbn-$T-lgpl-shared-9.0:original
RUN rm -rf /opt/ffbuild/include/ffnvcodec /opt/ffbuild/lib/pkgconfig/ffnvcodec.pc \\
 && git init -q /tmp/nv && git -C /tmp/nv fetch -q --depth 1 https://github.com/FFmpeg/nv-codec-headers $NVENC_COMMIT && git -C /tmp/nv checkout -q FETCH_HEAD \\
 && make -C /tmp/nv PREFIX=/opt/ffbuild install && rm -rf /tmp/nv
DOCKER
# Same name as the original image: BtbN's build.sh uses the local one.
docker build -q -t "$IMAGE" "$D/nvenc-antigo" >/dev/null

# In GitHub Actions, build.sh would name the image after the running repository
# (GITHUB_REPOSITORY) instead of using the one prepared above.
(cd "$D/FFmpeg-Builds" && rm -rf artifacts && env -u GITHUB_REPOSITORY ./build.sh $T lgpl-shared 9.0)
PKG=$(ls "$D"/FFmpeg-Builds/artifacts/*$T-lgpl-shared-9.0.zip "$D"/FFmpeg-Builds/artifacts/*$T-lgpl-shared-9.0.tar.xz 2>/dev/null | head -1)
rm -rf "$OUT" "$OUT.tmp"
mkdir -p "$OUT.tmp"
case "$PKG" in
  *.zip) unzip -q "$PKG" -d "$OUT.tmp" ;;
  *) tar -xJf "$PKG" -C "$OUT.tmp" ;;
esac
mv "$OUT".tmp/* "$OUT" && rmdir "$OUT.tmp"
printf 'base image: %s\nFFmpeg-Builds: %s\nnv-codec-headers: %s\n' "$DIGEST" "$BUILDS_COMMIT" "$NVENC_COMMIT" > "$OUT/BUILD-INFO"
echo "FFmpeg ready (NVENC $NVENC_SDK) in $OUT, from $DIGEST"
