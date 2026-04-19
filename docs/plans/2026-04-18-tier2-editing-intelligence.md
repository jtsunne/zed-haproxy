# Tier 2 Editing Intelligence — HAProxy Zed Extension

## Overview

Deliver Tier 2 of the roadmap: make the extension actively helpful while typing, not just correct while reading. Four LSP capabilities plus one data-model extension, shipped together as v0.3.0:

- Completion (`textDocument/completion`) for backends, ACLs, servers, stick-tables, and directive names with context-aware triggering.
- Hover (`textDocument/hover`) showing backend summaries, ACL definitions, directive doc snippets, and server address details.
- References (`textDocument/references`) listing every call-site of a symbol under the cursor.
- Rename (`textDocument/rename`) with `prepareProvider` for safe single-file renames of backends, ACLs, servers, frontends, and listens.
- New `SymbolKind::StickTable` so completion inside `sc0_*`/`sc1_*` and `stick match`/`stick on table <name>` contexts resolves correctly.

Single-file scope retained from Tier 1. Cross-file resolution is explicitly deferred to Tier 3. Regex parser stays; tree-sitter migration is Tier 4.

## Context

- Files involved:
  - `src/lsp_server.rs` — add handlers, extend `SymbolKind`, extend `parse_document`, add word-boundary replacement for rename.
  - `src/docs.rs` (create) — curated directive docs table (~50 entries) used by hover and completion `documentation`.
  - `test/lsp_probes.py` — extend with `COMPLETION_PROBES`, `HOVER_PROBES`, `REFERENCES_PROBES`, `RENAME_PROBES`.
  - `test/haproxy.conf` — existing small fixture; add a stick-table definition so completion/references probes have coverage.
  - `extension.toml` — version bump to `0.3.0`.
  - `README.md` — document new capabilities.
  - `CLAUDE.md` — note new LSP providers and caches.
- Related patterns:
  - `HaproxyLsp` caches are URI-keyed and populated transactionally by `parse_document`. New data (stick-tables, parsed section ranges for rename scope) extends the same pattern.
  - Existing `Symbol.references: Vec<Reference>` already carries call-site ranges — references handler is a thin wrapper, and rename reuses the same data plus the definition range.
  - `find_definition` is cursor-aware with a keyword-walk-back algorithm; completion and hover reuse the same prefix tokenizer to decide context.
  - LSP capabilities advertised in `initialize` result at `src/lsp_server.rs:982-997`.
- Dependencies: none new. Continue using `serde_json::json!` for wire format. `src/docs.rs` is a plain Rust module with a `HashMap<&'static str, &'static str>`.

## Development Approach

- Testing approach: Regular (implementation first, then integration probes in `test/lsp_probes.py`).
- Complete each task fully before moving to the next.
- CRITICAL: every task MUST include new/updated tests in `test/lsp_probes.py`.
- CRITICAL: all probes must pass (`python3 test/lsp_probes.py`) before starting the next task.
- Maintain backward compatibility with Tier 1 capabilities — all existing definition, declaration, foldingRange, and documentSymbol probes must keep passing after every task.
- Feature work stays single-file scope. No cross-file indexing in this tier.

## Implementation Steps

