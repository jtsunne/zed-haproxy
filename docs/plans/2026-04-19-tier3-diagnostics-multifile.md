# Tier 3 — Diagnostics & Multi-file Resolution

## Overview

Deliver Tier 3 of the roadmap as v0.4.0: catch config errors statically before running `haproxy -c`, and make the extension usable for projects whose configs are split across multiple files. Ships three LSP capabilities together:

- Diagnostics (`textDocument/publishDiagnostics`) — 7 rule types covering undefined/unused/duplicate references and missing `default_backend`.
- Cross-file resolution — follow `.if` / `.include` preprocessor directives plus `-f` / `crt` external file references; build a project-wide URI-keyed symbol index so `textDocument/definition`, `references`, `rename`, and diagnostics all work across files.
- Workspace symbols (`workspace/symbol`) — `Cmd+T` project-wide fuzzy symbol search.

Regex parser stays; tree-sitter migration is Tier 4. Config is opt-in via `.zed/haproxy.toml`.

## Context

- Files involved:
  - `src/lsp_server.rs` — add diagnostics engine, cross-file index builder, workspace/symbol handler, project-config loader, notification sender (new since current code only sends responses).
  - `src/lib.rs` — extend `language_server_command` initialization_options so LSP receives worktree-root hint from Zed.
  - `test/lsp_probes.py` — add `DIAGNOSTICS_PROBES`, `CROSSFILE_PROBES`, `WORKSPACE_SYMBOL_PROBES`.
  - `test/haproxy.conf` — augment with a broken backend ref (for diagnostics fixture) and a new `test/fragments/` subdir (for cross-file fixture).
  - `extension.toml` — bump `version` to `0.4.0`.
  - `README.md`, `CLAUDE.md` — document new providers, caches, config file schema.
- Related patterns:
  - `HaproxyLsp` caches (`symbols`, `folds`, `outline`, `documents`) are `HashMap<String, …>` keyed by URI — extend with a project-level index (`HashMap<ProjectRoot, ProjectIndex>`) layered on top.
  - `parse_document` is transactional — same discipline for the project index builder.
  - Notifications vs responses: today the LSP only emits responses; diagnostics need a new `send_notification` helper that writes to the same stdout framing (`Content-Length:` header) as the response writer.
  - All identifier resolution currently flows through `find_definition` / `find_references_to_symbol` at `src/lsp_server.rs` — plumb a `project_scope` option through those instead of duplicating logic.
- Dependencies: none new. Use `std::fs` for cross-file reads. No glob crate — implement a small glob matcher for `extra_files` patterns or require literal paths.

## Development Approach

- Testing approach: Regular (implementation first, integration probes in `test/lsp_probes.py`).
- Complete each task fully before moving to the next.
- CRITICAL: every task MUST include new/updated probes in `test/lsp_probes.py`.
- CRITICAL: all probes must pass (`python3 test/lsp_probes.py`) before starting the next task.
- Tier 1/2 probes must keep passing throughout — zero behavioral regression for existing handlers.
- Build verification after each code task: `./build.sh` must succeed.

## Implementation Steps

### Task 1: Notification infrastructure + diagnostics framework

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [x] add `send_notification(method, params)` helper that frames a JSON-RPC notification on stdout with `Content-Length:`; ensure it's interleave-safe with response writes
- [x] add `diagnostics: HashMap<String, Vec<Diagnostic>>` cache on `HaproxyLsp`
- [x] add `collect_diagnostics(uri)` entry point called at the end of `parse_document` (after caches are committed); it builds a fresh `Vec<Diagnostic>` and publishes via `textDocument/publishDiagnostics`
- [x] publish an empty diagnostics array for clean files so stale diagnostics are cleared
- [x] extend `test/lsp_probes.py` with a diagnostics listener that accumulates `publishDiagnostics` notifications between requests, plus a first probe asserting a clean file yields an empty diagnostics array
- [x] run `./build.sh && python3 test/lsp_probes.py` — all existing probes plus the new empty-diagnostics probe must pass before Task 2

