#!/bin/bash
# Builds the plugin package advanced-editing-<version>.rgplugin (from pkg/), and
# pkg-diag/plugin.wasm, the same module with the diagnostic commands the tests use.
#
#   CARGO_TARGET_DIR  build directory (default: target/ in this folder)
#   AE_CONDA_ENV      run cargo inside this conda environment (default: cargo from PATH)
#
# Dependencies are not downloaded here (--offline): run `cargo fetch` once first.
set -euo pipefail
cd "$(dirname "$0")"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"
VERSION=$(sed -n 's/^  "version": "\([^"]*\)",$/\1/p' plugin.json | head -n 1)
WASM="$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/advanced_editing.wasm"

cargo_build() {
  if [ -n "${AE_CONDA_ENV:-}" ]; then
    conda run -n "$AE_CONDA_ENV" env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
      cargo build --offline --release --target wasm32-unknown-unknown "$@"
  else
    cargo build --offline --release --target wasm32-unknown-unknown "$@"
  fi
}

cargo_build --features diagnostics
mkdir -p pkg-diag
cp "$WASM" pkg-diag/plugin.wasm

cargo_build
rm -rf pkg
mkdir -p pkg
cp plugin.json pkg/
cp "$WASM" pkg/plugin.wasm
rm -f "advanced-editing-$VERSION.rgplugin"
(cd pkg && zip -X -q "../advanced-editing-$VERSION.rgplugin" plugin.json plugin.wasm)
echo "advanced-editing-$VERSION.rgplugin ($(wc -c < pkg/plugin.wasm | tr -d ' ') bytes of WebAssembly)"
