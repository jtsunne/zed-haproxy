#!/bin/bash
set -e

echo "Installing HAProxy LSP globally..."

# Build the LSP server
echo "Building LSP server..."
cargo build --bin haproxy-lsp --features lsp-server --release

# Install the binary globally
echo "Installing haproxy-lsp to cargo bin directory..."
cargo install --path . --features lsp-server --bin haproxy-lsp

# Check installation
if which haproxy-lsp >/dev/null 2>&1; then
    echo "✅ haproxy-lsp installed successfully at: $(which haproxy-lsp)"
    
    # Test the binary
    echo "Testing LSP server..."
    if echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}' | haproxy-lsp >/dev/null 2>&1; then
        echo "✅ LSP server test passed"
    else
        echo "⚠️ LSP server test failed, but binary is installed"
    fi
else
    echo "❌ Installation failed. Make sure ~/.cargo/bin is in your PATH"
    echo "   Add this to your shell profile: export PATH=\"\$HOME/.cargo/bin:\$PATH\""
    exit 1
fi

echo ""
echo "🎉 HAProxy LSP server installed globally!"
echo "📁 Location: $(which haproxy-lsp)"
echo ""
echo "Next steps:"
echo "1. Rebuild and reinstall the Zed extension: ./install-dev.sh"
echo "2. Restart Zed"
echo "3. Open a HAProxy config file and test 'Go to Definition' (F12)"