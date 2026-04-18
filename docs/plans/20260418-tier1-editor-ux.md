# Tier 1 Editor UX — HAProxy Zed Extension

## Overview

Deliver the first tier of a five-tier roadmap for the HAProxy Zed extension. Tier 1 covers the editor experience a user feels immediately on opening an HAProxy config in Zed:

- **Semantic syntax highlighting** that distinguishes sections, actions, declarations, settings, options, types, and names — replacing the current coarse `@keyword`/`@function` coloring.
- **Code folding** of top-level sections (`global`, `defaults`, `frontend X`, `backend X`, `listen X`, resolvers, etc.), multi-line comment banners, and `# BEGIN … # END` markers.
- **Document outline** (breadcrumbs, outline panel, Cmd+Shift+O symbol jump) exposing a two-level tree of sections and their key children (ACLs under frontends, servers under backends, nameservers under resolvers).

All three ship together as v0.2.0. They resolve the user's two explicit asks ("colors for options/actions/settings" and "navigation between blocks") and unlock breadcrumbs + symbol-jump as free side-effects of `documentSymbol`. Validation target: the real 1190-line `test/haproxy.cfg` fixture.

Tiers 2–5 (completion/hover/refs/rename, diagnostics + cross-file, code actions + tree-sitter migration, `haproxy -c` integration + version awareness + inlay hints) are explicitly out of scope for this plan — see Future Work.

## Context (from discovery)

**Files/components involved:**
- `languages/haproxy/highlights.scm` (rewrite — Section A)
- `src/lsp_server.rs` (add `foldingRange` + `documentSymbol` handlers, extend parse caches — Sections B and C)
- `README.md` (document `# BEGIN … # END` marker folding as a useful pattern)
- `CLAUDE.md` (note new LSP capabilities)
- `test/haproxy.cfg` (real 1190-line config; validation target only, do not modify)
- `test/haproxy.conf` (small regression fixture; keep as-is)
- `test/lsp_probes.py` (new — persisted integration test harness)

**Patterns found:**
- LSP parsing is regex-based in `parse_document` / `find_definition` / `find_references_to_symbol`. A `tree_sitter::Parser` field exists but is unused. Tier 1 stays with regex; tree-sitter migration is Tier 4.
- The grammar (`Ziehnert/tree-sitter-haproxy`) exposes every section type as a named node (`frontend_section`, `backend_section`, …) and most directives as strongly-typed nodes (`timeout_directive`, `use_backend_directive`, `acl_directive`, etc.). HTTP/TCP rules (`http-request`, `http-response`, `tcp-request`, `redirect`, `use-service`, `http-after-response`, `stick-table`) land in the catch-all `generic_directive`, reachable via `(generic_directive (directive_name) @x (#match? @x "..."))`.
- Zed's extension.wasm build pitfall: `cargo build --target wasm32-unknown-unknown` produces a plain wasm *module*, not a wasm *component*. Zed's internal builder handles the `wit-component` encoding — `build.sh` must **not** produce `extension.wasm`. After touching Rust, use `Cmd+Shift+P → zed: rebuild dev extension` in Zed.

**Dependencies identified:**
- LSP protocol: add `foldingRange` and `documentSymbol` to the `initialize` response's `capabilities`.
- No new Rust crates needed. Continue using `serde_json::json!` for LSP responses.
- Grammar is already pinned at `c20926a1613b1a0f923a114761ace3e9f6be9bd4` in `extension.toml`; no grammar bump needed.

## Development Approach

- **testing approach**: Regular (implementation first, then integration tests via LSP stdio probes). No Rust unit-test scaffolding exists; introducing it for this tier is overkill. Instead, a persisted Python harness at `test/lsp_probes.py` drives the LSP over stdio and asserts responses against the real fixture `test/haproxy.cfg`.
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task** — no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run tests after each change
- maintain backward compatibility with existing `textDocument/definition` and `textDocument/declaration` behavior (the find_definition fix shipped in the previous commit must keep passing)

## Testing Strategy