### Task 2: Undefined-reference diagnostics (errors)

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/haproxy.conf`
- Modify: `test/lsp_probes.py`

- [x] implement diagnostic rules for undefined references: `use_backend X` / `default_backend X` where `X` is not a backend; `if X` / `unless X` where `X` is not a defined ACL (skip ACL expression keywords like `{`, `||`, `&&`); `use_server X` where `X` is not a server in the enclosing backend
- [x] each diagnostic: `severity: 1` (Error), `source: "haproxy-lsp"`, `code: "undefined-backend" | "undefined-acl" | "undefined-server"`, precise range on the identifier token
- [x] add fixture lines to `test/haproxy.conf` (inside a comment-guarded block) exercising all three undefined cases
- [x] add `DIAGNOSTICS_PROBES` entries asserting each undefined-ref diagnostic is emitted with correct code, severity, and range
- [x] run `./build.sh && python3 test/lsp_probes.py` — new probes pass, prior probes unchanged

### Task 3: Unused-symbol and duplicate/structural diagnostics (warnings + errors)

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/haproxy.conf`
- Modify: `test/lsp_probes.py`

- [x] implement unused-backend warning (`Symbol.references.is_empty()` for `SymbolKind::Backend`; skip backends used only as `default_backend` — those already count as references)
- [x] implement unused-ACL warning (ACL defined but never appears in `if`/`unless` within same section)
- [x] implement duplicate-section error (two `backend foo` / `frontend foo` / `listen foo` with same name)
- [x] implement duplicate-ACL-in-same-section error (two `acl foo …` inside one frontend/listen)
- [x] implement missing-`default_backend` warning (frontend/listen that has `bind` but neither a `default_backend` nor any `use_backend` directive)
- [x] extend fixture with examples for each rule
- [x] add probes asserting severity/code/range for each rule
- [x] run `./build.sh && python3 test/lsp_probes.py` — all pass

### Task 4: Project-root discovery + config loader

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `src/lib.rs`
- Create: `test/fragments/main.cfg`
- Create: `test/fragments/.zed/haproxy.toml`
- Create: `test/fragments/backends.cfg`
- Modify: `test/lsp_probes.py`

- [x] define `ProjectConfig { project_root: PathBuf, follow_includes: bool, extra_files: Vec<String> }`; default to `{ project_root: <dir of first opened file>, follow_includes: true, extra_files: [] }`
- [x] on `initialize`, read `initializationOptions.workspace_root` if provided
- [x] on `didOpen`, walk up from the file's directory looking for `.zed/haproxy.toml`; parse with a tiny hand-rolled TOML reader (only 3 keys — no crate dep)
- [x] add `src/lib.rs` change to pass `worktree.root_path()` as `initializationOptions.workspace_root` in `language_server_command`
- [x] create fragment fixture: `main.cfg` with `.include backends.cfg`, `backends.cfg` with a backend definition
- [x] add a probe loading the fragment fixture and asserting the discovered project root matches expectations (expose via a custom `$/haproxy/projectInfo` request for test introspection)
- [x] run `./build.sh && python3 test/lsp_probes.py`

### Task 5: Cross-file include graph + project index

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] parse `.include <path>`, `.if`/`.elif`/`.else`/`.endif` (follow all included branches conservatively), and `-f <path>` / `crt <path>` references during document parse; emit an `IncludedFiles` set per URI
- [ ] build `ProjectIndex { symbols_by_name: HashMap<(SymbolKind, String), Vec<(Uri, Range)>>, symbols_by_uri: HashMap<Uri, Vec<Symbol>> }`; populate lazily on first request and refresh when any member file changes
- [ ] on `didOpen`/`didChange` of any file in the graph, eagerly `parse_document` for every file in the graph (read from disk for unopened files) so the index stays coherent
- [ ] add probes asserting: opening `main.cfg` populates the project index with symbols from `backends.cfg`; changing `backends.cfg` on disk and sending `didChange` for `main.cfg` refreshes the index
- [ ] run `./build.sh && python3 test/lsp_probes.py`

### Task 6: Cross-file definition, declaration, references, rename

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] update `find_definition`, `find_declaration`, `references` handler, and `rename` handler to consult the project index (not just the per-URI `symbols` cache); return `Location`s with the correct URI for out-of-file hits
- [ ] `rename` returns a `WorkspaceEdit.changes` map keyed by every affected URI
- [ ] F12 on a literal path in `.include <path>` or `-f <path>` returns a `Location` pointing to `{ uri: <resolved-file>, range: {0,0}-{0,0} }`
- [ ] undefined-reference diagnostics now consult the project index — references resolved in another file are no longer flagged
- [ ] add cross-file probes: definition on a `use_backend X` in `main.cfg` lands in `backends.cfg`; rename of `X` from `main.cfg` produces `TextEdit`s for both files; F12 on the `.include` path navigates to `backends.cfg`
- [ ] run `./build.sh && python3 test/lsp_probes.py`

### Task 7: Workspace symbol provider

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] advertise `workspaceSymbolProvider: true` in `initialize`
- [ ] implement `workspace/symbol`: enumerate every symbol in the project index, case-insensitive substring match on `query`, return `SymbolInformation[]` with `name`, `kind`, `location`, and (optionally) `containerName` set to the enclosing backend/frontend
- [ ] empty query returns up to a reasonable cap (e.g. 1000) of symbols so Zed can stream
- [ ] add `WORKSPACE_SYMBOL_PROBES` asserting: `opcart` query from `test/haproxy.prod.cfg` returns `backend opcart-direct`; cross-file query from `test/fragments/` returns symbols from multiple files
- [ ] run `./build.sh && python3 test/lsp_probes.py`

### Task 8: Verify acceptance criteria

- [ ] red-underline latency: measure `didChange` → `publishDiagnostics` round-trip on a `test/haproxy.prod.cfg`-scale input; must be ≤200ms (add timing assertion to a probe)
- [ ] cross-file acceptance: from fixture, opening `main.cfg` discovers and indexes `backends.cfg`; a probe asserts this explicitly
- [ ] workspace symbol acceptance: `opcart` returns `backend opcart-direct` across the whole project
- [ ] run full `python3 test/lsp_probes.py` — every probe passes
- [ ] run `./build.sh` — clean build, no warnings regression

### Task 9: Update documentation and bump version

- [ ] bump `extension.toml` `version` to `0.4.0`
- [ ] update `README.md` with diagnostics rules table, cross-file config schema (`.zed/haproxy.toml`), and workspace symbol capability
- [ ] update `CLAUDE.md` architecture notes: new `diagnostics` cache, `ProjectIndex`, `ProjectConfig`, notification sender, cross-file resolution flow; list new advertised capabilities
- [ ] move `docs/plans/2026-04-19-tier3-diagnostics-multifile.md` to `docs/plans/completed/`
- [ ] update `docs/roadmap.md` to mark Tier 3 as shipped in v0.4.0 (mirror the Tier 2 note style)
