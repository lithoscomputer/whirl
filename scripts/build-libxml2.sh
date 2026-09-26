#!/usr/bin/env bash
# Build the pinned libxml2 from source as a static, position-independent
# library for the `whirl-xpath` crate (ADR evaluate-checks-in-rust §1.5).
# Run through `mise run build:libxml2`; mise.toml points pkg-config at the
# prefix and sets LIBXML2_STATIC=1, so every Whirl binary links it
# statically and needs no libxml2 on the user's system.
#
# The prefix records a stamp of this script and the platform. The build is
# skipped only when the stamp matches, so a change to the version or the
# configure flags rebuilds it.
#
# Needs: curl, tar with xz, a C compiler, and make.
set -euo pipefail
cd "$(dirname "$0")/.."

LIBXML2_VERSION=2.15.4
LIBXML2_SHA256=98087fd181d9070724f3fbc65c7377db03038eb92bd882374daff44940138821
LIBXML2_URL="https://download.gnome.org/sources/libxml2/${LIBXML2_VERSION%.*}/libxml2-${LIBXML2_VERSION}.tar.xz"

root="$PWD/target/libxml2"
prefix="$root/prefix"
tarball="$root/libxml2-${LIBXML2_VERSION}.tar.xz"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

stamp="$(sha256 "$0" | cut -d' ' -f1) $(uname -s) $(uname -m)"
if [ -f "$prefix/lib/libxml2.a" ] && [ "$(cat "$prefix/.stamp" 2>/dev/null)" = "$stamp" ]; then
  exit 0
fi

mkdir -p "$root"
if [ ! -f "$tarball" ]; then
  curl -fsSL -o "$tarball.part" "$LIBXML2_URL"
  mv "$tarball.part" "$tarball"
fi
echo "$LIBXML2_SHA256  $tarball" | sha256 -c - >/dev/null || {
  echo "libxml2 source checksum mismatch: $tarball" >&2
  rm -f "$tarball"
  exit 1
}

case "$(uname -s)" in
  Darwin)
    # Match rustc's default deployment target for aarch64-apple-darwin so
    # ld does not warn that libxml2's objects target a newer macOS.
    export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"
    jobs="$(sysctl -n hw.ncpu)"
    ;;
  *)
    jobs="$(nproc)"
    ;;
esac

build="$root/build"
staging="$root/prefix.new"
rm -rf "$build" "$staging"
mkdir -p "$build"
tar -xJf "$tarball" -C "$build" --strip-components=1

# Kept: the HTML parser and XPath, plus what the `libxml` crate's wrapper
# code references (output, C14N, the reader and push parsers, patterns,
# regexps, XML Schemas, validation) and thread support. Off: everything
# with an external library (iconv, ICU, zlib, readline, Python), network
# and catalog access, and the programs. Whirl decodes every input to
# UTF-8 before libxml2 sees it, so iconv is not needed.
(
  cd "$build"
  # Per-function sections let the linker drop unused libxml2 code.
  CFLAGS="-O2 -fPIC -ffunction-sections -fdata-sections" ./configure \
    --prefix="$staging" \
    --disable-shared --enable-static --with-pic \
    --disable-dependency-tracking --enable-silent-rules \
    --without-iconv --without-icu --without-zlib \
    --without-python --without-readline --without-history \
    --without-http --without-modules --without-catalog --without-debug \
    --without-xinclude --without-xptr --without-writer \
    --without-relaxng --without-schematron --without-sax1 \
    --without-legacy --without-docs \
    --with-html --with-xpath --with-output --with-c14n \
    --with-reader --with-push --with-pattern --with-regexps \
    --with-schemas --with-valid --with-threads --with-iso8859x \
    >"$root/configure.log" 2>&1 || {
      cat "$root/configure.log" >&2
      exit 1
    }
  make -j"$jobs" libxml2.la >"$root/make.log" 2>&1 || {
    tail -50 "$root/make.log" >&2
    exit 1
  }
  # Install only the library, headers, and pkg-config file.
  {
    make install-libLTLIBRARIES install-pkgconfigDATA
    make -C include/libxml install
  } >>"$root/make.log" 2>&1
)

# The pkg-config file names the staging prefix; point it at the final one.
sed -i.bak "s|$staging|$prefix|g" "$staging/lib/pkgconfig/libxml-2.0.pc"
rm -f "$staging/lib/pkgconfig/libxml-2.0.pc.bak" "$staging/lib/libxml2.la"
echo "$stamp" >"$staging/.stamp"
rm -rf "$prefix" "$build"
mv "$staging" "$prefix"
echo "built libxml2 $LIBXML2_VERSION in $prefix"
