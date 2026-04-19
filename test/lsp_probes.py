#!/usr/bin/env python3
"""
LSP integration test harness for haproxy-lsp.

Drives the language server over stdio and asserts responses against fixtures.
Populated incrementally across Tier 1 tasks:
  - Task 1: DEFINITION_PROBES
  - Task 3: FOLDING_PROBES
  - Task 5: DOCUMENT_SYMBOL_PROBES

Usage:
    python3 test/lsp_probes.py
    python3 test/lsp_probes.py --binary ./target/debug/haproxy-lsp
"""

import argparse
import json
import os
import subprocess
import sys
import threading
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = REPO_ROOT / "bin" / "haproxy-lsp"
HAPROXY_CONF = REPO_ROOT / "test" / "haproxy.conf"
HAPROXY_CFG = REPO_ROOT / "test" / "haproxy.prod.cfg"


def path_to_uri(path: Path) -> str:
    return "file://" + str(path.resolve())


class LspClient:
    """Minimal LSP stdio client with Content-Length framing."""

    def __init__(self, binary: Path):
        self.proc = subprocess.Popen(
            [str(binary)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self._next_id = 1
        self._responses: dict[int, dict] = {}
        # Latest `publishDiagnostics` payload per URI, plus a monotonically
        # increasing version that bumps on every update. Tests call
        # `wait_for_diagnostics(uri, min_version=...)` after a did_open /
        # did_change to wait for the *next* publish rather than stale state.
        self._diagnostics: dict[str, list[dict]] = {}
        self._diagnostics_version: dict[str, int] = {}
        self._lock = threading.Lock()
        self._reader_thread = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader_thread.start()
        # Drain stderr continuously: the server uses `eprintln!` on framing
        # errors, and an undrained PIPE buffer (~64 KiB) would block the
        # server once full, causing spurious test timeouts.
        self._stderr_thread = threading.Thread(target=self._stderr_drain, daemon=True)
        self._stderr_thread.start()

    def _stderr_drain(self):
        stderr = self.proc.stderr
        if stderr is None:
            return
        try:
            while True:
                chunk = stderr.read(4096)
                if not chunk:
                    return
        except Exception:
            return

    def _reader_loop(self):
        stdout = self.proc.stdout
        assert stdout is not None
        while True:
            header = b""
            while not header.endswith(b"\r\n\r\n"):
                chunk = stdout.read(1)
                if not chunk:
                    return
                header += chunk
            content_length = None
            for line in header.decode("ascii", errors="replace").split("\r\n"):
                if line.lower().startswith("content-length:"):
                    content_length = int(line.split(":", 1)[1].strip())
                    break
            if content_length is None:
                continue
            body = b""
            while len(body) < content_length:
                chunk = stdout.read(content_length - len(body))
                if not chunk:
                    return
                body += chunk
            try:
                msg = json.loads(body.decode("utf-8"))
            except json.JSONDecodeError:
                continue
            if "id" in msg and msg.get("id") is not None:
                with self._lock:
                    self._responses[int(msg["id"])] = msg
            elif msg.get("method") == "textDocument/publishDiagnostics":
                params = msg.get("params") or {}
                uri = params.get("uri")
                if isinstance(uri, str):
                    diags = params.get("diagnostics") or []
                    with self._lock:
                        self._diagnostics[uri] = diags
                        self._diagnostics_version[uri] = (
                            self._diagnostics_version.get(uri, 0) + 1
                        )

    def _send(self, payload: dict):
        body = json.dumps(payload).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
        assert self.proc.stdin is not None
        self.proc.stdin.write(header + body)
        self.proc.stdin.flush()

    def request(self, method: str, params: dict, timeout: float = 5.0) -> dict:
        req_id = self._next_id
        self._next_id += 1
        self._send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        import time
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self._lock:
                if req_id in self._responses:
                    return self._responses.pop(req_id)
            time.sleep(0.01)
        raise TimeoutError(f"No response for request id={req_id} method={method}")

    def notify(self, method: str, params: dict):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def diagnostics_version(self, uri: str) -> int:
        with self._lock:
            return self._diagnostics_version.get(uri, 0)

    def wait_for_diagnostics(
        self, uri: str, min_version: int = 1, timeout: float = 3.0
    ) -> list[dict]:
        """Block until a `publishDiagnostics` for `uri` with version >=
        `min_version` arrives, then return its `diagnostics` array.

        Call `diagnostics_version(uri)` before sending a did_open / did_change
        to snapshot the pre-publish version, then pass `snapshot + 1` here.
        Raises TimeoutError on timeout."""
        import time
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self._lock:
                if self._diagnostics_version.get(uri, 0) >= min_version:
                    return list(self._diagnostics.get(uri, []))
            time.sleep(0.01)
        raise TimeoutError(
            f"No publishDiagnostics v>={min_version} for {uri} within {timeout}s"
        )

    def initialize(self):
        return self.request("initialize", {"capabilities": {}})

    def initialized(self):
        self.notify("initialized", {})

    def did_open(self, uri: str, text: str, language_id: str = "haproxy"):
        self.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            },
        )

    def shutdown(self):
        try:
            if self.proc.stdin is not None:
                self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=2.0)
        except Exception:
            self.proc.kill()


# ---------------------------------------------------------------------------
# Probes
# ---------------------------------------------------------------------------

# Definition probes target test/haproxy.conf.
# All line/col values are 0-indexed, matching LSP Position semantics.
# Reconstructed from src/lsp_server.rs::find_definition cursor-aware behavior.
DEFINITION_PROBES = [
    {
        "desc": "backend name in `use_backend X if Y`",
        "line": 33,
        "character": 20,
        "expected_def_line": 50,
    },
    {
        "desc": "ACL name in `use_backend X if Y`",
        "line": 33,
        "character": 55,
        "expected_def_line": 31,
    },
    {
        "desc": "standalone `use_backend X`",
        "line": 43,
        "character": 20,
        "expected_def_line": 50,
    },
    {
        "desc": "end-of-word on backend name",
        "line": 33,
        "character": 42,
        "expected_def_line": 50,
    },
    {
        "desc": "on `backend X` definition line",
        "line": 50,
        "character": 15,
        "expected_def_line": 50,
    },
    {
        "desc": "on `acl X ...` definition line",
        "line": 31,
        "character": 10,
        "expected_def_line": 31,
    },
    # --- dotted identifier coverage (grammar allows `.` in names) ---
    {
        "desc": "dotted backend name in `use_backend foo.bar if baz.qux`",
        "line": 90,
        "character": 20,
        "expected_def_line": 82,
    },
    {
        "desc": "dotted ACL name in `use_backend foo.bar if baz.qux`",
        "line": 90,
        "character": 35,
        "expected_def_line": 89,
    },
    # --- `listen NAME address` inline-bind form (grammar permits optional bind_address) ---
    {
        "desc": "listen name when header has inline bind address",
        "line": 78,
        "character": 10,
        "expected_def_line": 78,
    },
    # --- stick-table kind (Task 1) ---
    # The stick-table is bound to the enclosing section `st_ratelimit`
    # (line 94). Its definition range points at the `stick-table` directive
    # line (95). References via sc<N>_*(...) and `table <name>` resolve to
    # that directive line; the cursor on the section header still resolves
    # to the Backend kind at line 94 (existing behavior).
    {
        "desc": "stick-table name on backend header (Backend kind self-ref)",
        "line": 94,
        "character": 12,
        "expected_def_line": 94,
    },
    {
        "desc": "stick-table reference inside `sc0_http_req_rate(...)` call",
        "line": 102,
        "character": 65,
        "expected_def_line": 95,
    },
    {
        "desc": "stick-table reference after ` table ` keyword",
        "line": 101,
        "character": 40,
        "expected_def_line": 95,
    },
    # --- regression (Codex review): servers are section-scoped ---
    # Two backends each declare `server shared ...`. `use_server shared`
    # inside scoped_a must resolve to scoped_a's own server (line 129), not
    # cross-link to scoped_b's line 134 server.
    {
        "desc": "scoped server: use_server in backend A resolves to A's server",
        "line": 130,
        "character": 13,
        "expected_def_line": 129,
    },
    {
        "desc": "scoped server: use_server in backend B resolves to B's server",
        "line": 135,
        "character": 13,
        "expected_def_line": 134,
    },
    # --- regression (Codex review): stick match/store-* sample is not a table ---
    # Per HAProxy grammar, the token after `stick match`/`stick store-*` is a
    # sample expression, not a table name. Even though a stick-table named
    # `src` exists (at line 138), the cursor on `src` here must NOT resolve.
    {
        "desc": "`stick match src` — sample expression, not a stick-table",
        "line": 140,
        "character": 15,
        "expected_null": True,
    },
    {
        "desc": "`stick store-request src` — sample expression, not a stick-table",
        "line": 141,
        "character": 23,
        "expected_null": True,
    },
    # --- regression (Codex review): cursor on fetch inside `{ ... }` vs same-named ACL ---
    # `use_backend ... if { src 10.0.0.0/8 } real_acl` has an ACL named `src`
    # and an ACL named `real_acl`. Cursor on `src` inside braces must NOT
    # resolve to the same-named ACL (it's a sample fetch, not a reference),
    # while cursor on `real_acl` after the closing brace must still resolve.
    {
        "desc": "fetch token inside `{ ... }` must not resolve to same-named ACL",
        "line": 169,
        "character": 40,
        "expected_null": True,
    },
    {
        "desc": "ACL reference after closing brace still resolves",
        "line": 169,
        "character": 58,
        "expected_def_line": 168,
    },
]

