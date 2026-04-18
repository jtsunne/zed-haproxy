#!/bin/bash
set -e

# Builds the haproxy-lsp binary used by the Zed extension.
#
# Zed compiles extension.wasm itself (via its internal wit-component step)
# whenever you register this directory with "zed: install dev extension", so
# we deliberately do NOT build the wasm artifact here — the raw cargo output
# is a plain wasm module and Zed needs a wasm *component*, which only Zed's
# own builder produces correctly.

echo "Building haproxy-lsp (LSP server binary)..."
cargo build --bin haproxy-lsp --features lsp-server --release

mkdir -p bin
cp target/release/haproxy-lsp bin/haproxy-lsp
chmod +x bin/haproxy-lsp

echo ""
echo "LSP server ready: bin/haproxy-lsp"
echo ""
echo "To install the extension in Zed (one-time):"
echo "  Cmd+Shift+P -> 'zed: install dev extension' -> pick this directory"
echo ""
echo "After rebuilding the LSP, restart Zed (or the language server) to pick"
echo "up the new binary."
