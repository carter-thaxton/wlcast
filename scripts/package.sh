#!/bin/sh
# Build a release tarball of the static binary for this machine's
# architecture: target/dist/wlcast_vVERSION_linux_ARCH.tar.gz, plus a
# .sha256 file.
#
# Usage: scripts/package.sh [--allow-dirty] [WORKDIR]
#   WORKDIR        gst-full's directory, as given to build-gst-full.sh
#                  (default target/gst-full in this crate)
#   --allow-dirty  package even with uncommitted changes (for testing)
#
# Needs gst-full (scripts/build-gst-full.sh), cargo-about
# (`cargo install --locked --features cli cargo-about`), and Rust
# RELEASE_RUST (default below, override in the environment): either as the
# default rustc or installed with rustup under that version's name.  Other
# builds use whatever Rust you have.
#
# The binary is built with `cargo static-gst` and checked: only the expected
# shared libraries, a glibc requirement no newer than GLIBC_MAX (default
# 2.39, override in the environment), `--version` matching Cargo.toml, and a
# test-pattern run that gets as far as picking an encoder.  The tarball holds
# the binary, LICENSE, README.md, mediamtx.yml, and THIRD_PARTY_LICENSES.txt.
set -eu

ALLOW_DIRTY=
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) sed -n '2,21s/^# \{0,1\}//p' "$0"; exit 0 ;;
    --allow-dirty) ALLOW_DIRTY=1; shift ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
CRATE=$(realpath "$(dirname "$0")/..")
WORK=$(realpath -m "${1:-$CRATE/target/gst-full}")
GLIBC_MAX=${GLIBC_MAX:-2.39}
RELEASE_RUST=${RELEASE_RUST:-1.99.1}
cd "$CRATE"

die() { echo "package: $*" >&2; exit 1; }

VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
case "$(uname -m)" in
  x86_64) ARCH=amd64 ;;
  aarch64) ARCH=arm64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
NAME=wlcast_v${VERSION}_linux_$ARCH