### Task 1: Extend symbol model with StickTable and parse stick-table definitions

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/haproxy.conf` (add one `backend` with a `stick-table` directive)
- Modify: `test/lsp_probes.py`

- [x] add `SymbolKind::StickTable` variant and update `find_symbol_by_name` / `add_reference_to_symbol` discriminant checks so the existing symbol-lookup code treats it uniformly
- [x] extend `parse_document` to recognise `stick-table type ... ` inside `backend`/`frontend`/`listen` bodies; the table name is the enclosing section name (HAProxy binds one table per section), store a `Symbol` with `kind: StickTable` and range on the `stick-table` line
- [x] extend reference collection to record call-sites: `sc0_*(<name>)`, `sc1_*(<name>)`, `stick match <name>`, `stick store-request <name>`, `stick on ... table <name>` — add a new `ReferenceContext::StickTable`
- [x] add a stick-table fixture to `test/haproxy.conf` (one backend with `stick-table type ip size 1m expire 10s store http_req_rate(10s)` plus a reference from a frontend)
- [x] add definition probes for the stick-table name in both the definition line and one reference line
- [x] run `python3 test/lsp_probes.py` — all existing + new probes must PASS before Task 2 (3 pre-existing prod.cfg fold failures on HEAD are unchanged; all Tier 1 probes that were passing still pass, and all new Task 1 probes pass)

### Task 2: Advertise and implement references provider

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [x] advertise `referencesProvider: true` in the `initialize` capabilities block
- [x] add `textDocument/references` handler: reuse the cursor-aware keyword-walk-back from `find_definition` to resolve the symbol under the cursor, then return `Location[]` from `Symbol.references`; honor `context.includeDeclaration` by prepending the definition range when `true`
- [x] also handle the case of the cursor being on a definition line (re-use `find_declaration` logic) so references work from both sides
- [x] add `REFERENCES_PROBES` in `test/lsp_probes.py` covering: backend name on a `use_backend` line, ACL on a condition, stick-table in an `sc0_*(...)` call, includeDeclaration=true vs false
- [x] run `python3 test/lsp_probes.py` — full suite must PASS before Task 3 (pre-existing 3 prod.cfg fold failures unchanged; all 9 new references probes plus all prior passing probes PASS)

### Task 3: Implement rename with prepareProvider

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] advertise `renameProvider: { prepareProvider: true }` in `initialize`
- [ ] implement `textDocument/prepareRename`: resolve the symbol at cursor (backend, ACL, server, frontend, listen); return the exact identifier range so Zed pre-fills the rename box, or `null` when the cursor is not on a renameable token
- [ ] implement `textDocument/rename`: build a `WorkspaceEdit` with `changes: { uri: TextEdit[] }` containing one edit for the definition identifier range and one per reference; compute identifier ranges by locating the symbol token inside the cached line text (use the existing `search_from` pattern to avoid matching keywords like `backend` substring)
- [ ] reject invalid new names (empty, whitespace, containing characters outside `[a-zA-Z0-9_.-]`) by returning a JSON-RPC error with code `-32602` and a message Zed will surface
- [ ] stick-tables are intentionally NOT renameable this tier (they are bound to the enclosing section name — renaming the section handles it)
- [ ] add `RENAME_PROBES` covering: prepareRename returns correct range on a backend name, prepareRename returns null on a keyword, rename of a backend updates definition + every `use_backend`/`default_backend` reference, rename of an ACL updates definition + every `if`/`unless` reference, rename with invalid name returns an error
- [ ] run `python3 test/lsp_probes.py` — full suite must PASS before Task 4

### Task 4: Implement hover provider with directive docs

**Files:**
- Create: `src/docs.rs`
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] create `src/docs.rs` with `pub fn directive_doc(name: &str) -> Option<&'static str>` backed by a `HashMap<&'static str, &'static str>` (~50 curated entries covering `bind`, `server`, `acl`, `use_backend`, `default_backend`, `mode`, `balance`, `timeout`, `option`, `http-request`, `http-response`, `tcp-request`, `stick-table`, `stick`, `stats`, `redirect`, `use_server`, `log`, `maxconn`, `retries`, `cookie`, `http-check`, `tcp-check`, `default-server`, `resolvers`, `nameserver`, `peers`, `peer`, `userlist`, `user`, `group`, `cache`, `listen`, `frontend`, `backend`, `global`, `defaults`, `compression`, `http-reuse`, `errorfile`, `description`, `bind-process`, `monitor-uri`, `rate-limit`, `filter`, `capture`, `http-after-response`, `use-service`, `http-send-name-header`, `hash-type`); each entry is plain markdown (bullet syntax + common flags) kept under 10 lines
- [ ] add `textDocument/hover` handler returning `MarkupContent { kind: "markdown", value: ... }`; resolution order inside the handler:
  1. Word is a defined backend name → show definition line, `mode`, `balance`, and up to 5 server lines with `"…N more"` truncation
  2. Word is a defined ACL name → show the ACL's definition line
  3. Word is a defined stick-table → show the table type + store clauses
  4. Word is a defined server address context → show `host:port` plus flags (`check`, `backup`, `weight N`)
  5. Word is a known directive name (first token on the line and in the docs table) → show the docs snippet
  6. Otherwise → return `null` (Zed renders nothing)
