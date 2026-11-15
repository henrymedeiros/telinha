#!/bin/sh
# Downloads and prepares what the app needs to build and package, in .deps/
# (nothing is installed system-wide, no sudo needed):
#   - FFmpeg 9 (BtbN, LGPL) for Linux and Windows, built with the NVENC
#     of older drivers (scripts/ffmpeg-nvenc.sh, needs Docker)
#   - libva 2.22 (the new FFmpeg's VAAPI requires >= 2.21; LTS distros ship 2.20)
#   - PipeWire headers (extracted from Ubuntu .debs)
#   - Zig (builds targeting glibc 2.35), appimagetool and NSIS
# System requirements: curl, python3, ninja, clang, libdrm (headers), apt-get.
#
#   sh scripts/fetch-deps.sh          everything, to build and package
#   sh scripts/fetch-deps.sh teste    only what `cargo test` needs on Linux:
#                                     prebuilt FFmpeg, libva, PipeWire headers
set -eu
MODE="${1:-}"
case "$MODE" in
  ""|teste) ;;
  *) echo "usage: $0 [teste]"; exit 1 ;;
esac
cd "$(dirname "$0")/.."
D="$PWD/.deps"
mkdir -p "$D" "$D/debs"

# Pinned inputs: a dated release (or a commit) and the SHA-256 the file must
# have. To move one, change the name and the hash together, in a commit that
# says what it was. BtbN keeps month-end builds for about two years and daily
# ones for two weeks, so only a month-end build can be pinned.
FFMPEG_BUILD=autobuild-2026-10-31-12-59
FFMPEG_NAME=ffmpeg-n9.0.2-39-g7adfbe40dc
FFMPEG_LINUX_SHA256=5a58e2dffad4835ee3418939f2676d6898489f8e047299f5f62ee0da3f8dd5dc
FFMPEG_WIN_SHA256=e907b01fa4783913151237e37df2a10fa97d99a1598a021bf59d4ae62ffed4c0
LIBVA_SHA256=467c418c2640a178c6baad5be2e00d569842123763b80507721ab87eb7af8735
APPIMAGETOOL=1.9.1
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
APPIMAGE_RUNTIME=20251108
APPIMAGE_RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d

# Downloads $1 to $2 and stops unless its SHA-256 is $3.
fetch() {
  curl -fsSL -o "$2" "$1"
  echo "$3  $2" | sha256sum -c - >/dev/null 2>&1 || { echo "checksum mismatch: $1" >&2; rm -f "$2"; exit 1; }
}

B=https://github.com/BtbN/FFmpeg-Builds/releases/download/$FFMPEG_BUILD

# Built with the NVENC that runs on NVIDIA drivers from 2023 onward (BtbN's
# prebuilt ones need drivers from 2026). Without Docker, the prebuilt ones.
# For tests the prebuilt Linux one is enough: same FFmpeg, and no GPU is assumed.
TARGETS="linux64 win64"
[ "$MODE" = teste ] && TARGETS=linux64
for T in $TARGETS; do
  OUT="$D/ffmpeg-$([ $T = win64 ] && echo win || echo linux)"
  # A copy fetched for tests must not end up in a package.
  if [ "$MODE" != teste ] && [ -f "$OUT/.test-only" ]; then
    echo "FFmpeg ($T): replacing the copy fetched for tests"
    rm -rf "$OUT"
  fi
  [ -d "$OUT" ] && continue
  echo "FFmpeg ($T)"
  if [ "$MODE" != teste ] && command -v docker >/dev/null 2>&1; then
    sh scripts/ffmpeg-nvenc.sh $T
  else
    [ "$MODE" = teste ] || echo "warning: no Docker, using the prebuilt BtbN FFmpeg: its NVENC requires a 2026 driver"
    case $T in
      win64) fetch "$B/$FFMPEG_NAME-win64-lgpl-shared-9.0.zip" "$D/ffmpeg.zip" "$FFMPEG_WIN_SHA256" && unzip -q "$D/ffmpeg.zip" -d "$D" && rm "$D/ffmpeg.zip" ;;
      *) fetch "$B/$FFMPEG_NAME-linux64-lgpl-shared-9.0.tar.xz" "$D/ffmpeg.tar.xz" "$FFMPEG_LINUX_SHA256" && tar -xJf "$D/ffmpeg.tar.xz" -C "$D" && rm "$D/ffmpeg.tar.xz" ;;
    esac
    mv "$D/$FFMPEG_NAME-$T-lgpl-shared-9.0" "$OUT"
    if [ "$MODE" = teste ]; then touch "$OUT/.test-only"; fi
  fi