if [ -z "$ALLOW_DIRTY" ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  die "uncommitted changes (use --allow-dirty for a test package)"
fi
if [ "$(git tag --points-at HEAD)" != "v$VERSION" ]; then
  echo "package: note: HEAD isn't tagged v$VERSION" >&2
fi

export PKG_CONFIG_PATH=$WORK/prefix/lib/pkgconfig
GST=$(pkg-config --modversion gstreamer-1.0 2>/dev/null) ||
  die "no gst-full in $WORK; run scripts/build-gst-full.sh first"
SUBPROJECTS=$WORK/gstreamer-$GST/subprojects
BUILT=$WORK/build-$GST/subprojects
command -v cargo-about >/dev/null ||
  die "cargo-about missing: cargo install --locked --features cli cargo-about"
# Use the default rustc if it's the release version (rustup may call it
# "stable"), else the toolchain installed under that version's name.
if [ "$(rustc --version 2>/dev/null | cut -d' ' -f2)" != "$RELEASE_RUST" ]; then
  command -v rustup >/dev/null && rustup toolchain list | grep -q "^$RELEASE_RUST-" ||
    die "needs Rust $RELEASE_RUST: rustup toolchain install $RELEASE_RUST"
  export RUSTUP_TOOLCHAIN=$RELEASE_RUST
fi
rustc=$(rustc --version)

cargo static-gst
BIN=target/static/release/wlcast

# --- Checks on the binary ---

allowed="libc.so.6 libm.so.6 libgcc_s.so.1 libpipewire-0.3.so.0 libva.so.2
  libva-drm.so.2 libdrm.so.2 ld-linux-aarch64.so.1 ld-linux-x86-64.so.2"
for lib in $(readelf -d "$BIN" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
  echo "$allowed" | grep -qw -- "$lib" || die "unexpected shared library: $lib"
done

glibc=$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -n 1)
[ "$(printf '%s\n' "$glibc" "$GLIBC_MAX" | sort -V | tail -n 1)" = "$GLIBC_MAX" ] ||
  die "needs glibc $glibc, newer than GLIBC_MAX=$GLIBC_MAX"

[ "$("$BIN" --version)" = "wlcast $VERSION" ] ||
  die "--version says '$("$BIN" --version)', expected 'wlcast $VERSION'"

# Port 9 (discard) refuses the connection; picking an encoder is enough.
log=$(RUST_LOG=info timeout 10 "$BIN" --test-pattern \
  --rtsp-url rtsp://127.0.0.1:9/check 2>&1 || true)
encoder=$(echo "$log" | sed -n 's/.*INFO  wlcast::pipeline\] encoder: //p' | head -n 1)
[ -n "$encoder" ] || { echo "$log" >&2; die "test-pattern run picked no encoder"; }

# --- Third-party notices ---

# Version of a subproject: the wrap's git revision, else its directory's
# version suffix.
subver() {
  wrap=$SUBPROJECTS/$1.wrap
  rev=$(sed -n 's/^revision *= *//p' "$wrap")
  [ -n "$rev" ] && { echo "$rev"; return; }
  sed -n 's/^directory *= *//p' "$wrap" | sed "s/^$1-//"
}
subdir() { sed -n 's/^directory *= *//p' "$SUBPROJECTS/$1.wrap"; }

# name|component|license|source|license file, for everything gst-full links.
components() {
  g=$SUBPROJECTS
  echo "gstreamer|GStreamer $GST (core; plugins base, good, bad, ugly; rtsp-server)|LGPL-2.1-or-later|https://github.com/GStreamer/gstreamer/tree/$GST|$g/gstreamer/COPYING"
  v=$(subver glib); echo "glib|GLib $v|LGPL-2.1-or-later|https://github.com/GNOME/glib/tree/$v|$g/$(subdir glib)/COPYING"
  v=$(subver libffi); echo "libffi|libffi (meson port $v)|MIT|https://gitlab.freedesktop.org/gstreamer/meson-ports/libffi/-/tree/$v|$g/libffi/LICENSE"
  v=$(subver opus); echo "opus|Opus $v|BSD-3-Clause|https://github.com/xiph/opus/tree/v$v|$g/$(subdir opus)/COPYING"
  v=$(subver orc); echo "orc|ORC $v|BSD-2-Clause|https://github.com/GStreamer/orc/tree/$v|$g/orc/COPYING"
  v=$(subver pcre2); echo "pcre2|PCRE2 $v|BSD-3-Clause WITH PCRE2-exception|https://github.com/PCRE2Project/pcre2/tree/pcre2-$v|$g/$(subdir pcre2)/LICENCE"
  v=$(subver x264); echo "x264|x264 (meson port $v)|GPL-2.0-or-later|https://gitlab.freedesktop.org/gstreamer/meson-ports/x264/-/tree/$v|$g/x264/COPYING"
  v=$(subver zlib); echo "zlib|zlib $v|Zlib|https://github.com/madler/zlib/tree/v$v|$g/$(subdir zlib)/LICENSE"
}

# Every subproject gst-full built must be listed above.
known=$(components | cut -d'|' -f1)
for d in "$BUILT"/*; do
  n=$(basename "$d")
  case "$n" in gst-*) n=gstreamer ;; esac
  echo "$known" | grep -qx -- "$(echo "$n" | sed 's/-[0-9.]*$//')" ||
    die "gst-full built $n, which THIRD_PARTY_LICENSES doesn't cover"
done

STAGE=target/dist/$NAME
rm -rf "$STAGE"
mkdir -p "$STAGE"
notices=$STAGE/THIRD_PARTY_LICENSES.txt
{
  cat <<EOF
wlcast $VERSION includes the third-party software listed below.  wlcast itself
is licensed under the GPL-3.0-or-later (see LICENSE).

C libraries, linked statically.  They are built from the GStreamer $GST
release by scripts/build-gst-full.sh, which fetches each subproject at the
version pinned by GStreamer's subprojects/*.wrap files.  Source code:

EOF
  components | while IFS='|' read -r _ what lic src _; do
    printf '  %s\n    License: %s\n    Source:  %s\n' "$what" "$lic" "$src"
  done
  cat <<EOF

Rust crates, compiled in: their names, versions, and licenses are listed in
the second part of this file.  Their source is on https://crates.io.

================================================================================
Part 1: license texts of the C libraries
================================================================================
EOF
  seen=
  components | while IFS='|' read -r _ what _ _ file; do
    [ -f "$file" ] || die "missing license file $file"
    sum=$(sha256sum "$file" | cut -c1-16)
    printf '\n--------------------------------------------------------------------------------\n%s\n\n' "$what"
    prev=$(echo "$seen" | sed -n "s/^$sum //p" | head -n 1)
    if [ -n "$prev" ]; then
      echo "Same license text as $prev, above."
    else
      cat "$file"
      seen="$seen
$sum $what"
    fi
  done
  cat <<EOF

================================================================================
Part 2: Rust crates and their license texts
================================================================================
EOF
  cargo about generate --fail --features static-gst \
    -c scripts/about.toml scripts/about.hbs
} > "$notices"

# --- Tarball ---

cp "$BIN" LICENSE README.md mediamtx.yml "$STAGE/"
mtime=$(git log -1 --format=%cI)
tar -C target/dist --sort=name --owner=0 --group=0 --numeric-owner \
  --mtime="$mtime" -czf "target/dist/$NAME.tar.gz" "$NAME"
(cd target/dist && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")

echo "package: target/dist/$NAME.tar.gz"
echo "package:   $rustc; glibc >= $glibc; encoder here: $encoder"