# Folding probes verify `textDocument/foldingRange` output against fixture files.
# Each probe supplies the fixture path, an expected fold (startLine/endLine/kind),
# and how to match: `contains` asserts the exact fold is in the result,
# `absent` asserts the URI has no folds cached (not opened).
# Line numbers are 0-indexed (LSP convention).
FOLDING_PROBES: list[dict] = [
    # --- haproxy.conf: augmented with BEGIN/END pairs for this task ---
    {
        "desc": "conf: section fold of `backend accountCreationService_10000`",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 50, "endLine": 57, "kind": "region"},
    },
    {
        "desc": "conf: section fold of `backend profileEditingService_20000` stops before `listen dotted_stats`",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 58, "endLine": 77, "kind": "region"},
    },
    {
        "desc": "conf: BEGIN/END `ssl_options` region",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 66, "endLine": 69, "kind": "region"},
    },
    {
        "desc": "conf: BEGIN/END `notes` region",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 71, "endLine": 74, "kind": "region"},
    },
    {
        "desc": "conf: comment banner over `notes` block",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 71, "endLine": 74, "kind": "comment"},
    },
    {
        "desc": "conf: `listen` header with inline bind address folds as a section",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 78, "endLine": 81, "kind": "region"},
    },
    {
        "desc": "conf: dotted backend name folds as a section",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 82, "endLine": 85, "kind": "region"},
    },
    {
        "desc": "conf: `frontend dotted_caller` fold ends before stick-table backend",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 86, "endLine": 93, "kind": "region"},
    },
    {
        "desc": "conf: BEGIN/END `dotted_names` region wraps new fixtures",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 76, "endLine": 92, "kind": "region"},
    },
    {
        "desc": "conf: `backend st_ratelimit` section fold",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 94, "endLine": 97, "kind": "region"},
    },
    {
        "desc": "conf: `frontend st_caller` fold stops before next section",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 98, "endLine": 105, "kind": "region"},
    },
    {
        "desc": "conf: `frontend dup_acl_caller` fold runs up to the next section",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 118, "endLine": 126, "kind": "region"},
    },
    {
        "desc": "conf: `backend src` section fold ends before next frontend",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 137, "endLine": 144, "kind": "region"},
    },
    # --- haproxy.prod.cfg: the real 1190-line fixture ---
    {
        "desc": "prod.cfg: section fold of `defaults`",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 33, "endLine": 48, "kind": "region"},
    },
    {
        "desc": "prod.cfg: final section fold reaches last line",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 1169, "endLine": 1187, "kind": "region"},
    },
    {
        "desc": "prod.cfg: BEGIN/END `Rate limit for login` region",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 57, "endLine": 60, "kind": "region"},
    },
    # --- edge case: URI never opened returns [] ---
    {
        "desc": "unopened URI returns empty fold list",
        "fixture": "unopened",
        "match": "absent",
        "expected": None,
    },
]

# DocumentSymbol probes verify `textDocument/documentSymbol` output against
# test/haproxy.prod.cfg. Each probe declares a fixture key plus a match rule:
#
#   - "root_contains_symbol": root list must contain a symbol with {name, kind}
#     (optionally verified detail_contains / detail_regex).
#   - "children_count_at_least": a named root symbol has >= N children, all of a
#     required SymbolKind, with non-empty detail strings.
#   - "child_detail_regex": a named root symbol has >= 1 child whose detail
#     matches a regex pattern.
#   - "absent": the URI has no outline cached (never opened).
#
# LSP SymbolKind numeric values used here:
#   Namespace=3, Class=5, Property=7, Field=8, Interface=11.
DOCUMENT_SYMBOL_PROBES: list[dict] = [
    {
        "desc": "prod.cfg: root contains `defaults` (Namespace)",
        "fixture": "cfg",
        "match": "root_contains_symbol",
        "name": "defaults",
        "kind": 3,
    },
    {
        "desc": "prod.cfg: root contains `http-lb` (Interface)",
        "fixture": "cfg",
        "match": "root_contains_symbol",
        "name": "http-lb",
        "kind": 11,
    },
    {
        "desc": "prod.cfg: root contains `opcart-direct` (Class)",
        "fixture": "cfg",
        "match": "root_contains_symbol",
        "name": "opcart-direct",
        "kind": 5,
    },
    {
        "desc": "prod.cfg: root contains `stats` listen (Class)",
        "fixture": "cfg",
        "match": "root_contains_symbol",
        "name": "stats",
        "kind": 5,
    },
    {
        "desc": "prod.cfg: root contains `awsdnsresolvers` (Module)",
        "fixture": "cfg",
        "match": "root_contains_symbol",
        "name": "awsdnsresolvers",
        "kind": 2,
        "detail_regex": r"^\d+ nameservers$",
    },
    {
        "desc": "prod.cfg: `http-lb` has >=5 ACL children with non-empty detail",
        "fixture": "cfg",
        "match": "children_count_at_least",
        "name": "http-lb",
        "child_kind": 7,
        "min_children": 5,
        "require_non_empty_detail": True,
    },
    {
        "desc": "prod.cfg: `opcart-direct` has >=1 server child with address detail",
        "fixture": "cfg",
        "match": "child_detail_regex",
        "name": "opcart-direct",
        "child_kind": 8,
        "min_children": 1,
        "detail_regex": r".+:\d+",
    },
    {
        "desc": "prod.cfg: `awsdnsresolvers` has >=1 nameserver Field child",
        "fixture": "cfg",
        "match": "child_detail_regex",
        "name": "awsdnsresolvers",
        "child_kind": 8,
        "min_children": 1,
        "detail_regex": r".+:\d+",
    },
    {
        "desc": "unopened URI returns empty documentSymbol list",
        "fixture": "unopened",
        "match": "absent",
    },
    {
        "desc": "conf: listen with inline bind address surfaces `127.0.0.1:9091` in detail",
        "fixture": "conf",
        "match": "root_contains_symbol",
        "name": "dotted_stats",
        "kind": 5,
        "detail_regex": r"127\.0\.0\.1:9091",
    },
    # Trailing-`#`-comment header lines: outline must store only the first
    # identifier token as the symbol name, not the whole tail of the line.
    # `detail_forbidden_regex` guards against the earlier regression where
    # `#` or comment text leaked into the symbol detail (e.g. detail="#" or
    # "#, *:80") — the probes must fail loudly if that ever returns.
    {
        "desc": "decl: backend header with trailing `#` comment names symbol `be_commented`",
        "fixture": "decl",
        "match": "root_contains_symbol",
        "name": "be_commented",
        "kind": 5,
        "detail_forbidden_regex": r"#",
    },
    {
        "desc": "decl: frontend header with trailing `#` comment names symbol `fe_commented`",
        "fixture": "decl",
        "match": "root_contains_symbol",
        "name": "fe_commented",
        "kind": 11,
        "detail_regex": r"^\*:81$",
    },
    {
        "desc": "decl: listen header with trailing `#` comment names symbol `ln_commented`",
        "fixture": "decl",
        "match": "root_contains_symbol",
        "name": "ln_commented",
        "kind": 5,
        "detail_regex": r"^\*:82$",
    },
]


