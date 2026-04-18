# HAProxy Zed Extension with Go to Definition

A Zed editor extension that provides syntax highlighting and **"Go to Definition"** functionality for HAProxy configuration files.

## Features

- **Syntax Highlighting**: Rich syntax highlighting for HAProxy config files
- **Go to Definition**: Jump to backend/ACL definitions from references
- **Language Server Protocol**: Full LSP integration for navigation features

### Supported Navigation

- **Backend References**: `use_backend web_servers` → jumps to `backend web_servers`
- **Default Backend**: `default_backend api` → jumps to `backend api` 
- **ACL References**: `if is_mobile` → jumps to `acl is_mobile`

## Installation

### Prerequisites

- Rust installed via [rustup](https://rustup.rs/)
- Zed editor

### Development Installation

1. **Clone and build the LSP server**:
   ```bash
   git clone <repository>
   cd zed-haproxy
   ./build.sh
   ```
   This produces `bin/haproxy-lsp`. It does **not** build `extension.wasm` —
   Zed compiles that itself (see step 2).

2. **Register the extension with Zed** (one-time):
   `Cmd+Shift+P` → **`zed: install dev extension`** → select this directory.
   Zed will compile the extension from source (including the correct
   wasm-component encoding) and keep a symlink to this directory under
   `~/Library/Application Support/Zed/extensions/installed/haproxy`.

3. **Restart Zed**

After code changes, re-run `./build.sh` for the LSP and use
`zed: rebuild dev extension` (or reinstall) to refresh the wasm side. Never
copy a hand-built `extension.wasm` into Zed's extensions directory — `cargo
build --target wasm32-unknown-unknown` produces a plain wasm module, and
Zed requires a wasm *component*. Let Zed's builder handle it.

## Usage

1. Open HAProxy configuration files (`.cfg`, `.conf`, `.haproxy`)
2. Use **F12** or **"Go to Definition"** on:
   - Backend names in `use_backend` statements
   - Backend names in `default_backend` statements
   - ACL names in conditional expressions

### Example

```haproxy
backend web_servers          # ← DEFINITION
  server web1 192.168.1.10:80

frontend main
  bind *:80
  use_backend web_servers    # ← F12 here jumps to line 1
  acl is_api path_beg /api   # ← DEFINITION
  use_backend api if is_api  # ← F12 on "is_api" jumps to ACL
```

## Architecture

- **Extension Entry**: `src/lib.rs` - Zed extension integration
- **LSP Server**: `src/lsp_server.rs` - Language server with navigation logic
- **Grammar**: Tree-sitter grammar for syntax highlighting
- **Language Config**: `languages/haproxy/` - File associations and highlighting rules

## Development

### Building

```bash
# Build LSP server (what ./build.sh does)
cargo build --bin haproxy-lsp --features lsp-server --release
```

The extension's wasm artifact is built by Zed when you run
`zed: install dev extension` / `zed: rebuild dev extension`. Do not build
it yourself with `cargo build --target wasm32-unknown-unknown` — the
resulting plain wasm module won't load (Zed needs a wasm component).

### Testing LSP Server

```bash
# Test LSP server directly
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}' | ./bin/haproxy-lsp
```

### Project Structure

```
haproxy-zed/
├── src/
│   ├── lib.rs           # Extension entry point
│   └── lsp_server.rs    # LSP server implementation
├── languages/haproxy/   # Language configuration
├── grammars/           # Tree-sitter grammar
├── extension.toml      # Extension metadata
└── build.sh           # Build script
```

## Known Limitations

- **Single-file only**: References work within the same file
- **Simple parsing**: Uses regex-based parsing instead of full tree-sitter
- **Limited patterns**: Supports basic `use_backend` and ACL patterns

## Future Enhancements

- Cross-file reference resolution
- Hover information and documentation
- Auto-completion for backend/ACL names
- Full tree-sitter integration
- Support for more HAProxy directives

## Troubleshooting

### "Language server not found" error

The error means the LSP server binary isn't accessible. Try:

1. **Rebuild**: `./build.sh`
2. **Rebuild the extension in Zed**: `Cmd+Shift+P` → `zed: rebuild dev extension`
3. **Check binary**: `ls -la bin/haproxy-lsp`

### Extension not appearing

1. Restart Zed completely
2. Check extensions directory: `ls "$HOME/Library/Application Support/Zed/extensions/work/haproxy"`
3. Verify file associations in Zed settings

### Navigation not working

1. Ensure file is recognized as HAProxy config
2. Check the status bar shows "haproxy" language
3. Look for LSP server errors in Zed's developer console

## Contributing

1. Fork the repository
2. Make changes to `src/` files
3. Test with `./build.sh` then `zed: rebuild dev extension`
4. Submit pull request

## Credits

Tree-sitter parser from https://github.com/Ziehnert/tree-sitter-haproxy

## License

MIT License - see LICENSE file for details.
