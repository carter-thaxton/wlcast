#!/bin/sh
# Build the static "gst-full" GStreamer that `cargo static-gst` links.
#
# Usage: scripts/build-gst-full.sh [WORKDIR [VERSION]]
#   WORKDIR  holds the source, build and install trees (created if needed;
#            default target/gst-full in this crate)
#   VERSION  GStreamer release tag (default 1.26.11)
#
# Then build wlcast with `cargo static-gst`, which looks in target/gst-full
# by default; for another WORKDIR, set PKG_CONFIG_PATH=WORKDIR/prefix/lib/pkgconfig.
#
# Only the plugins wlcast uses are built. glib, orc, opus and x264 are built
# from meson subprojects and linked statically, so the result doesn't
# depend on the build host's -dev packages; libva stays a shared library.
set -eu

case "${1:-}" in -h|--help) sed -n '2,15s/^# \{0,1\}//p' "$0"; exit 0 ;; esac
CRATE=$(realpath "$(dirname "$0")/..")
DEFAULT_WORK=$CRATE/target/gst-full
WORK=$(realpath -m "${1:-$DEFAULT_WORK}")
VERSION=${2:-1.26.11}
SRC=$WORK/gstreamer-$VERSION
BUILD=$WORK/build-$VERSION
PREFIX=$WORK/prefix

# 1.26 matches gst-full-plugins against library file names, though its
# README shows bare plugin names ("coreelements"), which fail to match.
PLUGINS=""
for p in coreelements app videoconvertscale videorate audioconvert audioresample \
         opus rtp rtpmanager va videoparsersbad x264 rtspclientsink; do
  PLUGINS="$PLUGINS${PLUGINS:+;}libgst$p.a"
done

mkdir -p "$WORK"
if [ ! -d "$SRC" ]; then
  git clone --depth 1 --branch "$VERSION" \
    https://gitlab.freedesktop.org/gstreamer/gstreamer.git "$SRC"
fi

# Reconfigure an existing build (picks up changed options); start over if
# an earlier setup failed partway.
reconfigure=""
if [ -f "$BUILD/build.ninja" ]; then
  reconfigure=--reconfigure
else
  rm -rf "$BUILD"
fi
# auto_features=disabled turns off everything optional, here and in the
# subprojects; the plugins above are then enabled one by one. It also
# turns off SIMD, so that's re-enabled: orc (videoconvertscale and
# audioconvert), x264's assembly, opus intrinsics. tools=enabled builds
# orc's orcc (orc:tools yields to it), which -Dorc=enabled requires; it
# also builds gst-inspect-1.0 etc. against gst-full.
meson setup $reconfigure "$BUILD" "$SRC" \
  --prefix="$PREFIX" --libdir=lib \
  --buildtype=release --default-library=static \
  --force-fallback-for=glib,libffi,pcre2,zlib,orc,opus,x264 \
  -Dauto_features=disabled \
  -Dgst-full=enabled \
  -Dgst-full-target-type=static_library \
  -Dgst-full-plugins="$PLUGINS" \
  -Dbase=enabled -Dgood=enabled -Dbad=enabled -Dugly=enabled \
  -Drtsp_server=enabled -Dgpl=enabled \
  -Dorc=enabled -Dtools=enabled \
  -Dx264:asm=enabled -Dx264:cli=false -Dx264:bit-depth=8 \
  -Dopus:intrinsics=enabled \
  -Dgstreamer:registry=false \
  -Dgstreamer:ptp-helper=disabled \
  -Dgst-plugins-base:app=enabled \
  -Dgst-plugins-base:videoconvertscale=enabled \
  -Dgst-plugins-base:videorate=enabled \
  -Dgst-plugins-base:audioconvert=enabled \
  -Dgst-plugins-base:audioresample=enabled \
  -Dgst-plugins-base:opus=enabled \
  -Dgst-plugins-good:rtp=enabled \
  -Dgst-plugins-good:rtpmanager=enabled \
  -Dgst-plugins-bad:va=enabled \
  -Dgst-plugins-bad:videoparsers=enabled \
  -Dgst-plugins-ugly:x264=enabled \
  -Dgst-rtsp-server:rtspclientsink=enabled

meson compile -C "$BUILD"
meson install -C "$BUILD" --quiet

echo
echo "Installed to $PREFIX. Build wlcast with:"
if [ "$WORK" = "$DEFAULT_WORK" ]; then
  echo "  cargo static-gst"
else
  echo "  PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig cargo static-gst"
fi