# Declaration probes exercise `textDocument/declaration`, which returns an
# array of every reference location for the symbol under the cursor. The
# fixture is constructed inline so adding negation edge cases does not
# perturb line numbers of the on-disk fixtures used by other probe sets.
DECLARATION_FIXTURE_URI = "file:///tmp/haproxy-lsp-declaration-fixture.cfg"
DECLARATION_FIXTURE_TEXT = "\n".join(
    [
        "frontend fe",                              # 0
        "  bind *:80",                              # 1
        "  acl plain hdr(x-a) a",                   # 2
        "  acl dotted.acl hdr(x-b) b",              # 3
        "  use_backend be if plain",                # 4: positive plain
        "  use_backend be if !plain",               # 5: negated plain
        "  use_backend be if dotted.acl",           # 6: positive dotted
        "  use_backend be if !dotted.acl",          # 7: negated dotted
        "  use_backend be unless !plain",           # 8: negated under unless
        "  use_backend be_commented if plain",      # 9: ref to trailing-comment backend
        "",                                          # 10
        "backend be",                                # 11
        "  mode http",                               # 12
        "",                                          # 13
        "backend be_commented # trailing comment",   # 14: header with inline comment
        "  mode http",                               # 15
        "",                                          # 16
        "frontend fe_commented # trailing comment",  # 17: header with inline comment
        "  bind *:81",                               # 18
        "",                                          # 19
        "listen ln_commented # trailing comment",    # 20: header with inline comment
        "  bind *:82",                               # 21
        "",
    ]
)

# Separate inline fixture for the IP/hostname-collision regression: the
# fallback "try every symbol kind" path in find_definition used to return a
# random symbol of the same textual name (e.g. a backend literally named
# `10.0.0.1`) when the cursor was on a bind address or server hostname.
# These probes must see a null result — the LSP should refuse to resolve
# address/hostname tokens to unrelated symbols.
DEFINITION_NULL_FIXTURE_URI = "file:///tmp/haproxy-lsp-definition-null-fixture.cfg"
DEFINITION_NULL_FIXTURE_TEXT = "\n".join(
    [
        "backend 10.0.0.1",                     # 0: pathological numeric-name backend
        "  mode http",                          # 1
        "",                                      # 2
        "listen stats 10.0.0.1:9091",           # 3: bind addr collides textually with backend name
        "  bind *:9091",                        # 4
        "",                                      # 5
        "backend app",                           # 6
        "  mode http",                           # 7
        "  server s1 10.0.0.1:8080 check",      # 8: server addr collides textually with backend name
        "",                                      # 9
        "listen 10.0.0.1",                       # 10: same-kind collision — an actual listen named `10.0.0.1`
        "  bind *:7777",                         # 11
        "",                                      # 12
        "backend svc",                           # 13
        "  mode http",                           # 14
        "  server 10.0.0.1 10.0.0.2:8080 check", # 15: same-kind server — name `10.0.0.1` vs addr `10.0.0.2`
        "",
    ]
)

DEFINITION_NULL_PROBES: list[dict] = [
    {
        "desc": "bind address on `listen` header does not resolve (cross-kind: backend of same name)",
        "line": 3,
        "character": 15,
    },
    {
        "desc": "server address on `server` line does not resolve (cross-kind: backend of same name)",
        "line": 8,
        "character": 17,
    },
    {
        "desc": "bind address on `listen` header does not resolve (same-kind: another `listen 10.0.0.1`)",
        "line": 3,
        "character": 17,
    },
    {
        "desc": "server address on `server` line does not resolve (same-kind: another `server 10.0.0.1`)",
        "line": 15,
        "character": 20,
    },
]


# References probes exercise `textDocument/references` against
# test/haproxy.conf. Each probe drives the cursor-aware resolution path plus
# the definition-line fallback. `include_declaration` toggles whether the
# symbol's own definition range is prepended to the location list.
# `expected_lines` is a set of 0-indexed line numbers each returned Location
# must map to via `range.start.line`.
REFERENCES_PROBES: list[dict] = [
    {
        "desc": "backend name on `use_backend` line (exclude declaration)",
        "line": 33,
        "character": 20,
        "include_declaration": False,
        "expected_lines": {33, 43, 123, 156},
    },
    {
        "desc": "backend name on `use_backend` line (include declaration)",
        "line": 33,
        "character": 20,
        "include_declaration": True,
        "expected_lines": {33, 43, 50, 123, 156},
    },
    {
        "desc": "ACL in `if` condition (exclude declaration)",
        "line": 33,
        "character": 55,
        "include_declaration": False,
        "expected_lines": {33},
    },
    {
        "desc": "ACL in `if` condition (include declaration)",
        "line": 33,
        "character": 55,
        "include_declaration": True,
        "expected_lines": {31, 33},
    },
    {
        "desc": "stick-table in `sc0_*(name)` (exclude declaration)",
        "line": 102,
        "character": 65,
        "include_declaration": False,
        "expected_lines": {101, 102, 116, 148},
    },
    {
        "desc": "stick-table in `sc0_*(name)` (include declaration)",
        "line": 102,
        "character": 65,
        "include_declaration": True,
        "expected_lines": {95, 101, 102, 116, 148},
    },
    {
        "desc": "stick-table after ` table ` keyword (exclude declaration)",
        "line": 101,
        "character": 40,
        "include_declaration": False,
        "expected_lines": {101, 102, 116, 148},
    },
    {
        "desc": "server `use_server` call-sites listed (exclude declaration)",
        "line": 110,
        "character": 15,
        "include_declaration": False,
        "expected_lines": {110},
    },
    {
        "desc": "server reference from definition line (include declaration)",
        "line": 108,
        "character": 12,
        "include_declaration": True,
        "expected_lines": {108, 110},
    },
    {
        "desc": "duplicate ACL references attach only once per call-site; both declaration lines surface",
        "line": 123,
        "character": 48,
        "include_declaration": True,
        "expected_lines": {121, 122, 123},
    },
    {
        "desc": "backend name on definition line (include declaration)",
        "line": 50,
        "character": 15,
        "include_declaration": True,
        "expected_lines": {33, 43, 50, 123, 156},
    },
    {
        "desc": "backend name on definition line (exclude declaration)",
        "line": 50,
        "character": 15,
        "include_declaration": False,
        "expected_lines": {33, 43, 123, 156},
    },
    {
        "desc": "ACL after inline sample whose regex has unbalanced literal brace",
        "line": 156,
        "character": 70,
        "include_declaration": True,
        "expected_lines": {155, 156},
    },
]


