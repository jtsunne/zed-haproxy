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
FRAGMENTS_DIR = REPO_ROOT / "test" / "fragments"
FRAGMENTS_MAIN = FRAGMENTS_DIR / "main.cfg"
FRAGMENTS_BACKENDS = FRAGMENTS_DIR / "backends.cfg"
FRAGMENTS_TOML = FRAGMENTS_DIR / ".zed" / "haproxy.toml"
# Scoped-include fixture: a root file whose `.include` sits inside a backend
# body, pointing at a header-less fragment. Exercises inheritance of the
# enclosing section scope across include boundaries.
FRAGMENTS_SCOPED_MAIN = FRAGMENTS_DIR / "scoped-main.cfg"
FRAGMENTS_SCOPED_SERVERS = FRAGMENTS_DIR / "scoped-servers.cfg"


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

    def initialize(self, workspace_root: str | None = None):
        params: dict = {"capabilities": {}}
        if workspace_root is not None:
            params["initializationOptions"] = {"workspace_root": workspace_root}
        return self.request("initialize", params)

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

    def did_close(self, uri: str):
        self.notify(
            "textDocument/didClose",
            {"textDocument": {"uri": uri}},
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


DIAGNOSTICS_PROBES: list[dict] = [
    # Task 2: undefined-reference errors against test/haproxy.conf fixtures
    # appended below the `# --- regression: undefined-reference diagnostics` marker.
    # Range tuple: (line, start_char, end_char). Severity 1 = Error.
    {
        "desc": "conf: undefined-backend on `use_backend diag_missing_backend`",
        "code": "undefined-backend",
        "severity": 1,
        "range": (181, 14, 181, 34),
        "message_contains": "diag_missing_backend",
    },
    {
        "desc": "conf: undefined-backend on `default_backend diag_missing_backend2`",
        "code": "undefined-backend",
        "severity": 1,
        "range": (182, 18, 182, 39),
        "message_contains": "diag_missing_backend2",
    },
    {
        "desc": "conf: undefined-acl on `if diag_missing_acl`",
        "code": "undefined-acl",
        "severity": 1,
        "range": (183, 33, 183, 49),
        "message_contains": "diag_missing_acl",
    },
    {
        "desc": "conf: undefined-server on `use_server diag_missing_srv`",
        "code": "undefined-server",
        "severity": 1,
        "range": (188, 13, 188, 29),
        "message_contains": "diag_missing_srv",
    },
    # Task 3: unused-symbol / duplicate / structural diagnostics.
    {
        "desc": "conf: unused-backend on `backend diag_unused_backend`",
        "code": "unused-backend",
        "severity": 2,
        "range": (192, 8, 192, 27),
        "message_contains": "diag_unused_backend",
    },
    {
        "desc": "conf: unused-acl on `acl diag_unused_acl`",
        "code": "unused-acl",
        "severity": 2,
        "range": (199, 6, 199, 21),
        "message_contains": "diag_unused_acl",
    },
    {
        "desc": "conf: duplicate-section on second `backend diag_dup_section`",
        "code": "duplicate-section",
        "severity": 1,
        "range": (206, 8, 206, 24),
        "message_contains": "diag_dup_section",
    },
    {
        "desc": "conf: duplicate-acl on repeated `acl diag_dup_acl`",
        "code": "duplicate-acl",
        "severity": 1,
        "range": (214, 6, 214, 18),
        "message_contains": "diag_dup_acl",
    },
    {
        "desc": "conf: missing-default-backend on `frontend diag_no_backend_frontend`",
        "code": "missing-default-backend",
        "severity": 2,
        "range": (217, 9, 217, 33),
        "message_contains": "diag_no_backend_frontend",
    },
]


def _diag_range_tuple(diag: dict) -> tuple:
    r = diag.get("range") or {}
    s = r.get("start") or {}
    e = r.get("end") or {}
    return (s.get("line"), s.get("character"), e.get("line"), e.get("character"))


def run_diagnostics_probes(client: LspClient, results: Results):
    """Drive `textDocument/publishDiagnostics` and assert observed payloads.

    Task 1 baseline: every clean file publishes an empty diagnostics array.
    Task 2 adds undefined-reference assertions against test/haproxy.conf
    — each probe matches by (code, severity, range, message substring).
    Task 3 will extend this with unused-symbol and structural probes.
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

    # Task 2: fixture-driven undefined-reference probes.
    if not DIAGNOSTICS_PROBES:
        return
    if not HAPROXY_CONF.exists():
        results.record(
            "diagnostics", "fixture present", False, f"missing: {HAPROXY_CONF}"
        )
        return

    conf_uri = path_to_uri(HAPROXY_CONF)
    prev_version = client.diagnostics_version(conf_uri)
    client.did_open(conf_uri, HAPROXY_CONF.read_text())
    try:
        conf_diags = client.wait_for_diagnostics(conf_uri, min_version=prev_version + 1)
    except TimeoutError as exc:
        results.record("diagnostics", "conf fixture publish", False, str(exc))
        return

    for probe in DIAGNOSTICS_PROBES:
        expected_range = probe["range"]
        match = None
        for d in conf_diags:
            if d.get("code") != probe["code"]:
                continue
            if d.get("severity") != probe["severity"]:
                continue
            if _diag_range_tuple(d) != expected_range:
                continue
            if "message_contains" in probe:
                msg = d.get("message") or ""
                if probe["message_contains"] not in msg:
                    continue
            match = d
            break

        if match is not None:
            src = match.get("source")
            ok = src == "haproxy-lsp"
            detail = (
                f"matched code={probe['code']} range={expected_range} source={src!r}"
                if ok
                else f"matched but source={src!r} (expected 'haproxy-lsp')"
            )
            results.record("diagnostics", probe["desc"], ok, detail)
        else:
            candidates = [
                (d.get("code"), _diag_range_tuple(d)) for d in conf_diags
            ]
            results.record(
                "diagnostics",
                probe["desc"],
                False,
                f"no match for code={probe['code']} range={expected_range}; got {candidates}",
            )

    # Assert `TRUE` built-in ACL (line 188) is NOT flagged as undefined-acl.
    true_flagged = any(
        d.get("code") == "undefined-acl"
        and (d.get("range") or {}).get("start", {}).get("line") == 188
        for d in conf_diags
    )
    results.record(
        "diagnostics",
        "conf: built-in `TRUE` ACL is not flagged as undefined",
        not true_flagged,
        "not flagged" if not true_flagged else "unexpectedly flagged",
    )


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


def run_project_info_probes(client: LspClient, results: Results):
    """Exercise `$/haproxy/projectInfo` against the `test/fragments/` fixture.

    Opening `main.cfg` must resolve the project root through the fixture's
    `.zed/haproxy.toml`; the cached config must then be readable via the
    introspection request. These assertions cover Task 4's discovery path;
    Task 5+ will layer on cross-file index assertions over the same fixture.
    """
    if not FRAGMENTS_MAIN.exists():
        results.record("project-info", "fragments fixture present", False, f"missing: {FRAGMENTS_MAIN}")
        return

    main_uri = path_to_uri(FRAGMENTS_MAIN)
    client.did_open(main_uri, FRAGMENTS_MAIN.read_text())

    try:
        resp = client.request(
            "$/haproxy/projectInfo",
            {"textDocument": {"uri": main_uri}},
        )
    except TimeoutError as exc:
        results.record("project-info", "projectInfo request returns", False, str(exc))
        return

    result = resp.get("result")
    if not isinstance(result, dict):
        results.record(
            "project-info",
            "projectInfo payload is an object",
            False,
            f"got {type(result).__name__}: {result!r}",
        )
        return

    # Project root resolves relative to the config file's parent (the
    # fragments dir), with `project_root = "."` in the TOML.
    expected_root = str(FRAGMENTS_DIR.resolve())
    actual_root = Path(result.get("project_root", "")).resolve()
    ok = str(actual_root) == expected_root
    results.record(
        "project-info",
        "fragments project_root resolves to fragments dir",
        ok,
        f"got {actual_root}" if ok else f"expected {expected_root}, got {actual_root}",
    )

    results.record(
        "project-info",
        "fragments follow_includes == true",
        result.get("follow_includes") is True,
        f"got {result.get('follow_includes')!r}",
    )

    extra = result.get("extra_files") or []
    ok_extra = extra == ["extras/*.cfg"]
    results.record(
        "project-info",
        "fragments extra_files parsed from TOML",
        ok_extra,
        f"got {extra!r}",
    )

    expected_cfg = str(FRAGMENTS_TOML.resolve())
    actual_cfg = result.get("config_file")
    ok_cfg = isinstance(actual_cfg, str) and str(Path(actual_cfg).resolve()) == expected_cfg
    results.record(
        "project-info",
        "fragments config_file points to .zed/haproxy.toml",
        ok_cfg,
        f"got {actual_cfg!r}",
    )

    # When no config file is discoverable, the defaults must apply and
    # `project_root` falls back to the opened file's directory.
    from tempfile import TemporaryDirectory
    with TemporaryDirectory() as tmp:
        tmp_path = Path(tmp) / "loose.cfg"
        tmp_path.write_text("backend loose\n    server s1 127.0.0.1:80\n")
        loose_uri = path_to_uri(tmp_path)
        client.did_open(loose_uri, tmp_path.read_text())
        try:
            loose_resp = client.request(
                "$/haproxy/projectInfo",
                {"textDocument": {"uri": loose_uri}},
            )
        except TimeoutError as exc:
            results.record("project-info", "defaults projectInfo returns", False, str(exc))
            return
        loose_result = loose_resp.get("result") or {}
        default_root = Path(loose_result.get("project_root", "")).resolve()
        ok_def = default_root == Path(tmp).resolve()
        results.record(
            "project-info",
            "no config -> project_root defaults to file's directory",
            ok_def,
            f"got {default_root}" if ok_def else f"expected {Path(tmp).resolve()}, got {default_root}",
        )
        ok_def_cfg = loose_result.get("config_file") is None
        results.record(
            "project-info",
            "no config -> config_file is null",
            ok_def_cfg,
            f"got {loose_result.get('config_file')!r}",
        )


def run_cross_file_probes(client: LspClient, results: Results):
    """Exercise Task 5's include-graph + project index.

    Fixture: `test/fragments/main.cfg` uses `.include backends.cfg`. Opening
    `main.cfg` must walk the include graph, parse `backends.cfg` from disk,
    and aggregate its symbols into the project index. A follow-up `didChange`
    of `main.cfg` after the sibling's on-disk content changes must refresh
    the index to reflect the new sibling symbols.
    """
    if not FRAGMENTS_MAIN.exists() or not FRAGMENTS_BACKENDS.exists():
        results.record(
            "cross-file",
            "fragments fixture present",
            False,
            f"missing: {FRAGMENTS_MAIN} or {FRAGMENTS_BACKENDS}",
        )
        return

    original_backends = FRAGMENTS_BACKENDS.read_text()
    main_uri = path_to_uri(FRAGMENTS_MAIN)
    backends_uri = path_to_uri(FRAGMENTS_BACKENDS)

    try:
        client.did_open(main_uri, FRAGMENTS_MAIN.read_text())

        try:
            resp = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": main_uri}},
            )
        except TimeoutError as exc:
            results.record("cross-file", "projectIndex responds", False, str(exc))
            return
        result = resp.get("result")
        if not isinstance(result, dict):
            results.record(
                "cross-file",
                "projectIndex payload is an object",
                False,
                f"got {type(result).__name__}: {result!r}",
            )
            return

        uris = result.get("uris") or []
        ok_uris = main_uri in uris and backends_uri in uris
        results.record(
            "cross-file",
            "opening main.cfg includes backends.cfg in index",
            ok_uris,
            f"uris={uris}",
        )

        symbols = result.get("symbols") or []
        def has_symbol(name: str, kind: str, uri: str) -> bool:
            return any(
                s.get("name") == name and s.get("kind") == kind and s.get("uri") == uri
                for s in symbols
            )

        results.record(
            "cross-file",
            "index contains `backend be_web` from backends.cfg",
            has_symbol("be_web", "Backend", backends_uri),
            f"symbols for backends.cfg: {[s for s in symbols if s.get('uri') == backends_uri]}",
        )
        results.record(
            "cross-file",
            "index contains `server web1` scoped to be_web",
            any(
                s.get("name") == "web1"
                and s.get("kind") == "Server"
                and s.get("uri") == backends_uri
                and s.get("scope") == "be_web"
                for s in symbols
            ),
            "web1 present" ,
        )
        results.record(
            "cross-file",
            "index contains `frontend fe_main` from main.cfg",
            has_symbol("fe_main", "Frontend", main_uri),
            "fe_main present",
        )

        # Task 5 "changing backends.cfg on disk and sending didChange for
        # main.cfg refreshes the index": write a new backend into
        # backends.cfg on disk, then fire a didChange for main.cfg with
        # unchanged content. The LSP must re-read the sibling from disk.
        updated_backends = original_backends + (
            "\nbackend be_refresh\n    server refreshed 10.0.0.9:9000\n"
        )
        FRAGMENTS_BACKENDS.write_text(updated_backends)

        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": main_uri, "version": 2},
                "contentChanges": [{"text": FRAGMENTS_MAIN.read_text()}],
            },
        )

        try:
            resp2 = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": main_uri}},
            )
        except TimeoutError as exc:
            results.record("cross-file", "projectIndex after didChange responds", False, str(exc))
            return
        result2 = resp2.get("result") or {}
        symbols2 = result2.get("symbols") or []
        results.record(
            "cross-file",
            "didChange on main.cfg refreshes sibling (picks up be_refresh)",
            any(
                s.get("name") == "be_refresh"
                and s.get("kind") == "Backend"
                and s.get("uri") == backends_uri
                for s in symbols2
            ),
            f"found symbols for backends.cfg: {[s.get('name') for s in symbols2 if s.get('uri') == backends_uri]}",
        )
    finally:
        # Restore the fixture so re-runs start from a known state.
        FRAGMENTS_BACKENDS.write_text(original_backends)


def run_cross_file_navigation_probes(client: LspClient, results: Results):
    """Exercise Task 6: cross-file definition, declaration, references,
    rename, F12-on-include-path, and cross-file-aware undefined-reference
    diagnostics.

    Fixture layout:
        test/fragments/main.cfg
          0: global
          1:     daemon
          2:
          3: defaults
          4:     mode http
          5:     timeout connect 5s
          6:     timeout client  30s
          7:     timeout server  30s
          8:
          9: .include backends.cfg
         10:
         11: frontend fe_main
         12:     bind *:80
         13:     default_backend be_web

        test/fragments/backends.cfg
          0: backend be_web
          1:     mode http
          2:     server web1 10.0.0.1:8080
          3:     server web2 10.0.0.2:8080
    """
    if not FRAGMENTS_MAIN.exists() or not FRAGMENTS_BACKENDS.exists():
        results.record(
            "cross-file-nav",
            "fragments fixture present",
            False,
            f"missing: {FRAGMENTS_MAIN} or {FRAGMENTS_BACKENDS}",
        )
        return

    main_uri = path_to_uri(FRAGMENTS_MAIN)
    backends_uri = path_to_uri(FRAGMENTS_BACKENDS)

    client.did_open(main_uri, FRAGMENTS_MAIN.read_text())

    # 1. Cross-file definition: cursor on `be_web` in `default_backend be_web`
    #    (main.cfg line 13 col 22) must jump to backends.cfg line 0.
    try:
        resp = client.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": main_uri},
                "position": {"line": 13, "character": 22},
            },
        )
        loc = resp.get("result")
        ok = (
            isinstance(loc, dict)
            and loc.get("uri") == backends_uri
            and loc.get("range", {}).get("start", {}).get("line") == 0
        )
        results.record(
            "cross-file-nav",
            "definition on `default_backend be_web` lands in backends.cfg line 0",
            ok,
            f"got {loc!r}",
        )
    except TimeoutError as exc:
        results.record("cross-file-nav", "cross-file definition responds", False, str(exc))

    # 2. F12 on `.include backends.cfg` path token (main.cfg line 9 col 12).
    try:
        resp = client.request(
            "textDocument/definition",
            {
                "textDocument": {"uri": main_uri},
                "position": {"line": 9, "character": 12},
            },
        )
        loc = resp.get("result")
        ok = (
            isinstance(loc, dict)
            and loc.get("uri") == backends_uri
            and loc.get("range", {}).get("start", {}).get("line") == 0
            and loc.get("range", {}).get("start", {}).get("character") == 0
        )
        results.record(
            "cross-file-nav",
            "F12 on `.include backends.cfg` path jumps to backends.cfg {0,0}",
            ok,
            f"got {loc!r}",
        )
    except TimeoutError as exc:
        results.record("cross-file-nav", "include-path F12 responds", False, str(exc))

    # 3. Cross-file rename: renaming `be_web` at backends.cfg line 0 col 10
    #    must produce TextEdits in both backends.cfg and main.cfg.
    client.did_open(backends_uri, FRAGMENTS_BACKENDS.read_text())
    try:
        resp = client.request(
            "textDocument/rename",
            {
                "textDocument": {"uri": backends_uri},
                "position": {"line": 0, "character": 10},
                "newName": "be_renamed",
            },
        )
        changes = (resp.get("result") or {}).get("changes") or {}
        main_edits = changes.get(main_uri) or []
        backends_edits = changes.get(backends_uri) or []
        main_ok = any(
            e.get("newText") == "be_renamed"
            and e.get("range", {}).get("start", {}).get("line") == 13
            for e in main_edits
        )
        backends_ok = any(
            e.get("newText") == "be_renamed"
            and e.get("range", {}).get("start", {}).get("line") == 0
            for e in backends_edits
        )
        results.record(
            "cross-file-nav",
            "rename of `be_web` produces edit in backends.cfg (definition)",
            backends_ok,
            f"backends.cfg edits: {backends_edits}",
        )
        results.record(
            "cross-file-nav",
            "rename of `be_web` produces edit in main.cfg (reference)",
            main_ok,
            f"main.cfg edits: {main_edits}",
        )
    except TimeoutError as exc:
        results.record("cross-file-nav", "cross-file rename responds", False, str(exc))

    # 4. Cross-file references: cursor on the `be_web` DEFINITION in
    #    backends.cfg must list the reference in main.cfg (and, with
    #    includeDeclaration=true, the definition itself).
    try:
        resp = client.request(
            "textDocument/references",
            {
                "textDocument": {"uri": backends_uri},
                "position": {"line": 0, "character": 10},
                "context": {"includeDeclaration": False},
            },
        )
        locs = resp.get("result") or []
        main_hits = [
            l
            for l in locs
            if l.get("uri") == main_uri
            and l.get("range", {}).get("start", {}).get("line") == 13
        ]
        results.record(
            "cross-file-nav",
            "references on `be_web` def includes main.cfg call-site",
            bool(main_hits),
            f"locations: {locs}",
        )
    except TimeoutError as exc:
        results.record("cross-file-nav", "cross-file references responds", False, str(exc))

    # 5. Undefined-reference diagnostics: the `default_backend be_web`
    #    line in main.cfg must NOT be flagged because `be_web` is defined in
    #    backends.cfg.
    diags = client._diagnostics.get(main_uri) or []
    flagged = any(
        d.get("code") == "undefined-backend"
        and d.get("range", {}).get("start", {}).get("line") == 13
        for d in diags
    )
    results.record(
        "cross-file-nav",
        "cross-file be_web not flagged as undefined in main.cfg",
        not flagged,
        f"diagnostics: {diags}",
    )


def run_scoped_include_probes(client: LspClient, results: Results):
    """Exercise section-scope inheritance across `.include` boundaries.

    Fixture layout:
        test/fragments/scoped-main.cfg
          0: backend be_scoped
          1:     mode http
          2:     .include scoped-servers.cfg

        test/fragments/scoped-servers.cfg
          0: server scoped_s1 10.0.2.1:9000
          1: server scoped_s2 10.0.2.2:9000
          2: stick-table type ip size 100k expire 30s

    `scoped-servers.cfg` has no section header of its own — when HAProxy
    evaluates the config, `.include` is textual substitution so the fragment
    executes inside `backend be_scoped`'s body. The LSP must mirror that:

      - `server scoped_s1` and `server scoped_s2` must be indexed with
        `scope = "be_scoped"` (not `None`), so rename / references /
        `use_server` resolution across sections stay correct.
      - The bare `stick-table` directive must bind a StickTable symbol
        named `be_scoped` — before the fix, a header-less fragment dropped
        the stick-table entirely since no section name was tracked.
    """
    if not FRAGMENTS_SCOPED_MAIN.exists() or not FRAGMENTS_SCOPED_SERVERS.exists():
        results.record(
            "scoped-include",
            "scoped fixture present",
            False,
            f"missing: {FRAGMENTS_SCOPED_MAIN} or {FRAGMENTS_SCOPED_SERVERS}",
        )
        return

    main_uri = path_to_uri(FRAGMENTS_SCOPED_MAIN)
    servers_uri = path_to_uri(FRAGMENTS_SCOPED_SERVERS)

    client.did_open(main_uri, FRAGMENTS_SCOPED_MAIN.read_text())

    try:
        resp = client.request(
            "$/haproxy/projectIndex",
            {"textDocument": {"uri": main_uri}},
        )
    except TimeoutError as exc:
        results.record("scoped-include", "projectIndex responds", False, str(exc))
        return

    result = resp.get("result") or {}
    symbols = result.get("symbols") or []
    fragment_symbols = [s for s in symbols if s.get("uri") == servers_uri]

    def find_symbol(name: str, kind: str) -> dict | None:
        for s in fragment_symbols:
            if s.get("name") == name and s.get("kind") == kind:
                return s
        return None

    s1 = find_symbol("scoped_s1", "Server")
    results.record(
        "scoped-include",
        "server scoped_s1 in header-less include inherits backend scope",
        s1 is not None and s1.get("scope") == "be_scoped",
        f"got {s1!r}",
    )

    s2 = find_symbol("scoped_s2", "Server")
    results.record(
        "scoped-include",
        "server scoped_s2 in header-less include inherits backend scope",
        s2 is not None and s2.get("scope") == "be_scoped",
        f"got {s2!r}",
    )

    tbl = find_symbol("be_scoped", "StickTable")
    results.record(
        "scoped-include",
        "bare stick-table in fragment binds to parent section name",
        tbl is not None,
        f"fragment symbols: {[(s.get('kind'), s.get('name')) for s in fragment_symbols]}",
    )


def run_cross_file_diagnostics_probes(client: LspClient, results: Results):
    """Exercise cross-file diagnostic rules. Each probe builds an isolated
    project under a fresh temp directory with a `.zed/haproxy.toml` so the
    files participate in cross-file aggregation without cross-polluting
    unrelated tests.

    Covers four failure modes from the codex review:
      1. `undefined-acl` must NOT fire for an ACL defined in a sibling.
      2. `unused-acl` must NOT fire for an ACL whose only references live
         on the far side of an `.include`.
      3. `duplicate-section` MUST fire when two files in the same project
         declare the same `(keyword, name)` section.
      4. `duplicate-acl` MUST fire when two fragments pulled into the same
         parent section both define the same `acl NAME`.
    """
    from tempfile import TemporaryDirectory

    def setup_project(tmp: str, files: dict[str, str]) -> dict[str, str]:
        """Write `files` (relative path → content) into tmp plus a
        `.zed/haproxy.toml` anchoring the project root. Returns a
        {relative_path: absolute_uri} map.
        """
        zed_dir = Path(tmp) / ".zed"
        zed_dir.mkdir()
        (zed_dir / "haproxy.toml").write_text(
            'project_root = "."\nfollow_includes = true\n'
        )
        uris: dict[str, str] = {}
        for rel, body in files.items():
            p = Path(tmp) / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
            uris[rel] = path_to_uri(p)
        return uris

    def wait_diags(uri: str, timeout: float = 5.0) -> list[dict]:
        try:
            return client.wait_for_diagnostics(uri, timeout=timeout)
        except TimeoutError:
            return client._diagnostics.get(uri, [])

    # --- Probe 1: ACL defined in sibling, referenced in main — no
    # `undefined-acl` on the referring file.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                "frontend fe\n"
                "    bind *:80\n"
                "    .include acls.cfg\n"
                "    http-request deny if bad_ip\n"
                "    default_backend be\n"
                "backend be\n"
                "    server s1 127.0.0.1:1\n"
            ),
            "acls.cfg": "acl bad_ip src 1.2.3.4\n",
        })
        prev = client.diagnostics_version(uris["main.cfg"])
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            client.wait_for_diagnostics(uris["main.cfg"], min_version=prev + 1)
        except TimeoutError:
            pass
        diags = client._diagnostics.get(uris["main.cfg"], [])
        flagged = [d for d in diags if d.get("code") == "undefined-acl"]
        results.record(
            "xfile-diagnostics",
            "ACL defined in sibling suppresses undefined-acl on referring file",
            not flagged,
            f"diagnostics: {diags!r}",
        )

    # --- Probe 2: ACL defined in main, referenced only in sibling — no
    # `unused-acl` on main.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                "frontend fe\n"
                "    bind *:80\n"
                "    acl allow_net src 10.0.0.0/8\n"
                "    .include rules.cfg\n"
                "    default_backend be\n"
                "backend be\n"
                "    server s1 127.0.0.1:1\n"
            ),
            "rules.cfg": "    http-request deny unless allow_net\n",
        })
        prev = client.diagnostics_version(uris["main.cfg"])
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            client.wait_for_diagnostics(uris["main.cfg"], min_version=prev + 1)
        except TimeoutError:
            pass
        diags = client._diagnostics.get(uris["main.cfg"], [])
        flagged = [
            d for d in diags
            if d.get("code") == "unused-acl"
            and "allow_net" in (d.get("message") or "")
        ]
        results.record(
            "xfile-diagnostics",
            "ACL referenced only in sibling suppresses unused-acl in definer",
            not flagged,
            f"diagnostics: {diags!r}",
        )

    # --- Probe 3: Two files in same project both declare `backend be_dup`.
    # The canonical-first (lower (uri, line) tuple) keeps its range clean;
    # the later file MUST get a `duplicate-section` diagnostic.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                "backend be_dup\n"
                "    server s1 127.0.0.1:1\n"
                ".include other.cfg\n"
            ),
            "other.cfg": (
                "backend be_dup\n"
                "    server s9 127.0.0.9:9\n"
            ),
        })
        prev_main = client.diagnostics_version(uris["main.cfg"])
        prev_other = client.diagnostics_version(uris["other.cfg"])
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            client.wait_for_diagnostics(uris["main.cfg"], min_version=prev_main + 1)
            client.wait_for_diagnostics(uris["other.cfg"], min_version=prev_other + 1, timeout=2.0)
        except TimeoutError:
            pass
        main_diags = client._diagnostics.get(uris["main.cfg"], [])
        other_diags = client._diagnostics.get(uris["other.cfg"], [])
        other_flagged = [d for d in other_diags if d.get("code") == "duplicate-section"]
        main_flagged = [d for d in main_diags if d.get("code") == "duplicate-section"]
        results.record(
            "xfile-diagnostics",
            "cross-file duplicate backend flagged in the later file",
            bool(other_flagged),
            f"other.cfg diagnostics: {other_diags!r}",
        )
        results.record(
            "xfile-diagnostics",
            "cross-file duplicate: canonical-first file stays clean",
            not main_flagged,
            f"main.cfg diagnostics: {main_diags!r}",
        )

    # --- Probe 4: Two fragments pulled into the same frontend both define
    # `acl dup_acl`. Each fragment alone sees only one def; cross-file
    # aggregation MUST catch the duplicate.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                "frontend fe\n"
                "    bind *:80\n"
                "    .include frag_a.cfg\n"
                "    .include frag_b.cfg\n"
                "    default_backend be\n"
                "backend be\n"
                "    server s1 127.0.0.1:1\n"
            ),
            "frag_a.cfg": "    acl dup_acl src 1.2.3.4\n",
            "frag_b.cfg": "    acl dup_acl src 5.6.7.8\n",
        })
        prev_main = client.diagnostics_version(uris["main.cfg"])
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            client.wait_for_diagnostics(uris["main.cfg"], min_version=prev_main + 1)
            client.wait_for_diagnostics(uris["frag_b.cfg"], timeout=2.0)
        except TimeoutError:
            pass
        frag_b_diags = client._diagnostics.get(uris["frag_b.cfg"], [])
        frag_a_diags = client._diagnostics.get(uris["frag_a.cfg"], [])
        b_flagged = [d for d in frag_b_diags if d.get("code") == "duplicate-acl"]
        a_flagged = [d for d in frag_a_diags if d.get("code") == "duplicate-acl"]
        results.record(
            "xfile-diagnostics",
            "cross-fragment duplicate-acl flagged in the later fragment",
            bool(b_flagged),
            f"frag_b.cfg diagnostics: {frag_b_diags!r}",
        )
        results.record(
            "xfile-diagnostics",
            "cross-fragment duplicate-acl: canonical-first fragment stays clean",
            not a_flagged,
            f"frag_a.cfg diagnostics: {frag_a_diags!r}",
        )


def run_extra_files_glob_probes(client: LspClient, results: Results):
    """Exercise `extra_files` glob expansion in `.zed/haproxy.toml`.

    Historically `extra_files` accepted only literal paths — patterns with
    `*`, `?`, or `**` were stored but silently dropped from the include
    graph, so a common setup like `extra_files = ["conf.d/*.cfg"]` made
    those files invisible to symbols, navigation, diagnostics, and rename.

    These probes verify:
      - A `*`-glob inside a literal subdir (`conf.d/*.cfg`) pulls every
        matching file into the project index.
      - A recursive `**`-glob (`**/*.cfg`) pulls files from nested
        subdirectories.
      - A glob that matches nothing on disk stays inert (no crash, no
        spurious entries).
    """
    from tempfile import TemporaryDirectory

    def setup_project(tmp: str, extra_files_toml: str, files: dict[str, str]) -> dict[str, str]:
        zed_dir = Path(tmp) / ".zed"
        zed_dir.mkdir()
        (zed_dir / "haproxy.toml").write_text(
            'project_root = "."\nfollow_includes = true\n'
            f'extra_files = {extra_files_toml}\n'
        )
        uris: dict[str, str] = {}
        for rel, body in files.items():
            p = Path(tmp) / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
            uris[rel] = path_to_uri(p)
        return uris

    # --- Probe 1: `conf.d/*.cfg` pulls every matching sibling.
    with TemporaryDirectory() as tmp:
        uris = setup_project(
            tmp,
            '["conf.d/*.cfg"]',
            {
                "main.cfg": (
                    "frontend fe\n"
                    "    bind *:80\n"
                    "    default_backend be_glob_one\n"
                ),
                "conf.d/one.cfg": (
                    "backend be_glob_one\n"
                    "    server s1 127.0.0.1:1\n"
                ),
                "conf.d/two.cfg": (
                    "backend be_glob_two\n"
                    "    server s2 127.0.0.2:2\n"
                ),
                # Non-matching extension — must be ignored.
                "conf.d/ignore.txt": "not a cfg\n",
            },
        )
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            resp = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": uris["main.cfg"]}},
            )
        except TimeoutError as exc:
            results.record("extra-files-glob", "projectIndex responds", False, str(exc))
            return
        result = resp.get("result") or {}
        indexed_uris = set(result.get("uris") or [])
        indexed_symbols = result.get("symbols") or []

        results.record(
            "extra-files-glob",
            "conf.d/one.cfg pulled in via *-glob",
            uris["conf.d/one.cfg"] in indexed_uris,
            f"uris={sorted(indexed_uris)!r}",
        )
        results.record(
            "extra-files-glob",
            "conf.d/two.cfg pulled in via *-glob",
            uris["conf.d/two.cfg"] in indexed_uris,
            f"uris={sorted(indexed_uris)!r}",
        )
        results.record(
            "extra-files-glob",
            "conf.d/ignore.txt excluded (extension doesn't match)",
            uris["conf.d/ignore.txt"] not in indexed_uris,
            f"uris={sorted(indexed_uris)!r}",
        )
        results.record(
            "extra-files-glob",
            "be_glob_two symbol from globbed sibling reachable via project index",
            any(
                s.get("name") == "be_glob_two"
                and s.get("kind") == "Backend"
                and s.get("uri") == uris["conf.d/two.cfg"]
                for s in indexed_symbols
            ),
            f"symbols in conf.d/two.cfg: "
            f"{[s for s in indexed_symbols if s.get('uri') == uris['conf.d/two.cfg']]}",
        )

        # Cross-file navigation through the glob: F12 on the root file's
        # `default_backend be_glob_one` must resolve to conf.d/one.cfg.
        try:
            def_resp = client.request(
                "textDocument/definition",
                {
                    "textDocument": {"uri": uris["main.cfg"]},
                    # Line 2 is `    default_backend be_glob_one` — column
                    # 22 sits inside the name token.
                    "position": {"line": 2, "character": 22},
                },
            )
        except TimeoutError as exc:
            results.record("extra-files-glob", "definition request responds", False, str(exc))
            return
        def_result = def_resp.get("result")
        results.record(
            "extra-files-glob",
            "F12 on backend reference resolves into globbed sibling",
            isinstance(def_result, dict) and def_result.get("uri") == uris["conf.d/one.cfg"],
            f"got {def_result!r}",
        )

    # --- Probe 2: `**/*.cfg` pulls files from nested directories.
    with TemporaryDirectory() as tmp:
        uris = setup_project(
            tmp,
            '["**/*.cfg"]',
            {
                "main.cfg": (
                    "frontend fe\n"
                    "    bind *:80\n"
                    "    default_backend be_root\n"
                ),
                "nested/deeper/leaf.cfg": (
                    "backend be_root\n"
                    "    server s1 127.0.0.1:1\n"
                ),
            },
        )
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            resp = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": uris["main.cfg"]}},
            )
        except TimeoutError as exc:
            results.record("extra-files-glob", "recursive glob responds", False, str(exc))
            return
        indexed_uris = set((resp.get("result") or {}).get("uris") or [])
        results.record(
            "extra-files-glob",
            "recursive **/*.cfg glob reaches nested directory",
            uris["nested/deeper/leaf.cfg"] in indexed_uris,
            f"uris={sorted(indexed_uris)!r}",
        )

    # --- Probe 3: glob that matches nothing must stay inert.
    with TemporaryDirectory() as tmp:
        uris = setup_project(
            tmp,
            '["missing/*.cfg"]',
            {
                "main.cfg": (
                    "backend be_solo\n"
                    "    server s1 127.0.0.1:1\n"
                ),
            },
        )
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        try:
            resp = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": uris["main.cfg"]}},
            )
        except TimeoutError as exc:
            results.record("extra-files-glob", "empty-glob responds", False, str(exc))
            return
        indexed_uris = set((resp.get("result") or {}).get("uris") or [])
        results.record(
            "extra-files-glob",
            "non-matching glob leaves project index containing only main.cfg",
            indexed_uris == {uris["main.cfg"]},
            f"uris={sorted(indexed_uris)!r}",
        )


def run_didclose_eviction_probes(client: LspClient, results: Results):
    """Exercise that `didClose` evicts auto-loaded siblings along with the
    closed root, not just the closed URI alone.

    Scenario:
        main.cfg `.include`s sibling.cfg. Opening main.cfg pulls sibling.cfg
        into every per-URI cache (symbols, project_configs, project_indices)
        even though the client never opened it directly. A subsequent
        `didClose main.cfg` must evict sibling.cfg too — otherwise:
          - the project index keeps advertising the sibling's symbols
          - `workspace/symbol` keeps returning them
          - long-lived sessions leak unbounded sibling state

    Each case uses an isolated tempdir with its own `.zed/haproxy.toml` so
    the project boundary is explicit and no cross-test pollution occurs.
    """
    from tempfile import TemporaryDirectory

    def setup_project(tmp: str, files: dict[str, str]) -> dict[str, str]:
        zed_dir = Path(tmp) / ".zed"
        zed_dir.mkdir()
        (zed_dir / "haproxy.toml").write_text(
            'project_root = "."\nfollow_includes = true\n'
        )
        uris: dict[str, str] = {}
        for rel, body in files.items():
            p = Path(tmp) / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
            uris[rel] = path_to_uri(p)
        return uris

    def project_index_uris(client: LspClient, uri: str) -> set:
        try:
            resp = client.request(
                "$/haproxy/projectIndex",
                {"textDocument": {"uri": uri}},
            )
        except TimeoutError:
            return set()
        return set((resp.get("result") or {}).get("uris") or [])

    # --- Case 1: closing the only open root must drop the auto-loaded sibling.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                "frontend fe\n"
                "    bind *:80\n"
                "    .include sibling.cfg\n"
                "    default_backend be_sib\n"
            ),
            "sibling.cfg": (
                "backend be_sib\n"
                "    server s1 127.0.0.1:1\n"
            ),
        })
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())

        pre_close = project_index_uris(client, uris["main.cfg"])
        results.record(
            "didclose-eviction",
            "open root + sibling both visible in project index",
            uris["main.cfg"] in pre_close and uris["sibling.cfg"] in pre_close,
            f"pre-close uris={sorted(pre_close)!r}",
        )

        client.did_close(uris["main.cfg"])
        # Wait for eviction side effects: an empty-diagnostics publish is
        # sent per evicted URI, so poll until the sibling's cached diagnostics
        # go empty (or timeout).
        import time
        deadline = time.time() + 2.0
        while time.time() < deadline:
            if client._diagnostics.get(uris["sibling.cfg"]) == []:
                break
            time.sleep(0.02)

        # The only remaining workspace symbols from this project must NOT
        # include the sibling. Use workspace/symbol with a distinctive query.
        try:
            ws_resp = client.request("workspace/symbol", {"query": "be_sib"})
        except TimeoutError as exc:
            results.record("didclose-eviction", "workspace/symbol responds", False, str(exc))
            return
        ws_items = ws_resp.get("result") or []
        sibling_hits = [
            it for it in ws_items
            if (it.get("location") or {}).get("uri") == uris["sibling.cfg"]
        ]
        results.record(
            "didclose-eviction",
            "workspace/symbol no longer returns evicted sibling's symbols",
            not sibling_hits,
            f"hits={sibling_hits!r}",
        )

        # Re-open an unrelated file under the same tmp project to probe
        # whether any stale cache still holds the sibling's URI. Use a
        # fresh file so the project index rebuild doesn't re-import the
        # sibling from disk unless the include graph actually pulls it.
        standalone = Path(tmp) / "standalone.cfg"
        standalone.write_text("backend be_standalone\n    server s 127.0.0.1:2\n")
        standalone_uri = path_to_uri(standalone)
        client.did_open(standalone_uri, standalone.read_text())
        post_close = project_index_uris(client, standalone_uri)
        results.record(
            "didclose-eviction",
            "re-opened unrelated root sees no leftover sibling in project index",
            uris["sibling.cfg"] not in post_close,
            f"post-close uris={sorted(post_close)!r}",
        )
        client.did_close(standalone_uri)

    # --- Case 2: a sibling that's also explicitly opened must NOT be evicted
    # when its transitive parent is closed. Eviction must only remove
    # auto-loaded siblings, not client-owned buffers.
    with TemporaryDirectory() as tmp:
        uris = setup_project(tmp, {
            "main.cfg": (
                ".include sibling.cfg\n"
                "frontend fe\n"
                "    bind *:80\n"
                "    default_backend be_client_owned\n"
            ),
            "sibling.cfg": (
                "backend be_client_owned\n"
                "    server s1 127.0.0.1:1\n"
            ),
        })
        client.did_open(uris["main.cfg"], Path(tmp, "main.cfg").read_text())
        client.did_open(uris["sibling.cfg"], Path(tmp, "sibling.cfg").read_text())
        client.did_close(uris["main.cfg"])

        # Sibling is still owned by the client — its symbols must remain
        # reachable via workspace/symbol and the per-URI cache.
        try:
            ws_resp = client.request("workspace/symbol", {"query": "be_client_owned"})
        except TimeoutError as exc:
            results.record("didclose-eviction", "workspace/symbol responds (case 2)", False, str(exc))
            return
        ws_items = ws_resp.get("result") or []
        hits = [
            it for it in ws_items
            if (it.get("location") or {}).get("uri") == uris["sibling.cfg"]
        ]
        results.record(
            "didclose-eviction",
            "sibling that is also explicitly opened survives parent's didClose",
            bool(hits),
            f"hits={hits!r}",
        )
        client.did_close(uris["sibling.cfg"])


def run_workspace_symbol_probes(client: LspClient, results: Results):
    """Exercise Task 7's `workspace/symbol` provider.

    The handler enumerates every known symbol across all opened URIs
    (including include-graph siblings) and does a case-insensitive substring
    match on the query string. Empty query returns up to WORKSPACE_SYMBOL_CAP
    entries so Zed can stream.

    Assumes prior probes have opened `test/haproxy.prod.cfg` and the
    `test/fragments/` pair. Those did_opens run earlier in `main`, so the
    per-URI symbol cache is already populated by the time we query.
    """
    prod_uri = path_to_uri(HAPROXY_CFG)
    main_uri = path_to_uri(FRAGMENTS_MAIN)
    backends_uri = path_to_uri(FRAGMENTS_BACKENDS)

    # Ensure prod.cfg is opened — document-symbol probes do this, but guard
    # in case probe ordering ever changes.
    if HAPROXY_CFG.exists():
        client.did_open(prod_uri, HAPROXY_CFG.read_text())

    # --- 1. query `opcart` must return `backend opcart-direct`.
    try:
        resp = client.request(
            "workspace/symbol",
            {"query": "opcart"},
        )
    except TimeoutError as exc:
        results.record("workspace-symbol", "opcart query responds", False, str(exc))
        return

    items = resp.get("result")
    if not isinstance(items, list):
        results.record(
            "workspace-symbol",
            "workspace/symbol returns a list",
            False,
            f"got {type(items).__name__}: {items!r}",
        )
        return
    results.record(
        "workspace-symbol",
        "workspace/symbol returns a list",
        True,
        f"{len(items)} items",
    )

    opcart_direct = [
        it
        for it in items
        if it.get("name") == "opcart-direct"
        and it.get("kind") == 5  # Class / Backend
        and (it.get("location") or {}).get("uri") == prod_uri
    ]
    results.record(
        "workspace-symbol",
        "`opcart` query finds `backend opcart-direct` in prod.cfg",
        bool(opcart_direct),
        f"matched: {opcart_direct[:1]}",
    )

    # Every returned item must contain a substring of the query.
    bad = [it for it in items if "opcart" not in (it.get("name") or "").lower()]
    results.record(
        "workspace-symbol",
        "every result name contains `opcart` (case-insensitive)",
        not bad,
        f"violators: {bad[:3]}" if bad else "all match",
    )

    # --- 2. cross-file query across test/fragments/: `be_` returns symbols
    # from both main.cfg (fe_main references) and backends.cfg (be_web).
    if FRAGMENTS_MAIN.exists() and FRAGMENTS_BACKENDS.exists():
        client.did_open(main_uri, FRAGMENTS_MAIN.read_text())

        try:
            resp = client.request("workspace/symbol", {"query": "be_"})
        except TimeoutError as exc:
            results.record("workspace-symbol", "be_ query responds", False, str(exc))
            return
        items = resp.get("result") or []

        be_web = [
            it
            for it in items
            if it.get("name") == "be_web"
            and (it.get("location") or {}).get("uri") == backends_uri
        ]
        results.record(
            "workspace-symbol",
            "`be_` query finds `backend be_web` in backends.cfg",
            bool(be_web),
            f"matched: {be_web[:1]}",
        )

    # --- 3. case-insensitive match.
    try:
        resp = client.request("workspace/symbol", {"query": "OPCART"})
    except TimeoutError as exc:
        results.record("workspace-symbol", "case-insensitive query responds", False, str(exc))
        return
    items = resp.get("result") or []
    results.record(
        "workspace-symbol",
        "case-insensitive query `OPCART` still finds opcart-direct",
        any(it.get("name") == "opcart-direct" for it in items),
        f"{len(items)} items",
    )

    # --- 4. containerName populated for Server symbols (scope = backend).
    try:
        resp = client.request("workspace/symbol", {"query": "web1"})
    except TimeoutError as exc:
        results.record("workspace-symbol", "web1 query responds", False, str(exc))
        return
    items = resp.get("result") or []
    web1_hits = [
        it
        for it in items
        if it.get("name") == "web1"
        and it.get("kind") == 8  # Field / Server
        and (it.get("location") or {}).get("uri") == backends_uri
    ]
    results.record(
        "workspace-symbol",
        "web1 server has containerName = be_web",
        bool(web1_hits) and web1_hits[0].get("containerName") == "be_web",
        f"hit: {web1_hits[:1]}",
    )

    # --- 5. empty query returns a bounded but non-empty list.
    try:
        resp = client.request("workspace/symbol", {"query": ""})
    except TimeoutError as exc:
        results.record("workspace-symbol", "empty query responds", False, str(exc))
        return
    items = resp.get("result") or []
    results.record(
        "workspace-symbol",
        "empty query returns non-empty list (caps at 1000)",
        len(items) > 0 and len(items) <= 1000,
        f"{len(items)} items",
    )


def run_diagnostics_latency_probe(client: LspClient, results: Results):
    """Task 8 acceptance: measure didChange -> publishDiagnostics round-trip.

    The target is <=200ms on `test/haproxy.prod.cfg` (1188 lines). The probe
    times three consecutive didChange cycles and records the best measurement
    to dampen noise from the OS scheduler and the Python reader thread.
    """
    import time

    if not HAPROXY_CFG.exists():
        results.record(
            "latency", "prod.cfg fixture present", False, f"missing: {HAPROXY_CFG}"
        )
        return

    uri = path_to_uri(HAPROXY_CFG)
    text = HAPROXY_CFG.read_text()

    # Ensure the document is opened; prior probes likely did this already,
    # but re-opening is idempotent for the server and simplifies the probe.
    prev = client.diagnostics_version(uri)
    client.did_open(uri, text)
    try:
        client.wait_for_diagnostics(uri, min_version=prev + 1, timeout=5.0)
    except TimeoutError as exc:
        results.record("latency", "initial publishDiagnostics", False, str(exc))
        return

    best_ms = None
    for i in range(3):
        # Mutate slightly so the server treats this as a real change. Appending
        # a harmless blank comment line keeps semantics stable but forces a
        # full re-parse and re-publish.
        mutated = text + f"\n# latency probe iteration {i}\n"
        prev = client.diagnostics_version(uri)
        start = time.monotonic()
        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": 2 + i},
                "contentChanges": [{"text": mutated}],
            },
        )
        try:
            client.wait_for_diagnostics(uri, min_version=prev + 1, timeout=5.0)
        except TimeoutError as exc:
            results.record("latency", f"didChange iter {i}", False, str(exc))
            return
        elapsed_ms = (time.monotonic() - start) * 1000
        if best_ms is None or elapsed_ms < best_ms:
            best_ms = elapsed_ms

    assert best_ms is not None
    threshold_ms = 200.0
    ok = best_ms <= threshold_ms
    results.record(
        "latency",
        f"didChange -> publishDiagnostics on prod.cfg <= {threshold_ms:.0f}ms",
        ok,
        f"best={best_ms:.1f}ms (3 iters)",
    )


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
        client.initialize(workspace_root=str(REPO_ROOT))
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
        run_project_info_probes(client, results)
        run_cross_file_probes(client, results)
        run_cross_file_navigation_probes(client, results)
        run_scoped_include_probes(client, results)
        run_cross_file_diagnostics_probes(client, results)
        run_extra_files_glob_probes(client, results)
        run_didclose_eviction_probes(client, results)
        run_workspace_symbol_probes(client, results)
        run_diagnostics_latency_probe(client, results)
    finally:
        client.shutdown()

    results.print()
    return 0 if results.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
