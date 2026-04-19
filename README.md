# HAProxy Zed Extension with Go to Definition

A Zed editor extension that provides syntax highlighting and **"Go to Definition"** functionality for HAProxy configuration files.

## Features

- **Semantic Syntax Highlighting**: Directives are colored by category — section headers (`frontend`, `backend`, `listen`, `resolvers`, etc.) vs. identifier names, declaration directives (`bind`, `server`, `acl`, `peer`, `stick-table`) vs. action directives (`use_backend`, `default_backend`, `http-request`, `http-response`, `tcp-request`, `redirect`), settings directives (`timeout`, `maxconn`, `balance`, `stats`, `log`, etc.), option properties (`option httplog` → `httplog` as a property), enumerated types (`mode http`, `balance roundrobin`, `log-level info`), control keywords (`if`, `unless`, `!`, `||`, `&&`, and the TCP phase selectors `connection`/`content`/`session`), and value kinds (addresses, strings, numbers, time values). The coarse `@keyword`/`@function` coloring from older versions is gone.
- **Code Folding**: Top-level sections collapse to one line (`global`, `defaults`, `frontend X`, `backend X`, `listen X`, `resolvers X`, `userlist`, `peers`, `mailers`, `cache`, `program`, `ring`). Multi-line comment banners (2+ consecutive `#`-prefixed lines) fold as comment regions. Paired `# BEGIN <name>` / `# END <name>` markers fold as explicit regions — a useful convention for grouping related directives inside a long section. Use `editor: fold all` / `Cmd+K Cmd+0` to collapse, `editor: unfold all` / `Cmd+K Cmd+J` to expand.
- **Document Outline & Breadcrumbs**: The outline panel and breadcrumbs show a two-level tree — each section with its key children (ACLs under frontends/listens, servers under backends/listens, nameservers under resolvers). Section detail strings summarize relevant settings (e.g. a backend shows `<balance> · <mode> · N servers`, a resolvers block shows `N nameservers`, a frontend shows its `bind` addresses). `Cmd+Shift+O` jumps to any symbol by name; the selection range covers the identifier only, not the whole line.
- **Go to Definition**: Cursor-aware — F12 on an ACL name inside `use_backend X if Y` jumps to the ACL, not to backend X. Works for backend, frontend, listen, ACL, server, and stick-table references within the current file.
- **Find References**: `editor: find all references` (Shift+F12) lists every call-site of the symbol under the cursor — backends used by `use_backend`/`default_backend`, ACLs in `if`/`unless` conditions, stick-tables referenced by `sc0_*(...)` / `stick match` / `stick on ... table`. Works from both the definition and any reference line; toggle `includeDeclaration` to include the definition in results.
- **Rename Symbol**: `editor: rename` (F2) renames a backend, ACL, server, frontend, or listen and updates every call-site in the file atomically. `prepareProvider` pre-fills the rename box with the current identifier; invalid names (empty, whitespace, or any character outside `[a-zA-Z0-9_.-]`) are rejected with a JSON-RPC error. Stick-tables are renamed by renaming their enclosing section (HAProxy binds one table per section).
- **Hover**: Cursor on a backend name shows the definition line, `mode`, `balance`, and the first 5 server lines (`"…N more"` truncation). Cursor on an ACL shows the definition line. Cursor on a stick-table shows the type and store clauses. Cursor on a known directive (`bind`, `server`, `acl`, `http-request`, `stick-table`, …) shows a curated markdown snippet from `src/docs.rs`.
- **Completion**: Context-aware. After `use_backend `/`default_backend ` → backend names. After `if `/`unless `/`! ` → ACL names. After `use_server ` → server names from the enclosing backend only. Inside `sc0_*(`, `sc1_*(`, `stick match `, `stick store-request ` → stick-table names. Start-of-line inside a section → the per-section directive allowlist. Items carry `detail` (e.g. backend mode + balance), `documentation` from `src/docs.rs`, and rank by in-file usage frequency (popular symbols float to the top) with alphabetical tie-break.
- **Diagnostics**: Static analysis runs on every `didOpen`/`didChange` and publishes `textDocument/publishDiagnostics`. Catches typos and dead code before `haproxy -c`. See the [Diagnostics rules](#diagnostics-rules) table below.
- **Cross-file Resolution**: Follows `.include`, `.if`/`.elif`/`.else`/`.endif`, and `-f <path>` / `crt <path>` references to build a project-wide symbol index. Definition, declaration, references, rename, and diagnostics all resolve across files. F12 on the literal path in `.include <path>` navigates to the included file. Opt-in via `.zed/haproxy.toml` (see [Project configuration](#project-configuration) below).
- **Workspace Symbols**: `Cmd+T` fuzzy-searches every backend, frontend, listen, ACL, server, and stick-table across every indexed file in the project. Case-insensitive substring match; empty query streams up to 1000 symbols.
- **Language Server Protocol**: `foldingRangeProvider`, `documentSymbolProvider`, `definitionProvider`, `declarationProvider`, `referencesProvider`, `renameProvider` (with `prepareProvider`), `hoverProvider`, `completionProvider`, `workspaceSymbolProvider`. Cross-file scope for navigation, diagnostics, and rename when a project root is discovered; falls back to single-file scope otherwise.

### Diagnostics rules

| Rule | Severity | Code | Example |
|---|---|---|---|
| Undefined backend reference | Error | `undefined-backend` | `use_backend nope` when no `backend nope` exists |
| Undefined ACL reference | Error | `undefined-acl` | `use_backend foo if undefined_acl` |
| Undefined server in `use_server` | Error | `undefined-server` | `use_server nope if …` |
| Unused backend | Warning | `unused-backend` | `backend orphan` never referenced |
| Unused ACL | Warning | `unused-acl` | `acl orphan src 1.2.3.4` never referenced |
| Duplicate section name | Error | `duplicate-section` | two `backend foo` sections |
| Duplicate ACL in same section | Error | `duplicate-acl` | two `acl foo …` in one frontend/listen |
| Missing `default_backend` | Warning | `missing-default-backend` | frontend/listen with `bind` but no `default_backend`/`use_backend` |

All diagnostics include `source: "haproxy-lsp"` and a precise range on the offending identifier. Cross-file references are not flagged — if a backend is defined in `backends.cfg` and referenced from `main.cfg`, both files need to be in the same project index (see next section).

### Project configuration

For configs split across multiple files, drop a `.zed/haproxy.toml` at the project root:

```toml
[haproxy]
project_root = "."              # relative to the .zed/ parent, or absolute
follow_includes = true          # follow .include, .if/.elif/.else/.endif, -f, crt
extra_files = ["conf.d/*.cfg"]  # literal paths or simple glob patterns
```

Discovery walks up from the opened file's directory looking for `.zed/haproxy.toml`. If none is found, the LSP defaults to `{ project_root: <dir of first opened file>, follow_includes: true, extra_files: [] }` — so most single-directory setups work without any config file. Only three keys are recognized; other TOML content is ignored.

### Supported Navigation

- **Backend References**: `use_backend web_servers` → jumps to `backend web_servers`
- **Default Backend**: `default_backend api` → jumps to `backend api`
- **ACL References**: `if is_mobile` → jumps to `acl is_mobile`
- **Stick-table References**: `sc0_http_req_rate(tbl)` → jumps to the backend defining `stick-table type ... size ... store ...`

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
2. Available actions on symbols (backends, frontends, listens, ACLs, servers, stick-tables):
   - **F12** / Go to Definition — backend names in `use_backend`/`default_backend`, ACL names in `if`/`unless` conditions, stick-table names in `sc0_*(...)`/`stick match`/`stick on ... table`
   - **Shift+F12** / Find All References — every call-site of the symbol under the cursor
   - **F2** / Rename Symbol — backends, ACLs, servers, frontends, listens (stick-tables rename via their enclosing section)
   - **Hover** — summaries for symbols and curated markdown docs for known directives
   - **Completion** — context-aware suggestions after ` ` (space) or `(`, scoped by surrounding keyword

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

### Tier 2 capabilities in action

Completion — typing `use_backend ` shows every defined backend in the file, ranked by usage frequency:

```haproxy
frontend main
  use_backend web_servers     # start typing after the space
  #             ^ completion lists: web_servers, api, static, …
```

Hover — F1 / cursor rest on a backend name shows a summary:

```haproxy
use_backend api
#            ^ hover shows:
#              backend api
#              mode http
#              balance roundrobin
#              server api1 10.0.0.1:8080 check
#              server api2 10.0.0.2:8080 check
```

References — Shift+F12 on a backend name lists every `use_backend` / `default_backend` referencing it. Toggle `includeDeclaration` to also show the `backend <name>` line itself.

Rename — F2 on `api` renames the backend and every call-site atomically in one edit. Invalid names (`foo bar`, `bad@name`, empty) surface as a JSON-RPC error.

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

# Regression check — drives the LSP over stdio and asserts
# definition, declaration, folding, documentSymbol, references,
# rename, hover, and completion responses against fixtures.
python3 test/lsp_probes.py
```

### Project Structure

```
haproxy-zed/
├── src/
│   ├── lib.rs           # Extension entry point
│   ├── lsp_server.rs    # LSP server implementation
│   └── docs.rs          # Curated directive docs (used by hover + completion)
├── languages/haproxy/   # Language configuration
├── grammars/           # Tree-sitter grammar
├── extension.toml      # Extension metadata
└── build.sh           # Build script
```

## Known Limitations

- **Folding and outline are single-file**: Cross-file resolution covers definition, references, rename, diagnostics, and workspace symbols, but folding and outline are computed per-open-document.
- **Regex-based LSP parsing**: The LSP analyzes configs line-by-line with regex rather than a full tree-sitter AST. Works well for the directive set it supports, but complex quoted-string edge cases may be missed. Tree-sitter migration is planned.

## Future Enhancements

- `haproxy -c` integration for real syntax errors
- Code actions (quick-fixes for diagnostics)
- Config formatter
- Full tree-sitter integration in the LSP

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