# Rename probes exercise `textDocument/prepareRename` and `textDocument/rename`
# against test/haproxy.conf. `type` selects the handler; identifier ranges are
# computed from the on-disk fixture (identifiers are ASCII so char == byte).
#   - prepare: asserts `result.range` and `result.placeholder`, or that
#     the server returns `null` for non-renameable cursors.
#   - rename: asserts the set of edits under `changes[uri]` as
#     (line, start_char, end_char) tuples. A `new_name` field is required.
#   - rename with `expect_error`: asserts a JSON-RPC error with the given
#     `expected_error_code`.
RENAME_PROBES: list[dict] = [
    {
        "desc": "prepareRename on backend reference returns identifier range",
        "type": "prepare",
        "line": 33,
        "character": 20,
        "expected_range": (33, 14, 42),
        "expected_placeholder": "accountCreationService_10000",
    },
    {
        "desc": "prepareRename on backend definition returns identifier range",
        "type": "prepare",
        "line": 50,
        "character": 15,
        "expected_range": (50, 8, 36),
        "expected_placeholder": "accountCreationService_10000",
    },
    {
        "desc": "prepareRename on ACL reference returns identifier range",
        "type": "prepare",
        "line": 33,
        "character": 55,
        "expected_range": (33, 46, 73),
        "expected_placeholder": "app__accountCreationService",
    },
    {
        "desc": "prepareRename on `use_backend` keyword returns null",
        "type": "prepare",
        "line": 33,
        "character": 5,
        "expected_null": True,
    },
    {
        "desc": "prepareRename on `backend` keyword of definition line returns null",
        "type": "prepare",
        "line": 50,
        "character": 3,
        "expected_null": True,
    },
    {
        "desc": "rename backend updates definition + every reference",
        "type": "rename",
        "line": 33,
        "character": 20,
        "new_name": "newBackend",
        "expected_edits": {(50, 8, 36), (33, 14, 42), (43, 14, 42), (123, 14, 42), (156, 14, 42)},
    },
    {
        "desc": "rename ACL updates definition + every if-condition reference",
        "type": "rename",
        "line": 33,
        "character": 55,
        "new_name": "newAcl",
        "expected_edits": {(31, 6, 33), (33, 46, 73)},
    },
    {
        "desc": "rename with empty name returns -32602",
        "type": "rename",
        "line": 33,
        "character": 20,
        "new_name": "",
        "expect_error": True,
        "expected_error_code": -32602,
    },
    {
        "desc": "rename with whitespace in name returns -32602",
        "type": "rename",
        "line": 33,
        "character": 20,
        "new_name": "bad name",
        "expect_error": True,
        "expected_error_code": -32602,
    },
    {
        "desc": "rename with disallowed character returns -32602",
        "type": "rename",
        "line": 33,
        "character": 20,
        "new_name": "bad@name",
        "expect_error": True,
        "expected_error_code": -32602,
    },
    {
        "desc": "prepareRename on server `use_server` reference returns identifier range",
        "type": "prepare",
        "line": 110,
        "character": 15,
        "expected_range": (110, 13, 22),
        "expected_placeholder": "srv_alpha",
    },
    {
        "desc": "rename server updates definition + every `use_server` reference",
        "type": "rename",
        "line": 110,
        "character": 15,
        "new_name": "srv_renamed",
        "expected_edits": {(108, 9, 18), (110, 13, 22)},
    },
    # --- regression (Codex): scoped server rename only touches own section ---
    # Renaming `server shared` in backend scoped_a must rewrite the two
    # shared references in scoped_a only (lines 129, 130). The same-named
    # server in scoped_b (lines 134, 135) must be left alone.
    {
        "desc": "rename server scoped to enclosing backend (does not touch duplicate in other backend)",
        "type": "rename",
        "line": 129,
        "character": 11,
        "new_name": "shared_a",
        "expected_edits": {(129, 9, 15), (130, 13, 19)},
    },
    # --- regression (Codex): section rename cascades to stick-table call sites ---
    # Renaming backend `st_ratelimit` must rewrite the backend header,
    # the stick-table ` table st_ratelimit` ref (line 101), the
    # `sc0_http_req_rate(st_ratelimit)` ref (line 102), and the
    # `table st_ratelimit # ...` ref in the second fixture (line 116).
    {
        "desc": "rename section with stick-table cascades to sc*_*/table call-sites",
        "type": "rename",
        "line": 94,
        "character": 12,
        "new_name": "rl_renamed",
        "expected_edits": {
            (94, 8, 20),
            (101, 35, 47),
            (102, 59, 71),
            (116, 35, 47),
            (148, 35, 47),
            (148, 71, 83),
        },
    },
    # --- regression (Codex): direct rename skipping prepareRename must not
    # accept cursor positions that would have been rejected by prepareRename.
    # Cursor on the `backend` keyword (column 2) of a definition line must
    # return null, matching the prepareRename guard instead of silently
    # renaming the whole symbol.
    {
        "desc": "rename on `backend` keyword of definition line returns null",
        "type": "rename",
        "line": 50,
        "character": 2,
        "new_name": "renamed",
        "expected_null": True,
    },
    {
        "desc": "rename on `use_backend` keyword returns null",
        "type": "rename",
        "line": 33,
        "character": 5,
        "new_name": "renamed",
        "expected_null": True,
    },
]


# Hover probes exercise `textDocument/hover` against test/haproxy.conf.
# Each probe points at a cursor position and asserts the hover response is a
# markdown MarkupContent whose `value` contains every listed substring
# (order-independent). `expected_null` asserts a null hover instead.
HOVER_PROBES: list[dict] = [
    {
        "desc": "hover on backend name in `use_backend` shows definition + servers",
        "line": 33,
        "character": 20,
        "expected_contains": [
            "backend accountCreationService_10000",
            "mode http",
            "balance roundrobin",
            "151_256_250_151_35800",
        ],
    },
    {
        "desc": "hover on backend header itself shows backend summary",
        "line": 50,
        "character": 15,
        "expected_contains": [
            "backend accountCreationService_10000",
            "server 151_256_250_151_35800",
        ],
    },
    {
        "desc": "hover on ACL reference shows ACL definition line",
        "line": 33,
        "character": 55,
        "expected_contains": [
            "acl app__accountCreationService",
            "hdr(x-microservice-app-id)",
        ],
    },
    {
        "desc": "hover on stick-table reference shows stick-table directive",
        "line": 102,
        "character": 65,
        "expected_contains": [
            "stick-table type ip",
            "http_req_rate(10s)",
        ],
    },
    {
        "desc": "hover on server name shows server directive line",
        "line": 56,
        "character": 15,
        "expected_contains": [
            "server 151_256_250_151_35800",
            "151.256.250.151:35800",
        ],
    },
    {
        "desc": "hover on `use_backend` directive keyword shows docs snippet",
        "line": 33,
        "character": 5,
        "expected_contains": [
            "use_backend",
        ],
    },
    {
        "desc": "hover on `stick-table` directive keyword shows docs snippet",
        "line": 95,
        "character": 4,
        "expected_contains": [
            "stick-table",
        ],
    },
    {
        "desc": "hover on whitespace returns null",
        "line": 1,
        "character": 0,
        "expected_null": True,
    },
    # --- regression (Codex): hover on a fetch token inside `{ ... }` must
    # NOT leak through to an unconstrained by-name lookup. The
    # `use_backend brace_cursor_target if { src 10.0.0.0/8 } real_acl` line
    # has a fetch `src` inside the brace group; a backend named `src` also
    # exists elsewhere in the fixture. Hover on the fetch must return null
    # (same contract as find_definition).
    {
        "desc": "hover on fetch inside `{ ... }` does not resolve to same-named backend",
        "line": 169,
        "character": 40,
        "expected_null": True,
    },
    {
        "desc": "hover on arbitrary non-symbol token returns null",
        "line": 3,
        "character": 3,
        "expected_null": True,
    },
]


