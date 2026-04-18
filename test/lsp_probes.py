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
        self._lock = threading.Lock()
        self._reader_thread = threading.Thread(target=self._reader_loop, daemon=True)
        self._reader_thread.start()

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
        "desc": "ACL name in `if !acl` style condition",
        "line": 33,
        "character": 55,
        "expected_def_line": 31,
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
        "desc": "conf: final section fold extends to EOF",
        "fixture": "conf",
        "match": "contains",
        "expected": {"startLine": 58, "endLine": 74, "kind": "region"},
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
    # --- haproxy.prod.cfg: the real 1190-line fixture ---
    {
        "desc": "prod.cfg: section fold of `defaults`",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 35, "endLine": 50, "kind": "region"},
    },
    {
        "desc": "prod.cfg: final section fold reaches last line",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 1171, "endLine": 1189, "kind": "region"},
    },
    {
        "desc": "prod.cfg: BEGIN/END `Rate limit for login` region",
        "fixture": "cfg",
        "match": "contains",
        "expected": {"startLine": 59, "endLine": 62, "kind": "region"},
    },
    # --- edge case: URI never opened returns [] ---
    {
        "desc": "unopened URI returns empty fold list",
        "fixture": "unopened",
        "match": "absent",
        "expected": None,
    },
]

# Populated in Task 5.
DOCUMENT_SYMBOL_PROBES: list[dict] = []


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


def run_document_symbol_probes(client: LspClient, results: Results):
    # Populated in Task 5. No-op for Task 1.
    if not DOCUMENT_SYMBOL_PROBES:
        return
    # Placeholder: real implementation lands with Task 5.
    raise NotImplementedError("DOCUMENT_SYMBOL_PROBES runner not yet implemented")


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
        run_folding_probes(client, results)
        run_document_symbol_probes(client, results)
    finally:
        client.shutdown()

    results.print()
    return 0 if results.failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