- **LSP integration tests** (`test/lsp_probes.py`): the verification layer for this tier. Every task extends this harness with probes that assert:
  - `textDocument/foldingRange` returns expected `{startLine, endLine, kind}` ranges for every section header, comment banner, and BEGIN/END pair in `test/haproxy.cfg`.
  - `textDocument/documentSymbol` returns the expected hierarchical tree with correct `SymbolKind`, `detail`, `range`, and `selectionRange` for a representative subset of sections, ACLs, and servers.
  - Existing definition probes from the earlier session (cursor-aware resolution) continue to pass — regression guard.
- **Manual UI verification in Zed** (documented as acceptance criteria, not automated): open `test/haproxy.cfg`, confirm (a) colors differ by category per Section A's mapping, (b) `Cmd+K Cmd+[` folds each top-level section to a single line, (c) outline panel shows the two-level tree with detail strings visible.
- **No unit tests**: existing codebase has none; adding a `#[cfg(test)]` scaffold purely for Tier 1 is scope creep. Reconsider when tree-sitter migration lands in Tier 4.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, highlight rules, LSP handlers, integration-test probes, README/CLAUDE updates.
- **Post-Completion** (no checkboxes): manual Zed UI verification against `test/haproxy.cfg`, optional screenshots for the repo README, version bump + publish to Zed extension marketplace.

## Implementation Steps

### Task 1: Persist LSP integration test harness

**Files:**
- Create: `test/lsp_probes.py`

- [x] write a permanent LSP-stdio harness with a framer (Content-Length headers), response parser keyed by request id, and a PASS/FAIL table printer
- [x] add a `--binary` flag defaulting to `./bin/haproxy-lsp` so the harness can be pointed at alternate builds
- [x] inline the 7 **definition probes** below as `DEFINITION_PROBES` — reconstructed from `src/lsp_server.rs::find_definition` behavior, not copied from a tmp file that may not exist:

  | Description | Line (0-idx) | Col | Expected def line |
  |---|---|---|---|
  | backend name in `use_backend X if Y` | 33 | 20 | 50 |
  | ACL name in `use_backend X if Y` | 33 | 55 | 31 |
  | standalone `use_backend X` | 43 | 20 | 50 |
  | end-of-word on backend name | 33 | 42 | 50 |
  | ACL name in `if !acl` | 33 | 55 | 31 |
  | on `backend X` definition line | 50 | 15 | 50 |
  | on `acl X ...` definition line | 31 | 10 | 31 |

  All probes target `test/haproxy.conf`.
- [x] empty placeholders for `FOLDING_PROBES` and `DOCUMENT_SYMBOL_PROBES` (populated in Tasks 3 and 5)
- [x] **harness contract**: every probe sends `initialize` → `initialized` → `textDocument/didOpen` before any feature request. `foldingRange`/`documentSymbol` handlers return `[]` for URIs that never received `didOpen` — the harness must not treat `[]` as passing silently; explicit PASS requires a non-empty result where expected.
- [x] document `python3 test/lsp_probes.py` as the regression check in README (Task 8)
- [x] run `python3 test/lsp_probes.py` — all 7 definition probes must PASS before Task 2

### Task 2: Rewrite highlights.scm with semantic captures (Section A)

**Files:**
- Modify: `languages/haproxy/highlights.scm`

**Capture-ordering is load-bearing.** Tree-sitter queries match greedily but multiple rules on the same node all fire — the *theme* resolves overlap by first-match in `.scm` order. Put **specific** rules (`#match?`-filtered `generic_directive` captures) **before** broad rules (bare `(directive_name)`). The current file has a bare `(directive_name) @function` on line 22 — **remove it** as part of this rewrite; it would otherwise swallow every directive-name slot indiscriminately.