# Completion probes exercise `textDocument/completion`. Each probe declares a
# cursor position on a fixture plus a minimum set of expected labels (not an
# exact-equality check, to keep the tests tolerant of future directive-list
# changes). `expected_kind` (when set) asserts every matched item carries the
# given CompletionItemKind; `expected_missing` asserts labels that MUST NOT
# appear (used to prove a context is distinguished from another).
COMPLETION_FIXTURE_USE_SERVER_URI = (
    "file:///tmp/haproxy-lsp-completion-use-server-fixture.cfg"
)
COMPLETION_FIXTURE_USE_SERVER_TEXT = "\n".join(
    [
        "backend bk",                    # 0
        "  server s1 10.0.0.1:1",        # 1
        "  server s2 10.0.0.2:2",        # 2
        "  use_server ",                 # 3: cursor at char 13 = right after `use_server `
        "",                               # 4
        "backend bk_other",               # 5
        "  server elsewhere 10.9.9.9:9", # 6: must NOT appear in bk scope
        "",
    ]
)

COMPLETION_PROBES: list[dict] = [
    {
        "desc": "after `use_backend ` → backend names",
        "fixture": "conf",
        "line": 33,
        "character": 14,
        "expected_labels": {
            "accountCreationService_10000",
            "profileEditingService_20000",
            "dotted.backend",
            "st_ratelimit",
        },
        "expected_kind": 7,  # Class
    },
    {
        "desc": "after `if ` → ACL names",
        "fixture": "conf",
        "line": 33,
        "character": 46,
        "expected_labels": {
            "app__accountCreationService",
            "app__profileEditingService",
            "dotted.acl",
        },
        "expected_kind": 21,  # Constant
    },
    {
        "desc": "inside `sc0_http_req_rate(` → stick-table names",
        "fixture": "conf",
        "line": 102,
        "character": 60,
        "expected_labels": {"st_ratelimit"},
        "expected_kind": 22,  # Struct
    },
    {
        "desc": "after `use_server ` → servers in enclosing backend only",
        "fixture": "use_server",
        "line": 3,
        "character": 13,
        "expected_labels": {"s1", "s2"},
        "expected_kind": 6,  # Variable
        "expected_missing": {"elsewhere"},
    },
    {
        "desc": "start of line inside backend section → directive allowlist",
        "fixture": "conf",
        "line": 57,
        "character": 0,
        "expected_labels": {"server", "balance", "mode", "option", "http-request"},
        "expected_kind": 14,  # Keyword
    },
    {
        "desc": "prod.cfg: after `use_backend ` → ≥5 backend names",
        "fixture": "cfg",
        "line": 727,
        "character": 16,
        "expected_min_labels_of_kind": {"kind": 7, "min": 5},
    },
]


DECLARATION_PROBES: list[dict] = [
    {
        "desc": "`!plain` in `if` condition yields declaration reference",
        "acl_line": 2,
        "acl_char": 6,
        "expected_ref_lines": {4, 5, 8, 9},
    },
    {
        "desc": "`!dotted.acl` in `if` condition yields declaration reference",
        "acl_line": 3,
        "acl_char": 8,
        "expected_ref_lines": {6, 7},
    },
    {
        "desc": "backend header with trailing `#` comment resolves declaration",
        "acl_line": 14,
        "acl_char": 10,
        "expected_ref_lines": {9},
    },
]


# ---------------------------------------------------------------------------
# Runners
# ---------------------------------------------------------------------------

class Results:
    def __init__(self):
        self.rows: list[tuple[str, str, str, str]] = []  # (section, desc, status, detail)
        self.failures = 0

    def record(self, section: str, desc: str, ok: bool, detail: str = ""):
        status = "PASS" if ok else "FAIL"
        if not ok:
            self.failures += 1
        self.rows.append((section, desc, status, detail))

    def print(self):
        if not self.rows:
            print("(no probes run)")
            return
        w_section = max(len(r[0]) for r in self.rows + [("Section", "", "", "")])
        w_desc = max(len(r[1]) for r in self.rows + [("", "Probe", "", "")])
        w_status = 4
        header = f"{'Section':<{w_section}}  {'Probe':<{w_desc}}  {'Stat':<{w_status}}  Detail"
        print(header)
        print("-" * len(header))
        for section, desc, status, detail in self.rows:
            print(f"{section:<{w_section}}  {desc:<{w_desc}}  {status:<{w_status}}  {detail}")
        print()
        total = len(self.rows)
        passed = total - self.failures
        print(f"{passed}/{total} probes passed")


