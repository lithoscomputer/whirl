#!/usr/bin/env bash
# Build and package a release archive for the current platform.
# Run through `mise run release` so the shim and libxml2 are built from
# source first (the repository-owned-tasks ADR requires it); the release
# binary embeds the built shim via build.rs and links libxml2 statically.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
target=$(rustc -vV | sed -n 's/^host: //p')
name="whirl-v${version}-${target}"
bin=target/release/whirl

test -f shim/dist/index.js || {
  echo "shim/dist/index.js missing; run through 'mise run release'" >&2
  exit 1
}

cargo build --release -p whirl

# The embedded shim carries this Playwright API name, which appears
# nowhere in the Rust source; its absence means build.rs found no shim
# dist and the binary would depend on WHIRL_SHIM_JS.
grep -aq 'getByPlaceholder' "$bin" || {
  echo "release binary is missing the embedded shim" >&2
  exit 1
}

"$bin" --version >/dev/null
"$bin" check examples/checkout.whirl

# libxml2 must be linked statically, so the binary runs on systems without
# it (ADR evaluate-checks-in-rust §1.5).
case "$(uname -s)" in
  Darwin) dynamic=$(otool -L "$bin") ;;
  *) dynamic=$(ldd "$bin" 2>&1 || true) ;;
esac
if printf '%s\n' "$dynamic" | grep -q libxml2; then
  echo "release binary links libxml2 dynamically:" >&2
  printf '%s\n' "$dynamic" >&2
  exit 1
fi
# `whirl check` validates a literal XPath expression through libxml2.
mkdir -p dist-release
xpath_probe=dist-release/xpath-probe.whirl
printf 'HTTP GET /feed\nASSERT status == 200\nASSERT xpath:"count(//_:entry)" >= 1\n' >"$xpath_probe"
"$bin" check "$xpath_probe"
printf 'HTTP GET /feed\nASSERT status == 200\nASSERT xpath:"//entry[" exists\n' >"$xpath_probe"
if "$bin" check "$xpath_probe" >/dev/null 2>&1; then
  echo "release binary accepted an invalid XPath expression" >&2
  exit 1
fi
rm -f "$xpath_probe"

rm -rf "dist-release/$name"
mkdir -p "dist-release/$name"
cp "$bin" README.md CHANGELOG.md LICENSE-MIT LICENSE-APACHE "dist-release/$name/"
tar -czf "dist-release/$name.tar.gz" -C dist-release "$name"

if command -v sha256sum >/dev/null 2>&1; then
  (cd dist-release && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
else
  (cd dist-release && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
fi

echo "packaged dist-release/$name.tar.gz"
cat "dist-release/$name.tar.gz.sha256"
