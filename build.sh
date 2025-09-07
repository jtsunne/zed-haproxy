#!/bin/bash
set -e

echo "Building HAProxy Zed Extension with LSP server..."

# Build the LSP server binary
echo "Building LSP server..."
cargo build --bin haproxy-lsp --features lsp-server --release

# Build the extension WebAssembly
echo "Building extension WebAssembly..."
cargo build --target wasm32-unknown-unknown --release --lib

# Copy the extension WebAssembly
echo "Copying extension WebAssembly..."
cp target/wasm32-unknown-unknown/release/haproxy_zed.wasm extension.wasm

# Create a binaries directory and copy the LSP server
echo "Preparing LSP server binary..."
mkdir -p bin
cp target/release/haproxy-lsp bin/haproxy-lsp

# Make the binary executable
chmod +x bin/haproxy-lsp

# Verify files
echo "Verifying build artifacts..."
ls -la extension.wasm
ls -la bin/haproxy-lsp

echo "Build complete! Extension is ready for installation."
echo ""
echo "LSP server location: bin/haproxy-lsp"
echo "Extension WebAssembly: extension.wasm"