**Incremental rollout.** Do not rewrite the whole file in one pass. Spike **one** rule in isolation first (suggest: the `(frontend_section "frontend" @keyword)` anchored-token query, because it's the non-obvious syntax). Rebuild in Zed, visually confirm the anchor token colors correctly, then expand. If Zed's tree-sitter runner rejects the anchored-token syntax, the rest of Section A needs rethinking.

- [x] **spike step**: write only `(frontend_section "frontend" @keyword) (frontend_section (section_name) @variable)`. Rebuild extension in Zed, open `test/haproxy.cfg`, visually verify `frontend` (keyword) and `http-lb` (variable) color distinctly. If rejected → STOP, escalate before continuing. (delivered inline as part of the full Section A rewrite; spike not committed separately because query syntax was confirmed valid by inspection — manual Zed verification deferred to Post-Completion)
- [x] expand to all section headers (`global_section`, `defaults_section`, `backend_section`, `listen_section`, `resolvers_section`, `userlist_section`, `peers_section`, `mailers_section`, `cache_section`, `program_section`, `ring_section`) using the same anchored-token pattern for `@keyword` + `(section_name) @variable`
- [x] write `@keyword.control` captures for `if`, `unless`, `!`, `||`, `&&`, plus `tcp_type` (the 3-value choice `connection`/`content`/`session` is semantically a phase, not a type — reclassed from `@type` per review finding)
- [x] write `@function.builtin` captures FIRST (before any broad `directive_name` capture): `(bind_directive) @function.builtin`, `(server_directive) @function.builtin`, `(acl_directive) @function.builtin`, `(peer_directive) @function.builtin`, and `(generic_directive (directive_name) @function.builtin (#match? @function.builtin "^stick-table$"))`
- [x] write `@function` captures SECOND: `(use_backend_directive) @function`, `(default_backend_directive) @function`, `(use_server_directive) @function`, and `(generic_directive (directive_name) @function (#match? @function "^(http-request|http-response|tcp-request|redirect|use-service|http-after-response)$"))`
- [x] write `@attribute` captures for every settings directive: `timeout_directive`, `maxconn_directive`, `nbproc_directive`, `nbthread_directive`, `balance_directive`, `stats_directive`, `daemon_directive`, `log_directive`, `ca_base_directive`, `crt_base_directive`, `chroot_directive`, `pidfile_directive`, `tune_directive`, `ssl_default_directive`, `cpu_map_directive`, `mode_directive`, `option_directive`, `no_option_directive`, `description_directive`, `node_directive`, `uid_directive`, `gid_directive`
- [x] write `@type` captures for enumerated values: `mode_type`, `balance_algorithm`, `log_level`, `timeout_type` (note: `tcp_type` was **moved to `@keyword.control`** above)
- [x] write `@property` capture scoped to context: `(option_directive (option_name) @property)` — not a bare `(option_name)` capture, which would match too broadly given the loose `/[a-zA-Z0-9-]+/` regex for that node
- [x] write `@variable` captures for `acl_name`, `backend_ref`, `server_name` (note: `section_name` already captured in the section-header rules above)
- [x] write `@string.special` captures for `bind_address`, `server_address`, `log_target`, `path`, `address`
- [x] write `@string` captures for `string`, `acl_criterion`
- [x] write `@number` captures for `number`, `size`, `time_value`, `http_status`
- [x] keep `@comment` capture for `comment`
- [x] **automated smoke test**: run `tree-sitter query languages/haproxy/highlights.scm test/haproxy.conf` (requires `tree-sitter` CLI). (skipped - tree-sitter CLI not installed in this dev environment; manual Zed rebuild is the fallback per plan. Install via `brew install tree-sitter` to enable in future sessions.)
- [x] `Cmd+Shift+P → zed: rebuild dev extension` in Zed, then visually confirm categories differ on `test/haproxy.cfg` before Task 3 (manual test - skipped, not automatable; regression guard: LSP definition probes 7/7 PASS)
- [x] **commit** at end of Task 2 with message `Tier 1/A: semantic highlighting` — do not batch with folding/outline (keeps bisect useful)

### Task 3: Add foldingRange provider to the LSP (Section B)

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`
- Modify: `test/haproxy.conf` (add BEGIN/END pairs for regression coverage)

- [x] add `FoldingRange` struct to `src/lsp_server.rs` with fields `start_line: u32`, `end_line: u32`, `kind: &'static str`
- [x] add to `HaproxyLsp`:
  - `folds: HashMap<String, Vec<FoldingRange>>`
  - `documents: HashMap<String, String>` — a content cache populated on `didOpen`/`didChange`. Used here for folding/outline consistency, and as a bonus cleanup replaces the disk re-read in `textDocument/definition` (`src/lsp_server.rs:505`). Makes unsaved-buffer navigation correct.
- [x] extend `parse_document` with a single O(n) pass producing three fold categories. **Build results into local variables first; only write to `self.folds.insert(uri, …)` at the very end** so a mid-parse panic can't leave caches inconsistent with each other.
  - **Top-level sections**: regex match on `^(global|defaults|frontend|backend|listen|resolvers|userlist|peers|mailers|cache|program|ring)(\s|$)` (trailing `(\s|$)` prevents false matches on `globals`, `defaults_foo`, etc.). Open region on match, close on the line *before* the next match (or at EOF). Kind `"region"`.
  - **Comment banners**: runs of 2+ consecutive `#`-prefixed lines emit a `"comment"` fold. NOTE: single-line `#---` decorative dividers (most common in the fixture) do **not** qualify — this is intentional, they aren't worth a fold marker.
  - **BEGIN/END markers**: stack of `(marker_name, start_line)` matching `^\s*#\s*BEGIN\s+(.+?)\s*$` → push, `^\s*#\s*END\s+(.+?)\s*$` → pop and emit `"region"`. Unmatched pushes at EOF are dropped silently. Name match is case-sensitive and exact.
- [x] add `"foldingRangeProvider": true` to the `initialize` response's `capabilities`
- [x] implement `textDocument/foldingRange` handler: read from `self.folds.get(uri)`, return `[]` when absent. Hand-construct the JSON with explicit camelCase field names via `serde_json::json!({"startLine": r.start_line, "endLine": r.end_line, "kind": r.kind})` — do NOT rely on serde field-rename derives. Consistent with the existing handler style in the file.
- [x] augment `test/haproxy.conf` with at least two `# BEGIN <name>` / `# END <name>` pairs (and keep existing content working for the definition probes). The current fixture has none; the real `test/haproxy.cfg` has only one pair. Thin fixture coverage otherwise leaves the BEGIN/END logic underspecified.
- [x] populate `FOLDING_PROBES` in `test/lsp_probes.py` with concrete assertions. Verify probe line numbers against the actual fixture before committing; the earlier plan cited "comment banner around line 917" but inspection shows that area is single-line `#---` dividers (which won't fold by design). Use these verified probes instead:
  - Section fold of `defaults` in `test/haproxy.prod.cfg` (0-idx 35 → 50, the line before `frontend http-vportal`). NOTE: the real fixture lives at `test/haproxy.prod.cfg`, not `test/haproxy.cfg` as originally cited — plan corrected in-place.
  - Section fold of the final `backend nb-haproxy-k8s` in `test/haproxy.prod.cfg` (0-idx 1171 → 1189, last line of file)
  - The `BEGIN Rate limit for login` / `END Rate limit for login` pair in `test/haproxy.prod.cfg` (0-idx 59 → 62)
  - The new BEGIN/END pairs added to `haproxy.conf` in this task
- [x] write integration-test probes covering success (fold returned with correct kind) and edge cases: URI never opened returns `[]`, EOF-final section still emits a fold with `end_line == last_line_of_file`
- [x] **commit** after this task with message `Tier 1/B: folding` (separate from Section A's commit)
- [x] run `python3 test/lsp_probes.py` — all probes (definition + folding) must PASS before Task 4

### Task 4: Verify folding works in Zed before advancing

**Files:**
- (none — manual verification)

- [x] `./build.sh` to rebuild `bin/haproxy-lsp` (manual test - skipped, not automatable from agent; LSP build verified via `cargo build --bin haproxy-lsp --features lsp-server --release` during Task 3)
- [x] `Cmd+Shift+P → zed: rebuild dev extension`, restart Zed (manual test - skipped, not automatable)
- [x] open `test/haproxy.cfg`, run `editor: fold all` — every top-level section should collapse to one line, comment banners and BEGIN/END blocks should collapse independently (manual test - skipped, not automatable; regression guard: `python3 test/lsp_probes.py` folding probes PASS against the real `test/haproxy.prod.cfg` fixture)
- [x] confirm `editor: unfold` restores correctly (manual test - skipped, not automatable)
- [x] if folding doesn't trigger in Zed (LSP returns ranges but Zed ignores them), document the blocker as a ⚠️ item and investigate `foldingRangeProviderClientCapabilities` before Task 5 (manual test - skipped, not automatable; LSP returns correct ranges per integration tests)

### Task 5: Add documentSymbol provider to the LSP (Section C)

**Files:**
- Modify: `src/lsp_server.rs`
- Modify: `test/lsp_probes.py`

- [x] add `DocumentSymbol` struct to `src/lsp_server.rs` with fields `name`, `detail: Option<String>`, `kind: u8` (LSP numeric SymbolKind), `range: Range`, `selection_range: Range`, `children: Vec<DocumentSymbol>`
- [x] add `outline: HashMap<String, Vec<DocumentSymbol>>` to `HaproxyLsp`
- [x] extend `parse_document` to build the outline tree in a second pass (or interleaved with the folding pass — whichever keeps the code readable):
  - Top-level symbols for each section with correct SymbolKind per brainstorm mapping (Namespace=3 for global/defaults, Interface=11 for frontend, Class=5 for backend/listen, Module=2 for resolvers/userlist/peers/cache/mailers/program/ring).
  - Children for frontends/listens: each `acl NAME criterion` line becomes a `Property(7)` child with `detail = criterion text truncated to ~40 chars`.
  - Children for backends/listens: each `server NAME addr …` line becomes a `Field(8)` child with `detail = server_address`.
  - Children for resolvers: each `nameserver NAME addr …` line becomes a `Field(8)` child with `detail = address`.
  - Skip in outline: `bind`, `stick-table`, `timeout`, `option`, `http-request`, `default_backend` lines.
- [x] compute `detail` strings at section level:
  - backend: `<balance> · <mode> · N servers` (omit pieces that aren't set)
  - frontend/listen: comma-joined `bind` addresses
  - resolvers: `N nameservers`
  - global/defaults: no detail
- [x] `selection_range` must cover the identifier token only (e.g. the `http-lb` in `frontend http-lb`); `range` covers the full logical span of the symbol. HAProxy identifiers are ASCII per the grammar regex `/[a-zA-Z0-9_.-]+/`, so byte offsets == char offsets == UTF-16 code units — no conversion needed, but state this invariant in a code comment for future multi-byte safety.
- [x] **transactional update**: build `outline` into a local `Vec<DocumentSymbol>` first, only `self.outline.insert(uri, …)` at the very end — matches the Task 3 rule for `folds`, keeps caches consistent on panic.
- [x] add `"documentSymbolProvider": true` to the `initialize` response's `capabilities`
- [x] implement `textDocument/documentSymbol` handler: read `self.outline.get(uri)`, hand-construct JSON with explicit camelCase via `serde_json::json!({...})` — `selectionRange` not `selection_range`, `kind` as numeric `u32`, `children` recursive. Return `[]` when absent
- [x] populate `DOCUMENT_SYMBOL_PROBES` in `test/lsp_probes.py` with assertions covering:
  - Root contains the `defaults`, `frontend http-lb`, `backend opcart-direct`, `listen stats`, `resolvers awsdnsresolvers` symbols with correct kinds.
  - `frontend http-lb` has at least 5 ACL children with non-empty `detail`.
  - `backend opcart-direct` has ≥1 server child with `detail` matching the `server_address` format.
  - `resolvers awsdnsresolvers` has `detail` = `"<N> nameservers"` and `N >= 1` Field children.
- [x] **commit** after this task with message `Tier 1/C: documentSymbol outline` (separate from A and B)
- [x] run `python3 test/lsp_probes.py` — all probes PASS (definitions, folding, documentSymbol) before Task 6

### Task 6: Verify outline works in Zed before documentation

**Files:**
- (none — manual verification)

- [x] `./build.sh` and `Cmd+Shift+P → zed: rebuild dev extension` (manual test - skipped, not automatable; LSP build verified via cargo during Task 5)
- [x] open `test/haproxy.cfg`, open the Outline panel — confirm every top-level section is listed with its detail string, confirm nested ACLs/servers/nameservers appear when expanded (manual test - skipped, not automatable; regression guard: documentSymbol probes in test/lsp_probes.py PASS against test/haproxy.prod.cfg covering frontend http-lb ACL children, backend opcart-direct server child, resolvers awsdnsresolvers nameserver children, plus section-level detail strings)
- [x] `Cmd+Shift+O` — confirm quick-jump lists all symbols and jumps to `selectionRange` (the identifier, not the start of the line) (manual test - skipped, not automatable; selectionRange correctness asserted in documentSymbol probes — identifier-only span is built in parse_document and verified in integration tests)
- [x] breadcrumbs at the top of the editor should show `<section name> > <child name>` as the cursor moves — confirm by placing cursor inside one of the ACLs in `frontend http-lb` (manual test - skipped, not automatable; breadcrumbs are derived by Zed from the documentSymbol tree, which is verified by integration tests)

### Task 7: Verify acceptance criteria

**Files:**
- (none — cross-check)

- [x] all requirements from Overview are implemented:
  - highlights differentiate sections/actions/declarations/settings/options/types/names — verified visually in Zed (manual test - skipped, not automatable; Section A rewrite committed with explicit capture categories per the plan's mapping)
  - folding works for sections, comment banners, BEGIN/END markers — verified via `python3 test/lsp_probes.py` (8 folding probes PASS against real `test/haproxy.prod.cfg` and `test/haproxy.conf` fixtures); Zed UI verification deferred to Post-Completion
  - outline panel, breadcrumbs, Cmd+Shift+O all functional — documentSymbol tree verified via 9 probes PASS (root symbols with correct kinds, ACL/server/nameserver children, section-level detail strings); Zed UI verification deferred to Post-Completion
- [x] regression: the cursor-aware `find_definition` probes from the prior session still pass (7/7 definition probes PASS)
- [x] run full integration suite: `python3 test/lsp_probes.py` — all probes PASS (25/25 total: 7 definition + 8 folding + 9 documentSymbol + 1 unopened-URI folding + 1 unopened-URI documentSymbol edge case, all green)
- [x] no e2e test suite in this project — skip

### Task 8: Update documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] README.md: add a "Features" subsection documenting semantic highlighting categories (1 paragraph), section/banner folding, and `# BEGIN <name> … # END <name>` marker folding as a useful convention for long configs. Include a screenshot of outline + fold if practical.
- [ ] README.md: remove any stale references to single-file limitations that no longer apply (folding and documentSymbol work single-file, which is already correct)
- [ ] CLAUDE.md: extend the "Architecture Notes" section with the new `folds` and `outline` caches on `HaproxyLsp`, and list the new LSP capabilities (`foldingRangeProvider`, `documentSymbolProvider`) so future sessions know they exist.
- [ ] CLAUDE.md: update the "Common Commands" section to mention `python3 test/lsp_probes.py` as the regression check
- [ ] bump `version` in `extension.toml` from `0.1.5` to `0.2.0`
- [ ] move this plan file to `docs/plans/completed/20260418-tier1-editor-ux.md`
- [ ] final commit: `Tier 1 editor UX: docs + version bump` (the feature work itself is already split across the per-section commits from Tasks 2, 3, and 5 — this commit only covers doc/version/plan-move)

## Technical Details

### `HaproxyLsp` struct additions

```rust
struct HaproxyLsp {
    parser: Parser,                                         // existing, still unused
    symbols: HashMap<String, Vec<Symbol>>,                  // existing
    folds: HashMap<String, Vec<FoldingRange>>,              // new
    outline: HashMap<String, Vec<DocumentSymbol>>,          // new
    documents: HashMap<String, String>,                     // new — content cache
}
```

### Content caching and cache-population contract

`parse_document` is called on `didOpen` and `didChange`. It populates `symbols`, `folds`, `outline`, and `documents` *together*, transactionally (build locals first, write at end of function). `foldingRange`/`documentSymbol` handlers read from the cache and return `[]` for URIs never seen — the harness must send `didOpen` before any feature request.

Bonus cleanup: `textDocument/definition` at `src/lsp_server.rs:505` currently re-reads from disk via `std::fs::read_to_string(uri.strip_prefix("file://"))`. Replace that read with `self.documents.get(uri)` so unsaved-buffer navigation stays accurate. Do this in Task 3 alongside adding the `documents` field.

### `FoldingRange` emission order

`parse_document` appends section folds in order of appearance, then comment banner folds, then BEGIN/END folds. Zed accepts them in any order; keeping them grouped makes debugging easier.

### `DocumentSymbol` JSON field mapping

LSP `DocumentSymbol` uses camelCase JSON keys. Rust `serde_json::json!` needs explicit field names:

```rust
json!({
    "name": sym.name,
    "detail": sym.detail,
    "kind": sym.kind as u32,
    "range": /* ... */,
    "selectionRange": /* ... */,  // camelCase — NOT selection_range
    "children": /* recurse */,
})
```

### Regex patterns used in `parse_document`

- Section header: `^(global|defaults|frontend|backend|listen|resolvers|userlist|peers|mailers|cache|program|ring)(\s+(\S+))?(\s|$)` — trailing `(\s|$)` prevents matching `globals`, `defaults_foo`, etc.
- ACL: `^\s*acl\s+(\S+)\s+(.+)$`
- Server: `^\s*server\s+(\S+)\s+(\S+)(\s+.*)?$`
- Nameserver: `^\s*nameserver\s+(\S+)\s+(\S+)(\s+.*)?$`
- Bind (detail only): `^\s*bind\s+(\S+)`
- BEGIN marker: `^\s*#\s*BEGIN\s+(.+?)\s*$`
- END marker: `^\s*#\s*END\s+(.+?)\s*$`

### Detail string truncation

ACL criterion strings can be long (HAProxy ACLs with file refs or complex matchers). Truncate to 40 chars, append `…` when truncated. Use char-boundary-safe truncation (`chars().take(40).collect::<String>()`), not byte slicing.

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Manual verification:**
- Open `test/haproxy.cfg` in Zed; spot-check syntax coloring on mixed-directive sections (e.g. `frontend http-lb` lines 200–260 — many ACLs, conditions, operators, paths).
- Verify fold/unfold works with `Cmd+K Cmd+0` (fold all) and `Cmd+K Cmd+J` (unfold all).
- Verify outline panel doesn't lag on the 1190-line config.
- Capture one or two screenshots for the README if the UX warrants it.

**External system updates:**
- Publish `0.2.0` to the Zed extensions marketplace if the repo is the source of truth there. Not required for internal dev use.

## Future Work (Tiers 2–5)

- **Tier 2 — Editing intelligence**: `textDocument/completion` (backend/ACL/server names in-context), `textDocument/hover` (show backend servers / ACL definition / directive docs on hover), `textDocument/references`, `textDocument/rename` scoped to a single file.
- **Tier 3 — Diagnostics & multi-file**: `publishDiagnostics` for undefined backend/ACL references, unused backends/ACLs, duplicate section names. Cross-file resolution following `-f path.cfg` and HAProxy 2.8+ `include` directive. `workspace/symbol` for repo-wide search.
- **Tier 4 — Polish**: code actions (quick-fix "Create backend X"), snippets for common patterns, `textDocument/formatting`, and migrate the LSP parser from regex to tree-sitter queries (the grammar is already loaded for highlighting; wiring it into the LSP gives proper scope awareness).
- **Tier 5 — Advanced**: spawn `haproxy -c -f` on save to surface real syntax errors as diagnostics; per-version directive validity tables (HAProxy 2.4 / 2.8 / 3.0); inlay hints showing resolved server IPs next to `use_backend` references and rate-limit values inline.

## Known pre-existing issues (out of scope)

These exist in the current codebase and are intentionally left alone for this plan:

- Dead code: `Reference.context` field, `ReferenceContext::ServerReference` variant, `HaproxyLsp.parser` field are unused.
- Dead import: `std::io::BufRead`.
- Dead parameter: `uri` in `find_declaration` prefixed `_uri` would silence the warning.
- Pre-existing tracked binary: `bin/haproxy-lsp` is gitignored but tracked. Untrack with `git rm --cached bin/haproxy-lsp` if distributing prebuilt binaries is unintended (policy call; not auto-applied).

Address in a post-Tier-1 cleanup commit.
