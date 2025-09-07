# HAProxy Zed Extension - Installation Instructions

## Quick Setup

The extension needs the `haproxy-lsp` binary to be available in your PATH for "Go to Definition" to work.

### Step 1: Install LSP Server

```bash
# Build and install the LSP server to ~/.local/bin
cargo build --bin haproxy-lsp --features lsp-server --release
mkdir -p ~/.local/bin
cp target/release/haproxy-lsp ~/.local/bin/
chmod +x ~/.local/bin/haproxy-lsp
```

### Step 2: Add to PATH

Add `~/.local/bin` to your PATH if it's not already there:

```bash
# Add to your shell profile (~/.zshrc, ~/.bashrc, etc.)
export PATH="$HOME/.local/bin:$PATH"

# Reload your shell or run:
source ~/.zshrc  # or ~/.bashrc
```

### Step 3: Verify LSP Server

```bash
# Test that the LSP server is accessible
which haproxy-lsp
# Should output: /Users/[your-username]/.local/bin/haproxy-lsp

# Test the server responds
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}' | haproxy-lsp
# Should output LSP capabilities JSON
```

### Step 4: Install Extension

```bash
# Install the extension to Zed
./install-dev.sh
```

### Step 5: Restart Zed

**Important**: Restart Zed completely for the extension to load.

## Testing

1. Open a HAProxy config file (`.cfg`, `.conf`, or `.haproxy`)
2. Verify the status bar shows "haproxy" as the language
3. Try **F12** ("Go to Definition") on:
   - Backend names in `use_backend` statements
   - ACL names in conditional expressions

### Example Test File

Create a test file `test.haproxy`:

```haproxy
backend web_servers
  server web1 192.168.1.10:80

frontend main  
  bind *:80
  use_backend web_servers
  acl is_api path_beg /api
  use_backend api_servers if is_api

backend api_servers
  server api1 192.168.1.20:8080
```

Try **F12** on "web_servers" in the `use_backend web_servers` line - it should jump to the `backend web_servers` definition.

## Troubleshooting

### "Language server failed to spawn"

This means the `haproxy-lsp` binary isn't in your PATH:

1. Verify: `which haproxy-lsp` 
2. If not found, repeat Steps 1-2 above
3. Restart your terminal and Zed

### Extension not appearing

1. Check installation: `ls "$HOME/Library/Application Support/Zed/extensions/work/haproxy"`
2. Restart Zed completely
3. Check that HAProxy files show "haproxy" in the status bar

### "Go to Definition" not working

1. Verify the file is recognized as HAProxy (status bar should show "haproxy")
2. Check Zed's developer console for LSP errors
3. Ensure you're clicking on backend/ACL names, not just anywhere

## Success!

When working correctly, you should see:
- ✅ HAProxy files show syntax highlighting  
- ✅ Status bar shows "haproxy" language
- ✅ F12 jumps from `use_backend web_servers` to `backend web_servers`
- ✅ F12 jumps from ACL references to ACL definitions