def run_definition_probes(client: LspClient, results: Results):
    if not HAPROXY_CONF.exists():
        results.record("definition", "fixture present", False, f"missing: {HAPROXY_CONF}")
        return
    uri = path_to_uri(HAPROXY_CONF)
    text = HAPROXY_CONF.read_text()
    client.did_open(uri, text)

    for probe in DEFINITION_PROBES:
        try:
            resp = client.request(
                "textDocument/definition",
                {
                    "textDocument": {"uri": uri},
                    "position": {
                        "line": probe["line"],
                        "character": probe["character"],
                    },
                },
            )
        except TimeoutError as exc:
            results.record("definition", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        # Some probes assert that the cursor resolves to NOTHING (e.g. a
        # sample expression after `stick match` must not cross-link to a
        # same-named stick-table). Accept both `null` and `[]` as "no
        # definition found" per LSP spec.
        if probe.get("expected_null"):
            ok = result is None or result == []
            detail = "null as expected" if ok else f"unexpected result: {result!r}"
            results.record("definition", probe["desc"], ok, detail)
            continue

        if result is None:
            results.record(
                "definition",
                probe["desc"],
                False,
                f"null result (probe at {probe['line']}:{probe['character']})",
            )
            continue

        # Server returns a single Location object, not an array.
        actual_line = None
        if isinstance(result, dict) and "range" in result:
            actual_line = result["range"]["start"]["line"]
        elif isinstance(result, list) and result:
            actual_line = result[0]["range"]["start"]["line"]

        expected = probe["expected_def_line"]
        ok = actual_line == expected
        detail = f"expected def line {expected}, got {actual_line}"
        results.record("definition", probe["desc"], ok, detail)


def run_folding_probes(client: LspClient, results: Results):
    if not FOLDING_PROBES:
        return

    # Open each fixture that probes reference so the LSP caches folds for it.
    opened_uris: dict[str, str] = {}
    fixtures = {
        "conf": HAPROXY_CONF,
        "cfg": HAPROXY_CFG,
    }
    for key, path in fixtures.items():
        if not any(p["fixture"] == key for p in FOLDING_PROBES):
            continue
        if not path.exists():
            results.record("folding", f"fixture present: {key}", False, f"missing: {path}")
            continue
        uri = path_to_uri(path)
        client.did_open(uri, path.read_text())
        opened_uris[key] = uri

    for probe in FOLDING_PROBES:
        fixture_key = probe["fixture"]
        if fixture_key == "unopened":
            # Use a URI we never sent didOpen for.
            uri = "file:///tmp/haproxy-lsp-never-opened.cfg"
        else:
            uri = opened_uris.get(fixture_key)
            if uri is None:
                results.record("folding", probe["desc"], False, "fixture not opened")
                continue

        try:
            resp = client.request(
                "textDocument/foldingRange",
                {"textDocument": {"uri": uri}},
            )
        except TimeoutError as exc:
            results.record("folding", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        if not isinstance(result, list):
            results.record(
                "folding",
                probe["desc"],
                False,
                f"expected list, got {type(result).__name__}: {result!r}",
            )
            continue

        match = probe["match"]
        if match == "absent":
            ok = result == []
            detail = f"got {len(result)} folds" if not ok else "empty as expected"
            results.record("folding", probe["desc"], ok, detail)
        elif match == "contains":
            expected = probe["expected"]
            found = any(
                r.get("startLine") == expected["startLine"]
                and r.get("endLine") == expected["endLine"]
                and r.get("kind") == expected["kind"]
                for r in result
            )
            if found:
                results.record("folding", probe["desc"], True, f"fold present ({len(result)} total)")
            else:
                preview = ", ".join(
                    f"[{r.get('startLine')}-{r.get('endLine')} {r.get('kind')}]" for r in result[:8]
                )
                results.record(
                    "folding",
                    probe["desc"],
                    False,
                    f"expected {expected}, not in {len(result)} folds: {preview}",
                )
        else:
            results.record("folding", probe["desc"], False, f"unknown match type: {match}")


def _find_root_symbol(symbols: list, name: str) -> dict | None:
    for s in symbols:
        if s.get("name") == name:
            return s
    return None


def run_document_symbol_probes(client: LspClient, results: Results):
    if not DOCUMENT_SYMBOL_PROBES:
        return

    import re

    opened_uris: dict[str, str] = {}
    fixtures = {
        "conf": HAPROXY_CONF,
        "cfg": HAPROXY_CFG,
    }
    for key, path in fixtures.items():
        if not any(p["fixture"] == key for p in DOCUMENT_SYMBOL_PROBES):
            continue
        if not path.exists():
            results.record("documentSymbol", f"fixture present: {key}", False, f"missing: {path}")
            continue
        uri = path_to_uri(path)
        # Safe to re-open; parse_document is idempotent on the cache.
        client.did_open(uri, path.read_text())
        opened_uris[key] = uri

    # Expose the inline declaration fixture so documentSymbol probes can
    # assert on trailing-comment header lines without adding a new on-disk
    # fixture (and without shifting line numbers of the existing probes).
    if any(p["fixture"] == "decl" for p in DOCUMENT_SYMBOL_PROBES):
        client.did_open(DECLARATION_FIXTURE_URI, DECLARATION_FIXTURE_TEXT)
        opened_uris["decl"] = DECLARATION_FIXTURE_URI

    cached: dict[str, list] = {}

    def get_symbols(uri: str) -> list | None:
        if uri in cached:
            return cached[uri]
        try:
            resp = client.request(
                "textDocument/documentSymbol",
                {"textDocument": {"uri": uri}},
            )
        except TimeoutError as exc:
            return None
        result = resp.get("result")
        if not isinstance(result, list):
            return None
        cached[uri] = result
        return result

    for probe in DOCUMENT_SYMBOL_PROBES:
        fixture_key = probe["fixture"]
        if fixture_key == "unopened":
            uri = "file:///tmp/haproxy-lsp-never-opened-ds.cfg"
        else:
            uri = opened_uris.get(fixture_key)
            if uri is None:
                results.record("documentSymbol", probe["desc"], False, "fixture not opened")
                continue

        symbols = get_symbols(uri)
        if symbols is None:
            results.record("documentSymbol", probe["desc"], False, "no result / timeout")
            continue

        match = probe["match"]
        if match == "absent":
            ok = symbols == []
            detail = f"got {len(symbols)} symbols" if not ok else "empty as expected"
            results.record("documentSymbol", probe["desc"], ok, detail)
            continue

        if match == "root_contains_symbol":
            sym = _find_root_symbol(symbols, probe["name"])
            if sym is None:
                preview = ", ".join(s.get("name", "?") for s in symbols[:8])
                results.record(
                    "documentSymbol",
                    probe["desc"],
                    False,
                    f"name {probe['name']!r} not in root ({len(symbols)} total): {preview}",
                )
                continue
            if sym.get("kind") != probe["kind"]:
                results.record(
                    "documentSymbol",
                    probe["desc"],
                    False,
                    f"kind mismatch: expected {probe['kind']}, got {sym.get('kind')}",
                )
                continue
            if "detail_regex" in probe:
                detail_str = sym.get("detail") or ""
                if not re.match(probe["detail_regex"], detail_str):
                    results.record(
                        "documentSymbol",
                        probe["desc"],
                        False,
                        f"detail {detail_str!r} did not match {probe['detail_regex']!r}",
                    )
                    continue
            if "detail_forbidden_regex" in probe:
                detail_str = sym.get("detail") or ""
                if re.search(probe["detail_forbidden_regex"], detail_str):
                    results.record(
                        "documentSymbol",
                        probe["desc"],
                        False,
                        f"detail {detail_str!r} matched forbidden {probe['detail_forbidden_regex']!r}",
                    )
                    continue
            results.record(
                "documentSymbol",
                probe["desc"],
                True,
                f"found (kind={sym.get('kind')}, detail={sym.get('detail')!r})",
            )
            continue

        if match == "children_count_at_least":
            sym = _find_root_symbol(symbols, probe["name"])
            if sym is None:
                results.record("documentSymbol", probe["desc"], False, f"parent {probe['name']!r} missing")
                continue
            kids = sym.get("children") or []
            kids_of_kind = [c for c in kids if c.get("kind") == probe["child_kind"]]
            if len(kids_of_kind) < probe["min_children"]:
                results.record(
                    "documentSymbol",
                    probe["desc"],
                    False,
                    f"only {len(kids_of_kind)} children of kind {probe['child_kind']} (need {probe['min_children']})",
                )
                continue
            if probe.get("require_non_empty_detail"):
                empty = [c for c in kids_of_kind if not (c.get("detail") or "").strip()]
                if empty:
                    results.record(
                        "documentSymbol",
                        probe["desc"],
                        False,
                        f"{len(empty)} children had empty detail",
                    )
                    continue
            results.record(
                "documentSymbol",
                probe["desc"],
                True,
                f"{len(kids_of_kind)} children (kind={probe['child_kind']})",
            )
            continue

        if match == "child_detail_regex":
            sym = _find_root_symbol(symbols, probe["name"])
            if sym is None:
                results.record("documentSymbol", probe["desc"], False, f"parent {probe['name']!r} missing")
                continue
            kids = sym.get("children") or []
            matching = [
                c
                for c in kids
                if c.get("kind") == probe["child_kind"]
                and re.search(probe["detail_regex"], c.get("detail") or "")
            ]
            if len(matching) < probe["min_children"]:
                preview = ", ".join(
                    f"{c.get('name')}={c.get('detail')!r}" for c in kids[:5]
                )
                results.record(
                    "documentSymbol",
                    probe["desc"],
                    False,
                    f"only {len(matching)} matched; sample: {preview}",
                )
                continue
            results.record(
                "documentSymbol",
                probe["desc"],
                True,
                f"{len(matching)} children matched regex",
            )
            continue

        results.record("documentSymbol", probe["desc"], False, f"unknown match type: {match}")


def run_definition_null_probes(client: LspClient, results: Results):
    if not DEFINITION_NULL_PROBES:
        return
    client.did_open(DEFINITION_NULL_FIXTURE_URI, DEFINITION_NULL_FIXTURE_TEXT)

    for probe in DEFINITION_NULL_PROBES:
        try:
            resp = client.request(
                "textDocument/definition",
                {
                    "textDocument": {"uri": DEFINITION_NULL_FIXTURE_URI},
                    "position": {
                        "line": probe["line"],
                        "character": probe["character"],
                    },
                },
            )
        except TimeoutError as exc:
            results.record("definition-null", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        # Accept both `null` and `[]` as "no definition found" per LSP spec.
        ok = result is None or result == []
        detail = "null as expected" if ok else f"unexpected result: {result!r}"
        results.record("definition-null", probe["desc"], ok, detail)


def run_references_probes(client: LspClient, results: Results):
    if not REFERENCES_PROBES:
        return
    if not HAPROXY_CONF.exists():
        results.record("references", "fixture present", False, f"missing: {HAPROXY_CONF}")
        return
    uri = path_to_uri(HAPROXY_CONF)
    client.did_open(uri, HAPROXY_CONF.read_text())

    for probe in REFERENCES_PROBES:
        try:
            resp = client.request(
                "textDocument/references",
                {
                    "textDocument": {"uri": uri},
                    "position": {
                        "line": probe["line"],
                        "character": probe["character"],
                    },
                    "context": {"includeDeclaration": probe["include_declaration"]},
                },
            )
        except TimeoutError as exc:
            results.record("references", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        if not isinstance(result, list):
            results.record(
                "references",
                probe["desc"],
                False,
                f"expected list, got {type(result).__name__}: {result!r}",
            )
            continue

        actual_lines = {loc["range"]["start"]["line"] for loc in result}
        expected = probe["expected_lines"]
        ok = actual_lines == expected
        if ok:
            detail = f"lines {sorted(actual_lines)}"
        else:
            detail = f"expected {sorted(expected)}, got {sorted(actual_lines)}"
        results.record("references", probe["desc"], ok, detail)


def run_rename_probes(client: LspClient, results: Results):
    if not RENAME_PROBES:
        return
    if not HAPROXY_CONF.exists():
        results.record("rename", "fixture present", False, f"missing: {HAPROXY_CONF}")
        return
    uri = path_to_uri(HAPROXY_CONF)
    client.did_open(uri, HAPROXY_CONF.read_text())

    for probe in RENAME_PROBES:
        probe_type = probe["type"]
        params = {
            "textDocument": {"uri": uri},
            "position": {
                "line": probe["line"],
                "character": probe["character"],
            },
        }

        if probe_type == "prepare":
            try:
                resp = client.request("textDocument/prepareRename", params)
            except TimeoutError as exc:
                results.record("rename", probe["desc"], False, str(exc))
                continue

            result = resp.get("result")
            if probe.get("expected_null"):
                ok = result is None
                detail = "null as expected" if ok else f"expected null, got {result!r}"
                results.record("rename", probe["desc"], ok, detail)
                continue

            if not isinstance(result, dict):
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"expected object, got {type(result).__name__}: {result!r}",
                )
                continue

            rng = result.get("range") or {}
            start = rng.get("start") or {}
            end = rng.get("end") or {}
            actual = (
                start.get("line"),
                start.get("character"),
                end.get("character"),
            )
            if end.get("line") != start.get("line"):
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"range spans multiple lines: {rng!r}",
                )
                continue
            expected = probe["expected_range"]
            if actual != expected:
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"expected range {expected}, got {actual}",
                )
                continue
            placeholder = result.get("placeholder")
            if placeholder != probe["expected_placeholder"]:
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"placeholder {placeholder!r} != {probe['expected_placeholder']!r}",
                )
                continue
            results.record(
                "rename",
                probe["desc"],
                True,
                f"range={actual} placeholder={placeholder!r}",
            )
            continue

        if probe_type == "rename":
            params["newName"] = probe["new_name"]
            try:
                resp = client.request("textDocument/rename", params)
            except TimeoutError as exc:
                results.record("rename", probe["desc"], False, str(exc))
                continue

            if probe.get("expect_error"):
                err = resp.get("error")
                if not isinstance(err, dict):
                    results.record(
                        "rename",
                        probe["desc"],
                        False,
                        f"expected error, got result={resp.get('result')!r}",
                    )
                    continue
                code = err.get("code")
                if code != probe["expected_error_code"]:
                    results.record(
                        "rename",
                        probe["desc"],
                        False,
                        f"error code {code} != {probe['expected_error_code']}",
                    )
                    continue
                results.record(
                    "rename",
                    probe["desc"],
                    True,
                    f"error code {code}: {err.get('message')!r}",
                )
                continue

            result = resp.get("result")
            if probe.get("expected_null"):
                if result is None:
                    results.record(
                        "rename",
                        probe["desc"],
                        True,
                        "null as expected",
                    )
                else:
                    results.record(
                        "rename",
                        probe["desc"],
                        False,
                        f"expected null, got {result!r}",
                    )
                continue
            if not isinstance(result, dict):
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"expected object, got {type(result).__name__}: {result!r}",
                )
                continue
            changes = result.get("changes") or {}
            edits = changes.get(uri)
            if not isinstance(edits, list):
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"no edits for uri: changes={changes!r}",
                )
                continue
            actual = set()
            all_newtext_ok = True
            for e in edits:
                rng = e.get("range") or {}
                s = rng.get("start") or {}
                en = rng.get("end") or {}
                if s.get("line") != en.get("line"):
                    all_newtext_ok = False
                    break
                actual.add((s.get("line"), s.get("character"), en.get("character")))
                if e.get("newText") != probe["new_name"]:
                    all_newtext_ok = False
                    break
            if not all_newtext_ok:
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"malformed edit in {edits!r}",
                )
                continue
            expected = probe["expected_edits"]
            if actual != expected:
                results.record(
                    "rename",
                    probe["desc"],
                    False,
                    f"expected edits {sorted(expected)}, got {sorted(actual)}",
                )
                continue
            results.record(
                "rename",
                probe["desc"],
                True,
                f"{len(actual)} edits at {sorted(actual)}",
            )
            continue

        results.record("rename", probe["desc"], False, f"unknown probe type: {probe_type}")


