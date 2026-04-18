#!/bin/bash
set -e

echo "Installing HAProxy Zed Extension (Development Mode)..."

# Build everything first
./build.sh

# Get Zed extensions directory
ZED_EXTENSIONS_DIR="$HOME/Library/Application Support/Zed/extensions/work"
EXTENSION_DIR="$ZED_EXTENSIONS_DIR/haproxy"

echo "Installing to: $EXTENSION_DIR"

# Create extension directory
mkdir -p "$EXTENSION_DIR"

# Copy extension files
cp extension.toml "$EXTENSION_DIR/"
cp extension.wasm "$EXTENSION_DIR/"
cp -r languages "$EXTENSION_DIR/"
cp -r grammars "$EXTENSION_DIR/" 2>/dev/null || echo "No grammars directory to copy"

# Copy LSP server binary to extension root (Zed resolves the command name
# relative to the extension directory when it is not found on PATH).
cp bin/haproxy-lsp "$EXTENSION_DIR/haproxy-lsp"
chmod +x "$EXTENSION_DIR/haproxy-lsp"

echo ""
echo "✅ Extension installed successfully!"
echo "📁 Location: $EXTENSION_DIR"
echo "🔧 LSP Server: $EXTENSION_DIR/haproxy-lsp"
echo ""
echo "Next steps:"
echo "1. Restart Zed"
echo "2. Open a HAProxy config file (.cfg, .conf, .haproxy)"
echo "3. Try 'Go to Definition' (F12) on backend names in use_backend statements"
echo ""
echo "Note: This is a development installation. For production, the LSP server"
echo "should be distributed via a proper package manager or download mechanism."