done

[ -d "$D/venv" ] || python3 -m venv "$D/venv"
"$D/venv/bin/pip" install -q meson==1.12.1 ziglang==0.17.0 patchelf==0.19.1.0

cat > "$D/zigcc-2.35" <<ZIG
#!/bin/sh
exec "$D/venv/bin/python3" -m ziglang cc -target x86_64-linux-gnu.2.35 -Wl,--allow-shlib-undefined "\$@"
ZIG
chmod +x "$D/zigcc-2.35"

if [ ! -f "$D/libva/lib/libva.so.2" ]; then
  echo "libva 2.22"
  fetch https://github.com/intel/libva/archive/refs/tags/2.22.0.tar.gz "$D/libva.tar.gz" "$LIBVA_SHA256" && tar -xzf "$D/libva.tar.gz" -C "$D" && rm "$D/libva.tar.gz"
  mkdir -p "$D/drm-include"
  cp /usr/include/xf86drm*.h "$D/drm-include/" && cp -r /usr/include/libdrm "$D/drm-include/"
  # sysconfdir, stripping and the prefix map keep this machine's paths out of the bundled library.
  (cd "$D/libva-2.22.0" && CC="$D/zigcc-2.35" CFLAGS="-O2 -I$D/drm-include -I$D/drm-include/libdrm -ffile-prefix-map=$D=." \
    "$D/venv/bin/meson" setup build --prefix="$D/libva" --libdir=lib --sysconfdir=/etc --strip -Dwith_x11=no -Dwith_glx=no -Dwith_wayland=no \
      -Denable_docs=false -Ddriverdir=/usr/lib/x86_64-linux-gnu/dri:/usr/lib64/dri:/usr/lib/dri \
    && ninja -C build install)
fi

if [ ! -d "$D/sysroot/usr/include/pipewire-0.3" ]; then
  echo "PipeWire headers"
  (cd "$D/debs" && apt-get download libpipewire-0.3-dev libspa-0.2-dev)
  for f in "$D"/debs/lib*-dev_*.deb; do dpkg-deb -x "$f" "$D/sysroot"; done
  ln -sf /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 "$D/sysroot/usr/lib/x86_64-linux-gnu/libpipewire-0.3.so"
fi

# The rest is only for packaging.
if [ "$MODE" = teste ]; then
  echo "done (tests only)"
  exit 0
fi

if [ ! -x "$D/appimagetool" ]; then
  fetch "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL/appimagetool-x86_64.AppImage" "$D/appimagetool" "$APPIMAGETOOL_SHA256"
  chmod +x "$D/appimagetool"
fi
# The runtime that appimagetool puts at the start of every AppImage: it would
# otherwise download the newest one while packaging.
if [ ! -f "$D/appimage-runtime" ]; then
  fetch "https://github.com/AppImage/type2-runtime/releases/download/$APPIMAGE_RUNTIME/runtime-x86_64" "$D/appimage-runtime" "$APPIMAGE_RUNTIME_SHA256"
fi
if [ ! -x "$D/nsis/usr/bin/makensis" ]; then
  (cd "$D/debs" && apt-get download nsis nsis-common)
  for f in "$D"/debs/nsis*.deb; do dpkg-deb -x "$f" "$D/nsis"; done
fi
command -v cargo-deb >/dev/null || cargo install cargo-deb --locked --version 3.8.0
command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild --locked --version 0.23.4
command -v cargo-xwin >/dev/null || cargo install cargo-xwin --locked --version 0.23.1
rustup target add x86_64-pc-windows-msvc >/dev/null
echo "done"