def run_hover_probes(client: LspClient, results: Results):
    if not HOVER_PROBES:
        return
    if not HAPROXY_CONF.exists():
        results.record("hover", "fixture present", False, f"missing: {HAPROXY_CONF}")
        return
    uri = path_to_uri(HAPROXY_CONF)
    client.did_open(uri, HAPROXY_CONF.read_text())

    for probe in HOVER_PROBES:
        try:
            resp = client.request(
                "textDocument/hover",
                {
                    "textDocument": {"uri": uri},
                    "position": {
                        "line": probe["line"],
                        "character": probe["character"],
                    },
                },
            )
        except TimeoutError as exc:
            results.record("hover", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        if probe.get("expected_null"):
            ok = result is None
            detail = "null as expected" if ok else f"expected null, got {result!r}"
            results.record("hover", probe["desc"], ok, detail)
            continue

        if not isinstance(result, dict):
            results.record(
                "hover",
                probe["desc"],
                False,
                f"expected object, got {type(result).__name__}: {result!r}",
            )
            continue
        contents = result.get("contents") or {}
        if not isinstance(contents, dict) or contents.get("kind") != "markdown":
            results.record(
                "hover",
                probe["desc"],
                False,
                f"expected markdown MarkupContent, got {contents!r}",
            )
            continue
        value = contents.get("value") or ""
        missing = [s for s in probe["expected_contains"] if s not in value]
        if missing:
            preview = value.replace("\n", "\\n")[:120]
            results.record(
                "hover",
                probe["desc"],
                False,
                f"missing {missing!r} in {preview!r}",
            )
            continue
        results.record(
            "hover",
            probe["desc"],
            True,
            f"matched {len(probe['expected_contains'])} substrings",
        )


def run_completion_probes(client: LspClient, results: Results):
    if not COMPLETION_PROBES:
        return

    opened_uris: dict[str, str] = {}
    fixtures = {
        "conf": HAPROXY_CONF,
        "cfg": HAPROXY_CFG,
    }
    for key, path in fixtures.items():
        if not any(p["fixture"] == key for p in COMPLETION_PROBES):
            continue
        if not path.exists():
            results.record("completion", f"fixture present: {key}", False, f"missing: {path}")
            continue
        uri = path_to_uri(path)
        client.did_open(uri, path.read_text())
        opened_uris[key] = uri

    # Inline fixture for the use_server scoping probe.
    if any(p["fixture"] == "use_server" for p in COMPLETION_PROBES):
        client.did_open(
            COMPLETION_FIXTURE_USE_SERVER_URI,
            COMPLETION_FIXTURE_USE_SERVER_TEXT,
        )
        opened_uris["use_server"] = COMPLETION_FIXTURE_USE_SERVER_URI

    for probe in COMPLETION_PROBES:
        uri = opened_uris.get(probe["fixture"])
        if uri is None:
            results.record("completion", probe["desc"], False, "fixture not opened")
            continue

        try:
            resp = client.request(
                "textDocument/completion",
                {
                    "textDocument": {"uri": uri},
                    "position": {
                        "line": probe["line"],
                        "character": probe["character"],
                    },
                },
            )
        except TimeoutError as exc:
            results.record("completion", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        # Spec allows either `CompletionList` or `CompletionItem[]`; we return
        # the list form so unwrap `.items`.
        items: list = []
        if isinstance(result, dict):
            items = result.get("items") or []
        elif isinstance(result, list):
            items = result

        actual_labels = {item.get("label") for item in items}

        if "expected_min_labels_of_kind" in probe:
            spec = probe["expected_min_labels_of_kind"]
            of_kind = [i for i in items if i.get("kind") == spec["kind"]]
            ok = len(of_kind) >= spec["min"]
            detail = (
                f"{len(of_kind)} items of kind {spec['kind']} (need ≥{spec['min']})"
            )
            results.record("completion", probe["desc"], ok, detail)
            continue

        expected = probe["expected_labels"]
        missing = expected - actual_labels
        if missing:
            preview = ", ".join(sorted(actual_labels))[:120]
            results.record(
                "completion",
                probe["desc"],
                False,
                f"missing {sorted(missing)}; got {preview!r}",
            )
            continue

        if "expected_missing" in probe:
            forbidden = probe["expected_missing"] & actual_labels
            if forbidden:
                results.record(
                    "completion",
                    probe["desc"],
                    False,
                    f"forbidden labels leaked: {sorted(forbidden)}",
                )
                continue

        if "expected_kind" in probe:
            wanted = probe["expected_kind"]
            mismatched = [
                i.get("label")
                for i in items
                if i.get("label") in expected and i.get("kind") != wanted
            ]
            if mismatched:
                results.record(
                    "completion",
                    probe["desc"],
                    False,
                    f"kind mismatch on {mismatched}: expected {wanted}",
                )
                continue

        results.record(
            "completion",
            probe["desc"],
            True,
            f"{len(items)} items, {len(expected)} expected labels present",
        )


def run_diagnostics_probes(client: LspClient, results: Results):
    """Drive `textDocument/publishDiagnostics` and assert observed payloads.

    Task 1 baseline: every clean file must publish an empty diagnostics
    array so stale marks clear on the client. Tasks 2 and 3 will extend
    this with undefined-reference, unused-symbol, and structural probes.
    """
    # Use an inline minimal config so this probe stays stable even as the
    # on-disk fixtures gain intentionally-broken lines in later tasks.
    clean_cfg = (
        "global\n"
        "    daemon\n"
        "\n"
        "defaults\n"
        "    mode http\n"
        "\n"
        "backend web\n"
        "    server s1 127.0.0.1:8080\n"
        "\n"
        "frontend fe\n"
        "    bind *:80\n"
        "    default_backend web\n"
    )
    fake_uri = "file:///tmp/haproxy-lsp-diag-clean.cfg"

    prev_version = client.diagnostics_version(fake_uri)
    client.did_open(fake_uri, clean_cfg)
    try:
        diags = client.wait_for_diagnostics(fake_uri, min_version=prev_version + 1)
    except TimeoutError as exc:
        results.record("diagnostics", "clean file publishes empty array", False, str(exc))
        return

    ok = diags == []
    detail = "empty array as expected" if ok else f"unexpected diagnostics: {diags!r}"
    results.record("diagnostics", "clean file publishes empty array", ok, detail)


def run_declaration_probes(client: LspClient, results: Results):
    if not DECLARATION_PROBES:
        return
    client.did_open(DECLARATION_FIXTURE_URI, DECLARATION_FIXTURE_TEXT)

    for probe in DECLARATION_PROBES:
        try:
            resp = client.request(
                "textDocument/declaration",
                {
                    "textDocument": {"uri": DECLARATION_FIXTURE_URI},
                    "position": {
                        "line": probe["acl_line"],
                        "character": probe["acl_char"],
                    },
                },
            )
        except TimeoutError as exc:
            results.record("declaration", probe["desc"], False, str(exc))
            continue

        result = resp.get("result")
        if not isinstance(result, list):
            results.record(
                "declaration",
                probe["desc"],
                False,
                f"expected list, got {type(result).__name__}: {result!r}",
            )
            continue

        actual_lines = {loc["range"]["start"]["line"] for loc in result}
        expected = probe["expected_ref_lines"]
        missing = expected - actual_lines
        ok = not missing
        if ok:
            detail = f"ref lines {sorted(actual_lines)}"
        else:
            detail = f"missing {sorted(missing)}; got {sorted(actual_lines)}"
        results.record("declaration", probe["desc"], ok, detail)


def main() -> int:
    parser = argparse.ArgumentParser(description="haproxy-lsp integration probes")
    parser.add_argument(
        "--binary",
        default=str(DEFAULT_BINARY),
        help="Path to haproxy-lsp binary (default: ./bin/haproxy-lsp)",
    )
    args = parser.parse_args()

    binary = Path(args.binary)
    if not binary.exists():
        print(f"error: LSP binary not found at {binary}", file=sys.stderr)
        print("hint: run ./build.sh first", file=sys.stderr)
        return 2

    results = Results()
    client = LspClient(binary)
    try:
        client.initialize()
        client.initialized()
        run_definition_probes(client, results)
        run_definition_null_probes(client, results)
        run_folding_probes(client, results)
        run_document_symbol_probes(client, results)
        run_declaration_probes(client, results)
        run_references_probes(client, results)
        run_rename_probes(client, results)
        run_hover_probes(client, results)
        run_completion_probes(client, results)
        run_diagnostics_probes(client, results)
    finally:
        client.shutdown()

    results.print()
    return 0 if results.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