- [ ] advertise `hoverProvider: true` in `initialize`
- [ ] add `HOVER_PROBES` covering each of the six resolution paths, asserting the `MarkupContent.value` contains expected substrings (backend name, server list, directive syntax keyword, etc.)
- [ ] run `python3 test/lsp_probes.py` — full suite must PASS before Task 5

### Task 5: Implement context-aware completion

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [ ] advertise `completionProvider: { triggerCharacters: [" ", "("], resolveProvider: false }` in `initialize`
- [ ] add `textDocument/completion` handler; context detection reuses the prefix-token walk from `find_definition` and the cursor line text:
  - After `use_backend`/`default_backend` → backend names (kind=7 Class)
  - After `if`/`unless`/`!` in a condition → ACL names (kind=21 Constant)
  - After `use_server` → server names scoped to the enclosing backend (walk up the line index to find the owning `backend` section header)
  - Inside `sc0_*(`, `sc1_*(`, `stick match `, `stick store-request ` → stick-table names
  - Start of line inside a known section → hard-coded per-section directive allowlist (keys pulled from `src/docs.rs`)
- [ ] each `CompletionItem` carries `label`, `kind`, `detail` (e.g. backend mode + balance, ACL criterion summary), `documentation` (from `src/docs.rs` for directive completions, from the symbol's surrounding line for identifier completions), and `sortText` derived from in-file usage frequency (count references to rank popular symbols higher)
- [ ] rank by frequency of use in the current file; stable secondary sort by alphabetical name
- [ ] add `COMPLETION_PROBES` covering each of the five contexts on `test/haproxy.conf`; each probe asserts a minimum set of expected labels is present (not an exact-equality check, since directive allowlists may evolve); include one probe asserting ≥5 backend names under a `use_backend ` prefix on the real fixture `test/haproxy.prod.cfg`
- [ ] run `python3 test/lsp_probes.py` — full suite must PASS before Task 6

### Task 6: Verify acceptance, update docs, bump version

**Files:**
- Modify: `extension.toml`
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `docs/roadmap.md`

- [ ] run `./build.sh` — the LSP binary must build clean with zero warnings for new code paths
- [ ] run `python3 test/lsp_probes.py` — full suite (definition + declaration + folding + documentSymbol + references + rename + hover + completion) must PASS
- [ ] bump `version` in `extension.toml` to `0.3.0`
- [ ] update `README.md` with a Tier 2 capabilities section (completion, hover, references, rename) and a short demo example for each
- [ ] update `CLAUDE.md` to list the new advertised capabilities, the new `StickTable` symbol kind, and the `src/docs.rs` docs table pattern
- [ ] update `docs/roadmap.md`: mark Tier 2 as shipped; move this plan into `docs/plans/completed/` after manual Zed verification

## Post-Completion

- Manual Zed verification on `test/haproxy.conf` and `test/haproxy.prod.cfg`: completion triggers after `use_backend `, hover on `use_backend opcart-direct` shows server list, rename of `opcart-direct` updates every call-site atomically, references panel lists every use.
- Rebuild the dev extension in Zed (`Cmd+Shift+P → zed: rebuild dev extension`) and smoke-test in the editor UI.
- Tag release `v0.3.0` and publish to the Zed extension marketplace.
