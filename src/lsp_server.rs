use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

mod docs;

// Project-level configuration discovered from `.zed/haproxy.toml` (or defaults
// when no config file is found). One config is resolved per opened document
// and cached by URI. Future tasks (cross-file index, workspace symbols) will
// consult `follow_includes` / `extra_files` to decide which sibling files to
// pull into the project index.
#[derive(Debug, Clone)]
struct ProjectConfig {
    project_root: PathBuf,
    follow_includes: bool,
    extra_files: Vec<String>,
    // Path to the `.zed/haproxy.toml` file that produced this config, if any.
    // `None` means defaults were used (no config discovered).
    config_file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    kind: SymbolKind,
    range: Range,
    uri: String,
    references: Vec<Reference>,
    // Enclosing section name for symbols whose identity is scoped to a
    // section body (currently: `Server`). Two backends may both declare a
    // `server web1`, and these are distinct entities — name matching alone
    // would cross-link them. `None` means global identity (sections, ACLs,
    // stick-tables).
    scope: Option<String>,
}

#[derive(Debug, Clone)]
struct Reference {
    range: Range,
    uri: String,
    context: ReferenceContext,
    // Enclosing section name for references whose target resolution depends
    // on the call site's section (currently: `UseServer`). A `use_server
    // web1` line in backend A refers to that backend's `web1`, not to any
    // other backend's same-named server.
    scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ReferenceContext {
    UseBackend,
    DefaultBackend,
    UseServer,
    AclCondition,
    AclUnlessCondition,
    StickTable,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SymbolKind {
    Backend,
    Frontend,
    Listen,
    Acl,
    Server,
    StickTable,
}

// Serialize `SymbolKind` as a stable string for introspection endpoints.
// Numeric `DocumentSymbol.kind` values are LSP-defined and reused elsewhere;
// the project index speaks its own schema and benefits from legible names.
fn symbol_kind_name(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Backend => "Backend",
        SymbolKind::Frontend => "Frontend",
        SymbolKind::Listen => "Listen",
        SymbolKind::Acl => "Acl",
        SymbolKind::Server => "Server",
        SymbolKind::StickTable => "StickTable",
    }
}

#[derive(Debug, Clone)]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone)]
struct Position {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone)]
struct FoldingRange {
    start_line: u32,
    end_line: u32,
    kind: &'static str,
}

// LSP DiagnosticSeverity: 1=Error, 2=Warning, 3=Information, 4=Hint.
// See https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#diagnostic
#[derive(Debug, Clone)]
struct Diagnostic {
    range: Range,
    severity: u8,
    code: &'static str,
    source: &'static str,
    message: String,
}

// LSP numeric SymbolKind values. Kept as u8 for compactness; serialized as u32.
// See https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#symbolKind
#[derive(Debug, Clone)]
struct DocumentSymbol {
    name: String,
    detail: Option<String>,
    kind: u8,
    // `range` spans the logical extent of the symbol (whole section body for
    // sections, whole line for children). `selection_range` covers the
    // identifier token only — Zed targets this for Cmd+Shift+O jump and
    // breadcrumbs. HAProxy identifiers are ASCII per the grammar regex
    // `/[a-zA-Z0-9_.-]+/`, so byte == char == UTF-16 code unit; no conversion
    // needed here, but keep this invariant in mind for any future multi-byte
    // identifier work.
    range: Range,
    selection_range: Range,
    children: Vec<DocumentSymbol>,
}

struct HaproxyLsp {
    symbols: HashMap<String, Vec<Symbol>>,
    folds: HashMap<String, Vec<FoldingRange>>,
    outline: HashMap<String, Vec<DocumentSymbol>>,
    documents: HashMap<String, String>,
    // Per-URI diagnostics cache. Rebuilt at the tail of `parse_document` and
    // published via a `textDocument/publishDiagnostics` notification; an
    // empty Vec is still published so stale diagnostics clear on the client.
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    // Pending outbound notifications. `send_notification` pushes; the main
    // loop drains after `handle_request` returns so framed writes to stdout
    // stay serialized with the single optional response per request frame.
    pending_notifications: Vec<Value>,
    // Workspace root supplied via `initializationOptions.workspace_root`
    // (passed through from Zed's `worktree.root_path()`). Used to cap the
    // upward walk when discovering `.zed/haproxy.toml` so we don't stray
    // outside the opened worktree.
    workspace_root: Option<PathBuf>,
    // Per-URI resolved project configuration. Populated on `didOpen` by
    // `resolve_project_config`; reused by `$/haproxy/projectInfo` and by
    // cross-file resolution in subsequent tasks.
    project_configs: HashMap<String, ProjectConfig>,
    // Per-URI resolved include graph neighbours (file URIs). Populated during
    // `parse_document` from `.include`, `-f`, and `crt` directives. Used to
    // reach sibling files during the recursive graph walk and to aggregate
    // the per-project symbol index.
    included_files: HashMap<String, Vec<String>>,
    // URIs the client has explicitly opened via `textDocument/didOpen`. Kept
    // as a separate set from `self.documents` because the graph walk also
    // caches sibling documents; when re-parsing the graph we want to trust
    // only client-owned buffers for unsaved edits and re-read siblings from
    // disk so edits made out-of-band (e.g. another editor) are picked up.
    explicitly_opened: HashSet<String>,
    // Per-project-root symbol index, keyed by the project-root path string.
    // Populated at the tail of every top-level `parse_document` call after
    // the include graph has been walked; consulted by the cross-file
    // resolution handlers in later tasks (Task 6+) and by the
    // `$/haproxy/projectIndex` introspection request.
    project_indices: HashMap<String, ProjectIndex>,
}

// Aggregate symbol index for all files reachable from a single project root
// via `.include` / `-f` / `crt` resolution. Keyed per (SymbolKind, name) so
// cross-file definition / references / rename lookups can enumerate every
// occurrence without re-scanning per-URI maps.
#[derive(Debug, Clone, Default)]
struct ProjectIndex {
    project_root: PathBuf,
    uris: Vec<String>,
    symbols_by_name: HashMap<(SymbolKind, String), Vec<ProjectSymbolRef>>,
}

#[derive(Debug, Clone)]
struct ProjectSymbolRef {
    uri: String,
    range: Range,
    scope: Option<String>,
}

const SECTION_KEYWORDS: &[&str] = &[
    "global", "defaults", "frontend", "backend", "listen", "resolvers",
    "userlist", "peers", "mailers", "cache", "program", "ring",
];

// Convert a `file://` URI to a filesystem path. Handles the common `file:///`
// triple-slash form on Unix. Percent-decodes a few characters that routinely
// appear in fixture paths (space → `%20`); non-file URIs and malformed inputs
// return `None`. A lightweight decoder is enough here — the LSP only ever
// sees URIs it previously minted or that Zed produced from on-disk paths.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // On Unix `file:///foo/bar` → path `/foo/bar`; on Windows a drive letter
    // would follow. We only target Unix (Zed runs on macOS/Linux).
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

// Convert a filesystem path to a `file://` URI. Mirrors `uri_to_path` — the
// canonicalized absolute path becomes the URI body with spaces percent-encoded
// so the round-trip through `uri_to_path` stays lossless on the (rare) path
// that contains them. Non-canonicalizable paths (does-not-exist, permission)
// yield `None` so callers can skip them from the include graph.
fn path_to_file_uri(p: &Path) -> Option<String> {
    let canon = p.canonicalize().ok()?;
    let s = canon.to_string_lossy();
    let encoded = s.replace(' ', "%20");
    Some(format!("file://{}", encoded))
}

// Strip a surrounding pair of `"` or `'` from an include path token if
// present; otherwise return the slice unchanged. `.include "foo bar.cfg"`
// is not exercised by our fixtures but appears in real configs, so the
// defensive strip keeps us from treating the leading quote as part of the
// filename and then failing to resolve it on disk.
fn unquote_path_token(tok: &str) -> &str {
    let bytes = tok.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &tok[1..tok.len() - 1]
    } else {
        tok
    }
}

// Resolve an include-directive path token against the including file's
// directory first, then the project root. Absolute paths must exist on
// disk to be returned. Returns the resolved path as-is (not canonicalized
// — that happens in `path_to_file_uri` to keep the include graph keyed on
// canonical URIs).
fn resolve_include_path(
    path_tok: &str,
    file_dir: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    if path_tok.is_empty() {
        return None;
    }
    let p = PathBuf::from(path_tok);
    if p.is_absolute() {
        if p.exists() {
            return Some(p);
        }
        return None;
    }
    let candidate = file_dir.join(&p);
    if candidate.exists() {
        return Some(candidate);
    }
    let candidate = project_root.join(&p);
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// Hand-rolled TOML reader supporting exactly the three keys we accept:
// `project_root = "…"`, `follow_includes = true|false`, `extra_files = [..]`.
// Blank lines and `#` comments are ignored. Unrecognized keys are silently
// skipped so users can drop `[section]` headers or future keys without the
// parser failing — keeps the config file forward-compatible.
#[derive(Debug, Default)]
struct RawProjectConfig {
    project_root: Option<String>,
    follow_includes: Option<bool>,
    extra_files: Option<Vec<String>>,
}

fn parse_project_toml(content: &str) -> RawProjectConfig {
    let mut raw = RawProjectConfig::default();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
            continue;
        }
        let (key, value) = match trimmed.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        // Strip a trailing line comment (`= "value" # note`). We only split
        // on `#` when it's not inside a quoted string.
        let value = strip_toml_inline_comment(value);
        match key {
            "project_root" => {
                if let Some(s) = parse_toml_string(value) {
                    raw.project_root = Some(s);
                }
            }
            "follow_includes" => match value {
                "true" => raw.follow_includes = Some(true),
                "false" => raw.follow_includes = Some(false),
                _ => {}
            },
            "extra_files" => {
                if let Some(arr) = parse_toml_string_array(value) {
                    raw.extra_files = Some(arr);
                }
            }
            _ => {}
        }
    }
    raw
}

fn strip_toml_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut in_string = false;
    let mut escape = false;
    let mut end = bytes.len();
    for (i, &b) in bytes.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        match b {
            b'\\' if in_string => escape = true,
            b'"' => in_string = !in_string,
            b'#' if !in_string => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    value[..end].trim()
}

fn parse_toml_string(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return None;
    }
    // Only handle the minimal set of escapes likely to appear in paths.
    let inner = &value[1..value.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn parse_toml_string_array(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut items: Vec<String> = Vec::new();
    // Split on top-level commas (strings here are simple — no nested arrays).
    let mut buf = String::new();
    let mut in_string = false;
    let mut escape = false;
    for c in inner.chars() {
        if escape {
            buf.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                buf.push(c);
                escape = true;
            }
            '"' => {
                buf.push(c);
                in_string = !in_string;
            }
            ',' if !in_string => {
                let token = buf.trim().to_string();
                if !token.is_empty() {
                    if let Some(s) = parse_toml_string(&token) {
                        items.push(s);
                    }
                }
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    let token = buf.trim().to_string();
    if !token.is_empty() {
        if let Some(s) = parse_toml_string(&token) {
            items.push(s);
        }
    }
    Some(items)
}

// Walk up from `start_dir` looking for `.zed/haproxy.toml`. Stops at the
// workspace root (exclusive: we still check the workspace root itself) or at
// the filesystem root. Returns the first config file found, or `None`.
fn discover_project_config_file(
    start_dir: &Path,
    workspace_root: Option<&Path>,
) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(".zed").join("haproxy.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        // Stop once we've checked the workspace root. Do *not* ascend above
        // it — a user may have opened a worktree deeper than $HOME.
        if let Some(root) = workspace_root {
            if dir == root {
                return None;
            }
        }
        cur = dir.parent();
    }
    None
}

// Build a `ProjectConfig` for the file at `file_path`. `project_root` in the
// config file is resolved relative to the config file's parent directory; if
// absent (or no config file at all), it defaults to the file's own directory.
fn resolve_project_config_for_path(
    file_path: &Path,
    workspace_root: Option<&Path>,
) -> ProjectConfig {
    let file_dir = file_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let config_file = discover_project_config_file(&file_dir, workspace_root);

    let (project_root, follow_includes, extra_files) = if let Some(cfg_path) = &config_file {
        match std::fs::read_to_string(cfg_path) {
            Ok(content) => {
                let raw = parse_project_toml(&content);
                let cfg_dir = cfg_path
                    .parent()
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| file_dir.clone());
                let root = match raw.project_root {
                    Some(s) => {
                        let p = PathBuf::from(&s);
                        if p.is_absolute() {
                            p
                        } else {
                            cfg_dir.join(p)
                        }
                    }
                    None => cfg_dir,
                };
                (
                    root,
                    raw.follow_includes.unwrap_or(true),
                    raw.extra_files.unwrap_or_default(),
                )
            }
            Err(_) => (file_dir.clone(), true, Vec::new()),
        }
    } else {
        (file_dir.clone(), true, Vec::new())
    };

    ProjectConfig {
        project_root,
        follow_includes,
        extra_files,
        config_file,
    }
}

fn diagnostic_to_json(d: &Diagnostic) -> Value {
    json!({
        "range": {
            "start": { "line": d.range.start.line, "character": d.range.start.character },
            "end": { "line": d.range.end.line, "character": d.range.end.character },
        },
        "severity": d.severity,
        "code": d.code,
        "source": d.source,
        "message": d.message,
    })
}

fn is_section_header(line: &str) -> bool {
    // Section headers live at column 0; any leading whitespace disqualifies.
    if line.starts_with(|c: char| c.is_whitespace()) {
        return false;
    }
    for kw in SECTION_KEYWORDS {
        if let Some(rest) = line.strip_prefix(*kw) {
            if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
                return true;
            }
        }
    }
    false
}

fn parse_marker(line: &str, keyword: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let after_hash = trimmed.strip_prefix('#')?.trim_start();
    let rest = after_hash.strip_prefix(keyword)?;
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let name = rest.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn compute_folds(content: &str) -> Vec<FoldingRange> {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let mut section_folds: Vec<FoldingRange> = Vec::new();
    let mut comment_folds: Vec<FoldingRange> = Vec::new();
    let mut region_folds: Vec<FoldingRange> = Vec::new();

    // Section folds: open on header, close on line before next header or at EOF.
    let mut open_section_start: Option<u32> = None;
    for (i, line) in lines.iter().enumerate() {
        if is_section_header(line) {
            if let Some(start) = open_section_start {
                let end = (i as u32).saturating_sub(1);
                if end > start {
                    section_folds.push(FoldingRange { start_line: start, end_line: end, kind: "region" });
                }
            }
            open_section_start = Some(i as u32);
        }
    }
    if let Some(start) = open_section_start {
        if line_count > 0 {
            let end = (line_count - 1) as u32;
            if end > start {
                section_folds.push(FoldingRange { start_line: start, end_line: end, kind: "region" });
            }
        }
    }

    // Comment banner folds: runs of 2+ consecutive `#`-prefixed lines.
    let mut banner_start: Option<u32> = None;
    for (i, line) in lines.iter().enumerate() {
        let is_comment = line.trim_start().starts_with('#');
        if is_comment {
            if banner_start.is_none() {
                banner_start = Some(i as u32);
            }
        } else if let Some(start) = banner_start {
            let end = (i as u32).saturating_sub(1);
            if end > start {
                comment_folds.push(FoldingRange { start_line: start, end_line: end, kind: "comment" });
            }
            banner_start = None;
        }
    }
    if let Some(start) = banner_start {
        if line_count > 0 {
            let end = (line_count - 1) as u32;
            if end > start {
                comment_folds.push(FoldingRange { start_line: start, end_line: end, kind: "comment" });
            }
        }
    }

    // BEGIN/END region folds: case-sensitive, exact-name match via stack.
    let mut stack: Vec<(String, u32)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(name) = parse_marker(line, "BEGIN") {
            stack.push((name, i as u32));
        } else if let Some(name) = parse_marker(line, "END") {
            if let Some(pos) = stack.iter().rposition(|(n, _)| n == &name) {
                let (_, start) = stack.remove(pos);
                let end = i as u32;
                if end > start {
                    region_folds.push(FoldingRange { start_line: start, end_line: end, kind: "region" });
                }
            }
        }
    }

    let mut out = section_folds;
    out.extend(comment_folds);
    out.extend(region_folds);
    out
}

/// Extract every stick-table name referenced on a single line.
///
/// Recognised forms (deduplicated in caller since `add_reference_to_symbol`
/// matches by name):
///   - `sc<digit>_<ident>(<name>[, ...])` — counter accessor; first arg is the table.
///   - `stick match <name>` / `stick store-request <name>` / `stick store-response <name>`.
///   - Any occurrence of ` table <name>` (covers `stick on ... table X`,
///     `http-request track-sc0 src table X`, etc.). False positives are
///     absorbed by the symbol-existence check in `add_reference_to_symbol`.
///
/// Caller is expected to have trimmed leading whitespace and skipped comment
/// lines so `#`-commented example text does not contribute references.
/// Collect every stick-table reference on a line along with the byte offset
/// of the table identifier. Callers persist the offset into
/// `Reference.range.start.character` so later narrowing passes anchor directly
/// on the call-site rather than scanning from column 0 (which would latch onto
/// an unrelated same-name identifier appearing earlier on the line, e.g. an
/// ACL `foo` preceding `sc0_http_req_rate(foo)`).
fn collect_stick_table_references(line: &str) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    let bytes = line.as_bytes();

    // sc<digit>_<ident>(<first_arg>, ...)
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if bytes[i] == b's'
            && bytes[i + 1] == b'c'
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3] == b'_'
        {
            // Left word boundary: previous char must not be an identifier char
            // (avoids matching `foosc0_...` or `track-sc0`).
            let left_boundary = i == 0 || {
                let prev = bytes[i - 1];
                !(prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' || prev == b'-')
            };
            if left_boundary {
                if let Some(paren_rel) = line[i..].find('(') {
                    let paren_abs = i + paren_rel;
                    // Function name must be a contiguous identifier up to `(`.
                    let fn_slice = &line[i..paren_abs];
                    if !fn_slice.chars().any(char::is_whitespace) {
                        if let Some(close_rel) = line[paren_abs..].find(')') {
                            let close_abs = paren_abs + close_rel;
                            let inside_start = paren_abs + 1;
                            let inside = &line[inside_start..close_abs];
                            let first_arg_raw = inside.split(',').next().unwrap_or("");
                            let lead_ws = first_arg_raw.len() - first_arg_raw.trim_start().len();
                            let first_arg = first_arg_raw.trim();
                            if !first_arg.is_empty() && is_valid_identifier(first_arg) {
                                out.push((first_arg.to_string(), inside_start + lead_ws));
                            }
                            i = close_abs + 1;
                            continue;
                        }
                    }
                }
            }
        }
        i += 1;
    }

    // `stick match <sample> [table <tbl>]`, `stick store-request <sample> [table <tbl>]`,
    // `stick store-response <sample> [table <tbl>]`. The token immediately
    // after `match`/`store-*` is a sample expression (e.g. `src`), NOT the
    // table name — per HAProxy's grammar the table is carried by the optional
    // `table <name>` clause, handled below. Misparsing the sample as a table
    // name produces false references whenever a table happens to share the
    // name of a sample fetch (e.g. a table called `src`).

    // Generic ` table <name>` anywhere on the line.
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find(" table ") {
        let abs = search_from + rel + " table ".len();
        let tail = &line[abs..];
        if let Some(name) = tail.split_whitespace().next() {
            if is_valid_identifier(name) {
                out.push((name.to_string(), abs));
            }
        }
        search_from = abs;
    }

    out
}

/// Strip a trailing `#...` comment from a line. Returns the slice up to the
/// first `#` preceded by whitespace (or at column 0), mirroring HAProxy's
/// comment rule while leaving `#` embedded inside tokens alone.
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

fn is_valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Locate a word-bounded occurrence of `name` in `line` starting at byte
/// offset `search_from`. Returns (start, end) byte offsets. Because HAProxy
/// identifiers are ASCII per the grammar regex, byte offsets equal char
/// offsets equal UTF-16 code-unit offsets — the returned values can be used
/// directly as LSP `character` positions.
///
/// Word boundaries use the grammar's identifier charset (alphanumeric, `_`,
/// `-`, `.`), so `accountCreationService_10000` does not match inside
/// `app__accountCreationService`, and `backend` does not match inside
/// `use_backend`.
fn find_identifier_range(line: &str, name: &str, search_from: usize) -> Option<(u32, u32)> {
    let bytes = line.as_bytes();
    let name_bytes = name.as_bytes();
    if name_bytes.is_empty() || name_bytes.len() > bytes.len() {
        return None;
    }
    let is_id_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-';
    let mut i = search_from.min(bytes.len());
    while i + name_bytes.len() <= bytes.len() {
        if &bytes[i..i + name_bytes.len()] == name_bytes {
            let left_ok = i == 0 || !is_id_byte(bytes[i - 1]);
            let right_i = i + name_bytes.len();
            let right_ok = right_i == bytes.len() || !is_id_byte(bytes[right_i]);
            if left_ok && right_ok {
                return Some((i as u32, right_i as u32));
            }
        }
        i += 1;
    }
    None
}

/// Byte offset just past the keyword on a symbol's definition line, used as
/// the starting point for `find_identifier_range`. This avoids matching the
/// keyword itself when a user's symbol name happens to collide textually
/// with the keyword (e.g. a backend literally named `backend`).
fn def_line_search_from(line: &str, kind: &SymbolKind) -> Option<usize> {
    let skip = match kind {
        SymbolKind::Backend => "backend",
        SymbolKind::Frontend => "frontend",
        SymbolKind::Listen => "listen",
        SymbolKind::Acl => "acl",
        SymbolKind::Server => "server",
        SymbolKind::StickTable => return None,
    };
    let trimmed_start = line.len() - line.trim_start().len();
    Some(trimmed_start + skip.len())
}

/// Whether a reference context carries its own exact per-reference column.
/// When true, callers should use the stored position directly and bypass the
/// per-`(line, context)` floor that handles multi-occurrence disambiguation —
/// the floor is only needed for contexts where every reference on a shared
/// line currently anchors at column 0 (ACL chains, etc.).
fn ref_context_has_precise_position(ctx: &ReferenceContext) -> bool {
    matches!(ctx, ReferenceContext::StickTable)
}

/// HAProxy built-in anonymous ACL keywords. These are not user-defined ACLs
/// and therefore must not trigger `undefined-acl` diagnostics when referenced
/// in an `if` / `unless` condition. List mirrors the HAProxy docs "ACL anchors
/// and terminators" table plus the handful of anonymous predefined ACLs
/// (`TRUE`, `FALSE`) widely used in production configs.
const BUILTIN_ACL_NAMES: &[&str] = &[
    "FALSE",
    "TRUE",
    "HTTP",
    "HTTP_1.0",
    "HTTP_1.1",
    "HTTP_CONTENT",
    "HTTP_URL_ABSOLUTE",
    "HTTP_URL_SLASH",
    "HTTP_URL_STAR",
    "LOCALHOST",
    "METH_CONNECT",
    "METH_DELETE",
    "METH_GET",
    "METH_HEAD",
    "METH_OPTIONS",
    "METH_POST",
    "METH_PUT",
    "METH_TRACE",
    "RDP_COOKIE",
    "REQ_CONTENT",
    "WAIT_END",
];

/// For a directive line whose first non-whitespace token is `keyword`, locate
/// the immediate argument token and return `(name, start_col, end_col)` in
/// byte offsets (which equal LSP character offsets for ASCII identifiers).
/// Returns `None` when the keyword is absent, not at line start, or when the
/// argument is missing / not a valid HAProxy identifier.
fn find_leading_directive_arg(line: &str, keyword: &str) -> Option<(String, u32, u32)> {
    let trimmed_start = line.len() - line.trim_start().len();
    let after_ws = &line[trimmed_start..];
    let rest = after_ws.strip_prefix(keyword)?;
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let bytes = line.as_bytes();
    let mut i = trimmed_start + keyword.len();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if start == i {
        return None;
    }
    let name = &line[start..i];
    if !is_valid_identifier(name) {
        return None;
    }
    Some((name.to_string(), start as u32, i as u32))
}

/// Scan `content` for references to symbols that aren't defined in
/// `symbols`. Emits three diagnostic rules (severity `Error`):
///
///   - `undefined-backend` on `use_backend NAME [...]` and `default_backend
///     NAME` where `NAME` is not a `Backend` symbol.
///   - `undefined-acl` on `... if NAME` / `... unless NAME` where `NAME` is
///     neither an ACL definition in this file nor a HAProxy built-in
///     (`TRUE`, `FALSE`, `METH_GET`, ...).
///   - `undefined-server` on `use_server NAME [...]` where `NAME` is not a
///     `Server` symbol inside the enclosing backend/listen section. Server
///     identity is section-scoped, so a `server` of the same name in an
///     unrelated section does not silence the diagnostic.
///
/// Scope is tracked by walking section headers with `is_section_header`;
/// comments and the section header line itself are skipped so a
/// `# use_backend foo` sample config line doesn't trip rule #1.
fn undefined_reference_diagnostics(content: &str, symbols: &[Symbol]) -> Vec<Diagnostic> {
    use std::collections::HashSet;

    let backend_names: HashSet<&str> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Backend)
        .map(|s| s.name.as_str())
        .collect();
    let acl_names: HashSet<&str> = symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Acl)
        .map(|s| s.name.as_str())
        .collect();
    let builtin_acls: HashSet<&str> = BUILTIN_ACL_NAMES.iter().copied().collect();
    let mut servers_by_scope: HashMap<String, HashSet<&str>> = HashMap::new();
    for s in symbols {
        if s.kind == SymbolKind::Server {
            if let Some(scope) = &s.scope {
                servers_by_scope
                    .entry(scope.clone())
                    .or_default()
                    .insert(s.name.as_str());
            }
        }
    }

    let mut diags = Vec::new();
    let mut current_section: Option<String> = None;
    for (line_num, raw_line) in content.lines().enumerate() {
        let is_section = is_section_header(raw_line);
        let trimmed = raw_line.trim();
        let first_tok = trimmed.split_whitespace().next().unwrap_or("");
        if is_section {
            match first_tok {
                "backend" | "frontend" | "listen" | "peers" => {
                    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                    current_section = tokens.get(1).map(|s| s.to_string());
                }
                "global" | "defaults" | "resolvers" | "userlist" | "mailers"
                | "cache" | "program" | "ring" => {
                    current_section = None;
                }
                _ => {}
            }
            continue;
        }

        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }

        // Undefined backend: `use_backend NAME` and `default_backend NAME`.
        for keyword in &["use_backend", "default_backend"] {
            if let Some((name, start, end)) = find_leading_directive_arg(raw_line, keyword) {
                if !backend_names.contains(name.as_str()) {
                    diags.push(Diagnostic {
                        range: Range {
                            start: Position { line: line_num as u32, character: start },
                            end: Position { line: line_num as u32, character: end },
                        },
                        severity: 1,
                        code: "undefined-backend",
                        source: "haproxy-lsp",
                        message: format!("Undefined backend: {}", name),
                    });
                }
            }
        }

        // Undefined server: `use_server NAME` inside a backend/listen section.
        if let Some((name, start, end)) = find_leading_directive_arg(raw_line, "use_server") {
            let known = current_section
                .as_ref()
                .and_then(|s| servers_by_scope.get(s))
                .map(|set| set.contains(name.as_str()))
                .unwrap_or(false);
            if !known {
                diags.push(Diagnostic {
                    range: Range {
                        start: Position { line: line_num as u32, character: start },
                        end: Position { line: line_num as u32, character: end },
                    },
                    severity: 1,
                    code: "undefined-server",
                    source: "haproxy-lsp",
                    message: format!("Undefined server: {}", name),
                });
            }
        }

        // Undefined ACL: tokens in ` if ` / ` unless ` conditions that are
        // neither user-defined ACLs in this file nor HAProxy built-ins.
        for keyword in &["if", "unless"] {
            for (name, start, end) in collect_acl_ref_positions(raw_line, keyword) {
                if acl_names.contains(name.as_str())
                    || builtin_acls.contains(name.as_str())
                {
                    continue;
                }
                diags.push(Diagnostic {
                    range: Range {
                        start: Position { line: line_num as u32, character: start },
                        end: Position { line: line_num as u32, character: end },
                    },
                    severity: 1,
                    code: "undefined-acl",
                    source: "haproxy-lsp",
                    message: format!("Undefined ACL: {}", name),
                });
            }
        }
    }

    diags
}

/// Section header metadata used by structural diagnostic passes.
///
/// `name_start` / `name_end` are byte offsets on `header_line` (which equal
/// LSP character offsets — HAProxy identifiers are ASCII per the grammar).
/// `body_end_line` is inclusive and clamped to the line index immediately
/// before the next section header (or the last line of the file for the
/// trailing section). `name` is empty for section kinds that take no name
/// token (`global`, `defaults`).
struct SectionHeaderInfo {
    keyword: String,
    name: String,
    name_start: u32,
    name_end: u32,
    header_line: u32,
    body_end_line: u32,
}

fn collect_section_headers(content: &str) -> Vec<SectionHeaderInfo> {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let mut hdrs: Vec<SectionHeaderInfo> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !is_section_header(line) {
            continue;
        }
        let keyword = match line.split_whitespace().next() {
            Some(k) => k.to_string(),
            None => continue,
        };
        let (name, name_start, name_end) = match find_leading_directive_arg(line, &keyword) {
            Some((n, s, e)) => (n, s, e),
            None => (String::new(), 0u32, 0u32),
        };
        hdrs.push(SectionHeaderInfo {
            keyword,
            name,
            name_start,
            name_end,
            header_line: i as u32,
            body_end_line: 0,
        });
    }
    for i in 0..hdrs.len() {
        let next = if i + 1 < hdrs.len() {
            hdrs[i + 1].header_line.saturating_sub(1)
        } else if line_count > 0 {
            (line_count - 1) as u32
        } else {
            hdrs[i].header_line
        };
        hdrs[i].body_end_line = next;
    }
    hdrs
}

/// Emit a `duplicate-section` error (severity `Error`) on every second-and-later
/// occurrence of a `backend` / `frontend` / `listen` section with a name already
/// declared earlier in the file under the same keyword. Cross-kind collisions
/// (e.g. `backend foo` + `frontend foo`) are not flagged here; HAProxy permits
/// distinct namespaces per keyword in practice.
fn duplicate_section_diagnostics(headers: &[SectionHeaderInfo]) -> Vec<Diagnostic> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut diags = Vec::new();
    for h in headers {
        if !matches!(h.keyword.as_str(), "backend" | "frontend" | "listen") {
            continue;
        }
        if h.name.is_empty() {
            continue;
        }
        let key = (h.keyword.clone(), h.name.clone());
        if !seen.insert(key) {
            diags.push(Diagnostic {
                range: Range {
                    start: Position { line: h.header_line, character: h.name_start },
                    end: Position { line: h.header_line, character: h.name_end },
                },
                severity: 1,
                code: "duplicate-section",
                source: "haproxy-lsp",
                message: format!("Duplicate {} section: {}", h.keyword, h.name),
            });
        }
    }
    diags
}

/// Emit a `duplicate-acl` error on every second-and-later `acl NAME ...` within
/// the same `frontend` / `listen` body. Restricted to frontend/listen per the
/// Tier 3 plan — HAProxy technically permits repeated `acl` lines as an OR
/// shorthand, but in frontends/listens the typical intent of a repeat is a
/// copy-paste mistake that silently overrides condition semantics.
fn duplicate_acl_diagnostics(
    content: &str,
    headers: &[SectionHeaderInfo],
) -> Vec<Diagnostic> {
    use std::collections::HashSet;
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let mut diags = Vec::new();
    for h in headers {
        if !matches!(h.keyword.as_str(), "frontend" | "listen") {
            continue;
        }
        let body_start = (h.header_line as usize) + 1;
        let body_end = (h.body_end_line as usize).min(line_count.saturating_sub(1));
        if body_start > body_end {
            continue;
        }
        let mut seen: HashSet<String> = HashSet::new();
        for ln_idx in body_start..=body_end {
            let line = lines[ln_idx];
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if !trimmed.starts_with("acl ") && trimmed != "acl" {
                continue;
            }
            if let Some((name, start, end)) = find_leading_directive_arg(line, "acl") {
                if !seen.insert(name.clone()) {
                    diags.push(Diagnostic {
                        range: Range {
                            start: Position { line: ln_idx as u32, character: start },
                            end: Position { line: ln_idx as u32, character: end },
                        },
                        severity: 1,
                        code: "duplicate-acl",
                        source: "haproxy-lsp",
                        message: format!("Duplicate ACL in section: {}", name),
                    });
                }
            }
        }
    }
    diags
}

/// Emit a `missing-default-backend` warning for each `frontend` / `listen`
/// that has a `bind` directive (or a `listen NAME addr` inline bind) but
/// neither a `default_backend` nor any `use_backend` directive in its body.
/// Range is anchored on the section name token in the header.
fn missing_default_backend_diagnostics(
    content: &str,
    headers: &[SectionHeaderInfo],
) -> Vec<Diagnostic> {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let mut diags = Vec::new();
    for h in headers {
        if !matches!(h.keyword.as_str(), "frontend" | "listen") {
            continue;
        }
        if h.name.is_empty() {
            continue;
        }
        let body_start = (h.header_line as usize) + 1;
        let body_end = (h.body_end_line as usize).min(line_count.saturating_sub(1));
        let mut has_bind = false;
        let mut has_backend_ref = false;
        // `listen NAME addr[:port]` header form counts as an inline bind.
        if h.keyword == "listen" {
            if let Some(hdr_line) = lines.get(h.header_line as usize) {
                let toks: Vec<&str> = hdr_line.split_whitespace().collect();
                if toks.len() >= 3 && !toks[2].starts_with('#') {
                    has_bind = true;
                }
            }
        }
        if body_start <= body_end {
            for ln_idx in body_start..=body_end {
                let line = lines[ln_idx];
                let trimmed = line.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let tok = trimmed.split_whitespace().next().unwrap_or("");
                if tok == "bind" {
                    has_bind = true;
                }
                if tok == "default_backend" || tok == "use_backend" {
                    has_backend_ref = true;
                }
            }
        }
        if has_bind && !has_backend_ref {
            diags.push(Diagnostic {
                range: Range {
                    start: Position { line: h.header_line, character: h.name_start },
                    end: Position { line: h.header_line, character: h.name_end },
                },
                severity: 2,
                code: "missing-default-backend",
                source: "haproxy-lsp",
                message: format!(
                    "{} '{}' has `bind` but no `default_backend` or `use_backend`",
                    h.keyword, h.name
                ),
            });
        }
    }
    diags
}

/// Emit an `unused-backend` warning for every `backend` symbol whose reference
/// list is empty. Stick-table accessors (`sc0_*(name)`, `stick match name`)
/// attach to the `StickTable` symbol rather than the enclosing backend, so a
/// backend whose sole purpose is carrying a `stick-table` is still flagged —
/// callers are expected to reference the backend via `use_backend` somewhere
/// if they want the warning suppressed.
fn unused_backend_diagnostics(content: &str, symbols: &[Symbol]) -> Vec<Diagnostic> {
    let lines: Vec<&str> = content.lines().collect();
    let mut diags = Vec::new();
    for s in symbols {
        if s.kind != SymbolKind::Backend || !s.references.is_empty() {
            continue;
        }
        let line_idx = s.range.start.line as usize;
        let line = match lines.get(line_idx) {
            Some(l) => l,
            None => continue,
        };
        if let Some((_, start, end)) = find_leading_directive_arg(line, "backend") {
            diags.push(Diagnostic {
                range: Range {
                    start: Position { line: line_idx as u32, character: start },
                    end: Position { line: line_idx as u32, character: end },
                },
                severity: 2,
                code: "unused-backend",
                source: "haproxy-lsp",
                message: format!("Unused backend: {}", s.name),
            });
        }
    }
    diags
}

/// Emit an `unused-acl` warning for every `acl NAME ...` defined inside a
/// `frontend` / `listen` / `backend` body where no `if` / `unless` condition
/// in the same section body references `NAME`. Scope is enforced at the
/// section level to avoid cross-section false negatives: two sections each
/// defining `acl foo` don't suppress each other's warning.
///
/// Only the first occurrence of a duplicated ACL name within a section is
/// considered for this rule; the duplicates are already reported by
/// `duplicate_acl_diagnostics`.
fn unused_acl_diagnostics(
    content: &str,
    headers: &[SectionHeaderInfo],
) -> Vec<Diagnostic> {
    use std::collections::HashSet;
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();
    let mut diags = Vec::new();
    for h in headers {
        if !matches!(h.keyword.as_str(), "frontend" | "listen" | "backend") {
            continue;
        }
        let body_start = (h.header_line as usize) + 1;
        let body_end = (h.body_end_line as usize).min(line_count.saturating_sub(1));
        if body_start > body_end {
            continue;
        }
        let mut defs: Vec<(String, u32, u32, u32)> = Vec::new();
        let mut refs: HashSet<String> = HashSet::new();
        for ln_idx in body_start..=body_end {
            let line = lines[ln_idx];
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with("acl ") || trimmed == "acl" {
                if let Some((name, start, end)) = find_leading_directive_arg(line, "acl") {
                    defs.push((name, ln_idx as u32, start, end));
                }
            }
            for kw in &["if", "unless"] {
                for (name, _, _) in collect_acl_ref_positions(line, kw) {
                    refs.insert(name);
                }
            }
        }
        let mut seen: HashSet<String> = HashSet::new();
        for (name, ln, start, end) in defs {
            if !seen.insert(name.clone()) {
                continue;
            }
            if refs.contains(&name) {
                continue;
            }
            diags.push(Diagnostic {
                range: Range {
                    start: Position { line: ln, character: start },
                    end: Position { line: ln, character: end },
                },
                severity: 2,
                code: "unused-acl",
                source: "haproxy-lsp",
                message: format!("Unused ACL: {}", name),
            });
        }
    }
    diags
}

/// Collect ACL identifier references on a line under an `if` / `unless`
/// condition, with precise column ranges for each occurrence. Mirrors the
/// token-filtering semantics of `extract_acl_names_from_condition`
/// (brace-wrapped sample expressions skipped, operators `!` / `&&` / `||`
/// dropped, leading `!` negation stripped) but preserves positions for
/// diagnostic range reporting.
fn collect_acl_ref_positions(line: &str, keyword: &str) -> Vec<(String, u32, u32)> {
    let pattern = format!(" {} ", keyword);
    let cond_start = match line.find(&pattern) {
        Some(i) => i + pattern.len(),
        None => return Vec::new(),
    };
    let cond_slice = strip_inline_comment(&line[cond_start..]);
    let cond_end = cond_start + cond_slice.len();

    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut brace_depth: u32 = 0;
    let mut i = cond_start;
    while i < cond_end {
        while i < cond_end && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= cond_end {
            break;
        }
        let tok_start = i;
        while i < cond_end && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let tok_end = i;
        let tok = &line[tok_start..tok_end];
        if tok == "{" || tok == "!{" {
            brace_depth += 1;
            continue;
        }
        if tok == "}" {
            brace_depth = brace_depth.saturating_sub(1);
            continue;
        }
        if brace_depth > 0 {
            continue;
        }
        if tok == "||" || tok == "&&" || tok == "!" {
            continue;
        }
        let (name_start, name) = if let Some(stripped) = tok.strip_prefix('!') {
            (tok_start + 1, stripped)
        } else {
            (tok_start, tok)
        };
        if name.is_empty() || !is_valid_identifier(name) {
            continue;
        }
        out.push((name.to_string(), name_start as u32, tok_end as u32));
    }
    out
}

/// Byte offset just past the reference-context keyword on a reference line.
/// For contexts without a fixed leading keyword (server references) the
/// search starts at the first non-whitespace column, relying on the
/// word-bounded match in `find_identifier_range` to skip stray substring
/// hits. For stick-table references the offset comes from the reference's
/// own stored column — a stick-table call-site can appear at any position on
/// a line (e.g. after an ACL condition `if foo { sc0_*(foo) gt 10 }`), so
/// scanning from column 0 would latch onto the unrelated identifier first.
fn ref_line_search_from(line: &str, reference: &Reference) -> usize {
    let trimmed_start = line.len() - line.trim_start().len();
    match reference.context {
        ReferenceContext::UseBackend => line
            .find("use_backend")
            .map(|p| p + "use_backend".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::DefaultBackend => line
            .find("default_backend")
            .map(|p| p + "default_backend".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::UseServer => line
            .find("use_server")
            .map(|p| p + "use_server".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::AclCondition => line
            .find(" if ")
            .map(|p| p + " if ".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::AclUnlessCondition => line
            .find(" unless ")
            .map(|p| p + " unless ".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::StickTable => reference.range.start.character as usize,
    }
}

/// Build a `sortText` from an in-file usage count and a label.
///
/// Higher `count` → lower prefix (zero-padded inverse), so frequently-used
/// symbols rank first. Ties break alphabetically via the trailing label.
fn frequency_sort_key(count: usize, label: &str) -> String {
    let capped = count.min(999_999);
    let inverse = 999_999 - capped;
    format!("{:06}_{}", inverse, label)
}

/// Hard-coded directive allowlist per section keyword. Values overlap so
/// each section's completion menu is self-contained; the lists are not
/// exhaustive but cover the vast majority of real-world configs.
fn directives_for_section(section: &str) -> &'static [&'static str] {
    match section {
        "global" => &[
            "daemon", "log", "maxconn", "nbthread", "user", "group",
            "pidfile", "chroot", "stats", "ssl-default-bind-ciphers",
            "ssl-default-bind-options", "tune.ssl.default-dh-param",
        ],
        "defaults" => &[
            "balance", "cookie", "default-server", "errorfile", "http-check",
            "log", "maxconn", "mode", "option", "retries", "timeout",
        ],
        "frontend" => &[
            "acl", "bind", "capture", "compression", "default_backend",
            "description", "filter", "http-after-response", "http-request",
            "http-response", "log", "maxconn", "mode", "monitor-uri",
            "option", "rate-limit", "redirect", "stats", "tcp-request",
            "tcp-response", "timeout", "use-service", "use_backend",
        ],
        "backend" => &[
            "acl", "balance", "compression", "cookie", "default-server",
            "description", "errorfile", "filter", "hash-type",
            "http-after-response", "http-check", "http-request",
            "http-response", "http-reuse", "http-send-name-header", "mode",
            "option", "redirect", "retries", "server", "stick", "stick-table",
            "tcp-check", "tcp-request", "tcp-response", "timeout", "use_server",
        ],
        "listen" => &[
            "acl", "balance", "bind", "compression", "cookie",
            "default-server", "default_backend", "description", "errorfile",
            "filter", "hash-type", "http-check", "http-request",
            "http-response", "http-reuse", "log", "maxconn", "mode", "option",
            "redirect", "retries", "server", "stats", "stick", "stick-table",
            "tcp-check", "tcp-request", "tcp-response", "timeout",
            "use_backend",
        ],
        "resolvers" => &[
            "accepted_payload_size", "hold", "nameserver", "resolve_retries",
            "timeout",
        ],
        "userlist" => &["group", "user"],
        "peers" => &["bind", "peer", "server", "table"],
        "cache" => &["max-age", "max-object-size", "total-max-size"],
        "mailers" => &["mailer", "timeout"],
        "program" => &["command", "group", "option", "user"],
        "ring" => &["format", "maxlen", "server", "size", "timeout"],
        _ => &[],
    }
}

fn section_kind_for(keyword: &str) -> u8 {
    match keyword {
        "global" | "defaults" => 3,           // Namespace
        "frontend" => 11,                      // Interface
        "backend" | "listen" => 5,             // Class
        "resolvers" | "userlist" | "peers" | "cache" | "mailers" | "program" | "ring" => 2, // Module
        _ => 2,
    }
}

fn truncate_detail(s: &str, max: usize) -> String {
    // Char-boundary-safe truncation. ACL criterion strings can contain
    // multi-byte chars in comments/regexes, so byte slicing is unsafe.
    let chars: Vec<char> = s.trim().chars().collect();
    if chars.len() <= max {
        chars.into_iter().collect()
    } else {
        let mut out: String = chars.into_iter().take(max).collect();
        out.push('…');
        out
    }
}

fn compute_outline(content: &str) -> Vec<DocumentSymbol> {
    let lines: Vec<&str> = content.lines().collect();
    let line_count = lines.len();

    // Pass 1: locate section headers.
    struct HeaderInfo {
        keyword: String,
        name: String,
        name_start_col: u32,
        header_line: u32,
        // Inline bind address on a `listen NAME addr[:port]` or
        // `frontend NAME addr[:port]` header. HAProxy's grammar allows the
        // address to appear on the same line as the section declaration; when
        // present it is recorded here so the outline `detail` can surface it
        // even when the body has no `bind` directive.
        inline_address: Option<String>,
    }
    let mut headers: Vec<HeaderInfo> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !is_section_header(line) {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        let keyword = tokens[0].to_string();
        let name = if tokens.len() >= 2 {
            tokens[1].to_string()
        } else {
            String::new()
        };
        // Only `listen` permits an inline bind address in the section header
        // (grammar: `listen NAME [addr]`); `frontend` does not. Also guard
        // against trailing comments like `listen X # note` leaking `#` as the
        // address.
        let inline_address = if keyword == "listen"
            && tokens.len() >= 3
            && !tokens[2].starts_with('#')
        {
            Some(tokens[2].to_string())
        } else {
            None
        };
        let name_start_col = if !name.is_empty() {
            // Search for the name starting *past* the keyword so that
            // pathological headers like `backend backend` or `frontend end`
            // (where the name is a substring of the keyword or appears inside
            // it) still point at the identifier token, not the keyword.
            let search_from = keyword.len();
            line.get(search_from..)
                .and_then(|tail| tail.find(&name).map(|n| (n + search_from) as u32))
                .unwrap_or(search_from as u32)
        } else {
            0
        };
        headers.push(HeaderInfo {
            keyword,
            name,
            name_start_col,
            header_line: i as u32,
            inline_address,
        });
    }

    // Pass 2: build DocumentSymbol per section with children and detail.
    let mut result: Vec<DocumentSymbol> = Vec::with_capacity(headers.len());
    for (idx, h) in headers.iter().enumerate() {
        let end_line = if idx + 1 < headers.len() {
            headers[idx + 1].header_line.saturating_sub(1)
        } else if line_count > 0 {
            (line_count - 1) as u32
        } else {
            h.header_line
        };

        let mut children: Vec<DocumentSymbol> = Vec::new();
        let mut balance: Option<String> = None;
        let mut mode: Option<String> = None;
        let mut server_count: usize = 0;
        let mut nameserver_count: usize = 0;
        let mut binds: Vec<String> = Vec::new();

        let body_start = (h.header_line as usize) + 1;
        let body_end = (end_line as usize).min(line_count.saturating_sub(1));
        if body_start <= body_end {
            for ln_idx in body_start..=body_end {
                let ln = lines[ln_idx];
                let trimmed = ln.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                if tokens.is_empty() {
                    continue;
                }
                match tokens[0] {
                    "acl" if (h.keyword == "frontend" || h.keyword == "listen" || h.keyword == "backend")
                        && tokens.len() >= 3 =>
                    {
                        let acl_name = tokens[1];
                        let criterion: String = tokens[2..].join(" ");
                        let detail = truncate_detail(&criterion, 40);
                        // Search past the `acl` keyword to avoid matching an
                        // earlier occurrence of the name (e.g. in indentation
                        // alignment or an `acl acl ...` edge case).
                        let search_from = ln.find("acl").map(|p| p + 3).unwrap_or(0);
                        let name_start = ln
                            .get(search_from..)
                            .and_then(|tail| tail.find(acl_name).map(|n| (n + search_from) as u32))
                            .unwrap_or(search_from as u32);
                        let name_end = name_start + acl_name.len() as u32;
                        children.push(DocumentSymbol {
                            name: acl_name.to_string(),
                            detail: Some(detail),
                            kind: 7, // Property
                            range: Range {
                                start: Position { line: ln_idx as u32, character: 0 },
                                end: Position { line: ln_idx as u32, character: ln.len() as u32 },
                            },
                            selection_range: Range {
                                start: Position { line: ln_idx as u32, character: name_start },
                                end: Position { line: ln_idx as u32, character: name_end },
                            },
                            children: Vec::new(),
                        });
                    }
                    "server" if (h.keyword == "backend" || h.keyword == "listen")
                        && tokens.len() >= 3 =>
                    {
                        server_count += 1;
                        let srv_name = tokens[1];
                        let addr = tokens[2].to_string();
                        let search_from = ln.find("server").map(|p| p + 6).unwrap_or(0);
                        let name_start = ln
                            .get(search_from..)
                            .and_then(|tail| tail.find(srv_name).map(|n| (n + search_from) as u32))
                            .unwrap_or(search_from as u32);
                        let name_end = name_start + srv_name.len() as u32;
                        children.push(DocumentSymbol {
                            name: srv_name.to_string(),
                            detail: Some(addr),
                            kind: 8, // Field
                            range: Range {
                                start: Position { line: ln_idx as u32, character: 0 },
                                end: Position { line: ln_idx as u32, character: ln.len() as u32 },
                            },
                            selection_range: Range {
                                start: Position { line: ln_idx as u32, character: name_start },
                                end: Position { line: ln_idx as u32, character: name_end },
                            },
                            children: Vec::new(),
                        });
                    }
                    "nameserver" if h.keyword == "resolvers" && tokens.len() >= 3 => {
                        nameserver_count += 1;
                        let ns_name = tokens[1];
                        let addr = tokens[2].to_string();
                        let search_from = ln.find("nameserver").map(|p| p + 10).unwrap_or(0);
                        let name_start = ln
                            .get(search_from..)
                            .and_then(|tail| tail.find(ns_name).map(|n| (n + search_from) as u32))
                            .unwrap_or(search_from as u32);
                        let name_end = name_start + ns_name.len() as u32;
                        children.push(DocumentSymbol {
                            name: ns_name.to_string(),
                            detail: Some(addr),
                            kind: 8, // Field
                            range: Range {
                                start: Position { line: ln_idx as u32, character: 0 },
                                end: Position { line: ln_idx as u32, character: ln.len() as u32 },
                            },
                            selection_range: Range {
                                start: Position { line: ln_idx as u32, character: name_start },
                                end: Position { line: ln_idx as u32, character: name_end },
                            },
                            children: Vec::new(),
                        });
                    }
                    "balance" if tokens.len() >= 2 && balance.is_none() => {
                        balance = Some(tokens[1].to_string());
                    }
                    "mode" if tokens.len() >= 2 && mode.is_none() => {
                        mode = Some(tokens[1].to_string());
                    }
                    "bind" if tokens.len() >= 2 => {
                        binds.push(tokens[1].to_string());
                    }
                    _ => {}
                }
            }
        }

        let detail = match h.keyword.as_str() {
            "backend" => {
                let mut parts: Vec<String> = Vec::new();
                if let Some(b) = balance {
                    parts.push(b);
                }
                if let Some(m) = mode {
                    parts.push(m);
                }
                parts.push(format!("{} servers", server_count));
                Some(parts.join(" · "))
            }
            "frontend" | "listen" => {
                // Surface the inline bind address from `listen NAME addr`
                // first so the detail still renders when the body contains no
                // explicit `bind` directive; body binds follow so operators
                // see both sources when both exist. `frontend` never carries
                // an inline address (grammar only permits it on `listen`).
                let mut parts: Vec<String> = Vec::new();
                if let Some(addr) = &h.inline_address {
                    parts.push(addr.clone());
                }
                parts.extend(binds.iter().cloned());
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(", "))
                }
            }
            "resolvers" => Some(format!("{} nameservers", nameserver_count)),
            _ => None,
        };

        let kind = section_kind_for(&h.keyword);
        let display_name = if h.name.is_empty() {
            h.keyword.clone()
        } else {
            h.name.clone()
        };
        let selection = if h.name.is_empty() {
            Range {
                start: Position { line: h.header_line, character: 0 },
                end: Position {
                    line: h.header_line,
                    character: h.keyword.len() as u32,
                },
            }
        } else {
            Range {
                start: Position {
                    line: h.header_line,
                    character: h.name_start_col,
                },
                end: Position {
                    line: h.header_line,
                    character: h.name_start_col + h.name.len() as u32,
                },
            }
        };
        let end_char = lines
            .get(end_line as usize)
            .map(|l| l.len() as u32)
            .unwrap_or(0);

        result.push(DocumentSymbol {
            name: display_name,
            detail,
            kind,
            range: Range {
                start: Position { line: h.header_line, character: 0 },
                end: Position { line: end_line, character: end_char },
            },
            selection_range: selection,
            children,
        });
    }

    result
}

fn serialize_document_symbol(sym: &DocumentSymbol) -> Value {
    // LSP wire format requires camelCase field names (notably `selectionRange`)
    // and numeric `kind`. Hand-construct to avoid relying on serde renames.
    json!({
        "name": sym.name,
        "detail": sym.detail,
        "kind": sym.kind as u32,
        "range": {
            "start": {
                "line": sym.range.start.line,
                "character": sym.range.start.character,
            },
            "end": {
                "line": sym.range.end.line,
                "character": sym.range.end.character,
            },
        },
        "selectionRange": {
            "start": {
                "line": sym.selection_range.start.line,
                "character": sym.selection_range.start.character,
            },
            "end": {
                "line": sym.selection_range.end.line,
                "character": sym.selection_range.end.character,
            },
        },
        "children": sym.children.iter().map(serialize_document_symbol).collect::<Vec<_>>(),
    })
}

impl HaproxyLsp {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(HaproxyLsp {
            symbols: HashMap::new(),
            folds: HashMap::new(),
            outline: HashMap::new(),
            documents: HashMap::new(),
            diagnostics: HashMap::new(),
            pending_notifications: Vec::new(),
            workspace_root: None,
            project_configs: HashMap::new(),
            included_files: HashMap::new(),
            explicitly_opened: HashSet::new(),
            project_indices: HashMap::new(),
        })
    }

    // Queue a JSON-RPC notification. The main loop drains the queue after the
    // current request handler returns, so the framed write to stdout is
    // serialized with the (at most one) response for that request.
    fn send_notification(&mut self, method: &str, params: Value) {
        self.pending_notifications.push(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn drain_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.pending_notifications)
    }

    // Build the diagnostics set for `uri` and publish it. Called at the tail
    // of `parse_document` after the per-URI caches are committed, so rule
    // handlers can rely on `self.symbols[uri]` / `self.documents[uri]`.
    // Task 2 adds undefined-reference rules; Task 3 will add unused-symbol
    // and structural rules.
    fn collect_diagnostics(&mut self, uri: &str) {
        let mut diags: Vec<Diagnostic> = Vec::new();
        if let (Some(content), Some(symbols)) = (
            self.documents.get(uri).cloned(),
            self.symbols.get(uri).cloned(),
        ) {
            diags.extend(undefined_reference_diagnostics(&content, &symbols));
            let headers = collect_section_headers(&content);
            diags.extend(duplicate_section_diagnostics(&headers));
            diags.extend(duplicate_acl_diagnostics(&content, &headers));
            diags.extend(missing_default_backend_diagnostics(&content, &headers));
            diags.extend(unused_backend_diagnostics(&content, &symbols));
            diags.extend(unused_acl_diagnostics(&content, &headers));
        }
        let diags_json: Vec<Value> = diags.iter().map(diagnostic_to_json).collect();
        self.diagnostics.insert(uri.to_string(), diags);
        self.send_notification(
            "textDocument/publishDiagnostics",
            json!({
                "uri": uri,
                "diagnostics": diags_json,
            }),
        );
    }

    // Top-level parse entry point. Parses the document at `uri`, then walks
    // the include graph (`.include`, `-f`, `crt`) so siblings are parsed
    // transitively, and finally rebuilds the project index keyed by the
    // file's project root. Preserves the single-file semantics of the
    // original `parse_document` (symbols / folds / outline / diagnostics all
    // committed for `uri`) while layering cross-file state on top.
    fn parse_document(&mut self, uri: &str, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut visited: HashSet<String> = HashSet::new();
        self.parse_graph_node(uri, Some(content), &mut visited)?;
        self.rebuild_project_index_for(uri);
        Ok(())
    }

    // Parse a single node in the include graph and recurse into its
    // neighbours. `content_override` is supplied for the top-level call (the
    // buffer the client just sent); sibling calls pass `None`, which either
    // picks up the last content the client explicitly provided via
    // didOpen/didChange, or reads from disk when the sibling isn't an open
    // editor buffer. `visited` is shared across the whole walk to prevent
    // cycles.
    fn parse_graph_node(
        &mut self,
        uri: &str,
        content_override: Option<&str>,
        visited: &mut HashSet<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !visited.insert(uri.to_string()) {
            return Ok(());
        }

        let content: String = if let Some(c) = content_override {
            c.to_string()
        } else if self.explicitly_opened.contains(uri) {
            // Sibling that the client is actively editing — trust its buffer
            // over the on-disk copy so unsaved edits stay authoritative.
            self.documents.get(uri).cloned().unwrap_or_default()
        } else if let Some(path) = uri_to_path(uri) {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };

        // Ensure a project config is resolved for this URI. Siblings discovered
        // via the include graph inherit the root file's config implicitly;
        // `resolve_project_config_for_path` walks up from the sibling's
        // directory so a shared `.zed/haproxy.toml` still applies.
        if !self.project_configs.contains_key(uri) {
            if let Some(path) = uri_to_path(uri) {
                let cfg = resolve_project_config_for_path(
                    &path,
                    self.workspace_root.as_deref(),
                );
                self.project_configs.insert(uri.to_string(), cfg);
            }
        }

        self.parse_single_file(uri, &content)?;

        let includes = self.extract_include_uris(uri, &content);
        self.included_files.insert(uri.to_string(), includes.clone());

        for inc_uri in includes {
            if visited.contains(&inc_uri) {
                continue;
            }
            let _ = self.parse_graph_node(&inc_uri, None, visited);
        }

        Ok(())
    }

    // Discover include-graph neighbours on `content` for the file at `uri`.
    // Recognised directives:
    //   - `.include <path>` — HAProxy 2.4+ preprocessor include.
    //   - `-f <path>` — command-line-style include (rare inside configs but
    //     appears in `program` sections and deployment wrappers).
    //   - `crt <path>` — TLS certificate include on `bind` lines; only
    //     included when the resolved path is a file (directories are skipped
    //     since Task 5 does not implement directory walking).
    //
    // Path resolution tries the file's own directory first, then the project
    // root from the resolved `ProjectConfig`. Absolute paths are kept as-is.
    // `.if` / `.elif` / `.else` / `.endif` are parsed conservatively: every
    // branch is walked regardless of the condition, since the line-scanner
    // already treats conditional directives as ordinary content.
    fn extract_include_uris(&self, uri: &str, content: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let file_path = match uri_to_path(uri) {
            Some(p) => p,
            None => return out,
        };
        let file_dir = file_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let project_root = self
            .project_configs
            .get(uri)
            .map(|c| c.project_root.clone())
            .unwrap_or_else(|| file_dir.clone());

        let mut seen: HashSet<PathBuf> = HashSet::new();

        for raw_line in content.lines() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let line_no_comment = strip_inline_comment(trimmed);
            let tokens: Vec<&str> = line_no_comment.split_whitespace().collect();
            if tokens.is_empty() {
                continue;
            }

            if tokens[0] == ".include" && tokens.len() >= 2 {
                let path_tok = unquote_path_token(tokens[1]);
                if let Some(resolved) =
                    resolve_include_path(path_tok, &file_dir, &project_root)
                {
                    if resolved.is_file() && seen.insert(resolved.clone()) {
                        if let Some(u) = path_to_file_uri(&resolved) {
                            out.push(u);
                        }
                    }
                }
                continue;
            }

            for (i, tok) in tokens.iter().enumerate() {
                if *tok == "-f" {
                    if let Some(path_tok) = tokens.get(i + 1) {
                        let path_tok = unquote_path_token(path_tok);
                        if let Some(resolved) =
                            resolve_include_path(path_tok, &file_dir, &project_root)
                        {
                            if resolved.is_file() && seen.insert(resolved.clone()) {
                                if let Some(u) = path_to_file_uri(&resolved) {
                                    out.push(u);
                                }
                            }
                        }
                    }
                } else if *tok == "crt" {
                    if let Some(path_tok) = tokens.get(i + 1) {
                        let path_tok = unquote_path_token(path_tok);
                        if let Some(resolved) =
                            resolve_include_path(path_tok, &file_dir, &project_root)
                        {
                            if resolved.is_file() && seen.insert(resolved.clone()) {
                                if let Some(u) = path_to_file_uri(&resolved) {
                                    out.push(u);
                                }
                            }
                        }
                    }
                }
            }
        }

        out
    }

    // Rebuild the project index rooted at the project_root of `seed_uri`.
    // Reachability is computed by walking `self.included_files` forward from
    // `seed_uri`; any URI reachable contributes its cached `self.symbols`
    // entries into `symbols_by_name`. The resulting index replaces any prior
    // index for the same project root.
    fn rebuild_project_index_for(&mut self, seed_uri: &str) {
        let project_root = match self.project_configs.get(seed_uri) {
            Some(cfg) => cfg.project_root.clone(),
            None => return,
        };

        let mut reachable: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = vec![seed_uri.to_string()];
        while let Some(u) = stack.pop() {
            if !seen.insert(u.clone()) {
                continue;
            }
            reachable.push(u.clone());
            if let Some(incs) = self.included_files.get(&u) {
                for i in incs {
                    stack.push(i.clone());
                }
            }
        }
        reachable.sort();

        let mut symbols_by_name: HashMap<(SymbolKind, String), Vec<ProjectSymbolRef>> =
            HashMap::new();
        for u in &reachable {
            if let Some(syms) = self.symbols.get(u) {
                for s in syms {
                    symbols_by_name
                        .entry((s.kind.clone(), s.name.clone()))
                        .or_default()
                        .push(ProjectSymbolRef {
                            uri: u.clone(),
                            range: s.range.clone(),
                            scope: s.scope.clone(),
                        });
                }
            }
        }

        let key = project_root.to_string_lossy().into_owned();
        self.project_indices.insert(
            key,
            ProjectIndex {
                project_root,
                uris: reachable,
                symbols_by_name,
            },
        );
    }

    fn parse_single_file(&mut self, uri: &str, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Line-scanning parser. Tree-sitter is loaded by Zed for highlighting
        // only; no AST is available to the LSP.
        let mut symbols = Vec::new();
        // Track the enclosing named section so `stick-table` directives can
        // be attributed to the correct backend/frontend/listen/peers name
        // (HAProxy binds one table per section, keyed by the section name).
        let mut current_section_name: Option<String> = None;

        for (line_num, raw_line) in content.lines().enumerate() {
            let line = raw_line.trim();

            // Section headers live at column 0; reject indented lines so
            // typos like `  backend foo` don't register a phantom symbol
            // whose range highlights the wrong column when the client
            // navigates to the definition.
            let is_section_line = is_section_header(raw_line);

            // Update section tracker before per-directive parsing so that
            // `stick-table` on a subsequent line attributes to this section.
            let first_tok = line.split_whitespace().next().unwrap_or("");
            if is_section_line {
                match first_tok {
                    "backend" | "frontend" | "listen" | "peers" => {
                        let tokens: Vec<&str> = line.split_whitespace().collect();
                        current_section_name = tokens.get(1).map(|s| s.to_string());
                    }
                    "global" | "defaults" | "resolvers" | "userlist"
                    | "mailers" | "cache" | "program" | "ring" => {
                        current_section_name = None;
                    }
                    _ => {}
                }
            }

            // Parse backend definitions
            if is_section_line && line.starts_with("backend ") {
                // Grammar only permits a section_name token after the
                // keyword; take the first whitespace-delimited token so
                // inline trailing data (if any) doesn't contaminate the
                // symbol name.
                let name = line["backend ".len()..].split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    symbols.push(Symbol {
                        name: name.to_string(),
                        kind: SymbolKind::Backend,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: None,
                    });
                }
            }
            // Parse frontend definitions
            else if is_section_line && line.starts_with("frontend ") {
                let name = line["frontend ".len()..].split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    symbols.push(Symbol {
                        name: name.to_string(),
                        kind: SymbolKind::Frontend,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: None,
                    });
                }
            }
            // Parse listen definitions
            else if is_section_line && line.starts_with("listen ") {
                // `listen` accepts an optional inline bind address per
                // grammar (`listen stats 127.0.0.1:9000`), so the name is
                // the first token only — not the full remainder.
                let name = line["listen ".len()..].split_whitespace().next().unwrap_or("");
                if !name.is_empty() {
                    symbols.push(Symbol {
                        name: name.to_string(),
                        kind: SymbolKind::Listen,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: None,
                    });
                }
            }
            // Parse ACL definitions
            else if line.starts_with("acl ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let name = parts[1];
                    symbols.push(Symbol {
                        name: name.to_string(),
                        kind: SymbolKind::Acl,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: None,
                    });
                }
            }
            // Parse server definitions. Servers are scoped to the enclosing
            // backend/listen section — two sections may declare the same
            // server name, and those are distinct entities.
            else if line.trim_start().starts_with("server ") {
                let parts: Vec<&str> = line.trim_start().split_whitespace().collect();
                if parts.len() >= 2 {
                    let name = parts[1];
                    symbols.push(Symbol {
                        name: name.to_string(),
                        kind: SymbolKind::Server,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: current_section_name.clone(),
                    });
                }
            }
            // Parse stick-table directives. HAProxy binds one stick-table per
            // section, named after the enclosing section; the directive itself
            // carries no name token. Only register when inside a named section
            // body — stray `stick-table` in `global`/`defaults` is ignored.
            else if line.starts_with("stick-table ") || line == "stick-table" {
                if let Some(ref section_name) = current_section_name {
                    symbols.push(Symbol {
                        name: section_name.clone(),
                        kind: SymbolKind::StickTable,
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        references: Vec::new(),
                        scope: None,
                    });
                }
            }
        }
        
        // Second pass: collect references to symbols. Track the enclosing
        // section name on this pass too so scoped references (currently
        // `use_server`) can be resolved against the correct server definition
        // even when two backends share a server name.
        let mut updated_symbols = symbols;
        let mut ref_section_name: Option<String> = None;
        for (line_num, raw_line) in content.lines().enumerate() {
            let is_section_line = is_section_header(raw_line);
            let trimmed = raw_line.trim();
            let first_tok = trimmed.split_whitespace().next().unwrap_or("");
            if is_section_line {
                match first_tok {
                    "backend" | "frontend" | "listen" | "peers" => {
                        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
                        ref_section_name = tokens.get(1).map(|s| s.to_string());
                    }
                    "global" | "defaults" | "resolvers" | "userlist"
                    | "mailers" | "cache" | "program" | "ring" => {
                        ref_section_name = None;
                    }
                    _ => {}
                }
            }

            let line = trimmed;

            // Skip comment lines so commented-out sample config doesn't
            // produce phantom references (which would inflate reference
            // listings and skew completion frequency ranking).
            if line.starts_with('#') {
                continue;
            }

            // Collect backend references
            if line.contains("use_backend") {
                if let Some(backend_name) = self.extract_backend_from_use_backend(line) {
                    self.add_reference_to_symbol(&mut updated_symbols, &backend_name, SymbolKind::Backend,
                                              Reference {
                                                  range: Range {
                                                      start: Position { line: line_num as u32, character: 0 },
                                                      end: Position { line: line_num as u32, character: line.len() as u32 },
                                                  },
                                                  uri: uri.to_string(),
                                                  context: ReferenceContext::UseBackend,
                                                  scope: None,
                                              });
                }
            }

            if line.contains("default_backend") {
                if let Some(backend_name) = self.extract_backend_from_default_backend(line) {
                    self.add_reference_to_symbol(&mut updated_symbols, &backend_name, SymbolKind::Backend,
                                              Reference {
                                                  range: Range {
                                                      start: Position { line: line_num as u32, character: 0 },
                                                      end: Position { line: line_num as u32, character: line.len() as u32 },
                                                  },
                                                  uri: uri.to_string(),
                                                  context: ReferenceContext::DefaultBackend,
                                                  scope: None,
                                              });
                }
            }

            // Collect server references from `use_server NAME [if ACL]`.
            // Required for rename: Tier 2 lists servers as renameable, and
            // without this the definition line is rewritten but every call
            // site is left stale, silently breaking the config. The
            // reference carries the enclosing section as its scope so that
            // `add_reference_to_symbol` attaches it only to the matching
            // `server` definition in the SAME section — two backends that
            // both define a `server shared` stay independent.
            if line.starts_with("use_server ") {
                if let Some(server_name) = self.extract_server_from_use_server(line) {
                    self.add_reference_to_symbol(&mut updated_symbols, &server_name, SymbolKind::Server,
                                              Reference {
                                                  range: Range {
                                                      start: Position { line: line_num as u32, character: 0 },
                                                      end: Position { line: line_num as u32, character: line.len() as u32 },
                                                  },
                                                  uri: uri.to_string(),
                                                  context: ReferenceContext::UseServer,
                                                  scope: ref_section_name.clone(),
                                              });
                }
            }

            // Collect ACL references
            if line.contains(" if ") {
                if let Some(acl_names) = self.extract_acl_names_from_condition(line, "if") {
                    for acl_name in acl_names {
                        self.add_reference_to_symbol(&mut updated_symbols, &acl_name, SymbolKind::Acl,
                                                  Reference {
                                                      range: Range {
                                                          start: Position { line: line_num as u32, character: 0 },
                                                          end: Position { line: line_num as u32, character: line.len() as u32 },
                                                      },
                                                      uri: uri.to_string(),
                                                      context: ReferenceContext::AclCondition,
                                                      scope: None,
                                                  });
                    }
                }
            }

            if line.contains(" unless ") {
                if let Some(acl_names) = self.extract_acl_names_from_condition(line, "unless") {
                    for acl_name in acl_names {
                        self.add_reference_to_symbol(&mut updated_symbols, &acl_name, SymbolKind::Acl,
                                                  Reference {
                                                      range: Range {
                                                          start: Position { line: line_num as u32, character: 0 },
                                                          end: Position { line: line_num as u32, character: line.len() as u32 },
                                                      },
                                                      uri: uri.to_string(),
                                                      context: ReferenceContext::AclUnlessCondition,
                                                      scope: None,
                                                  });
                    }
                }
            }

            // Collect stick-table references. Strip any trailing `# comment`
            // so a commented-out ` table foo` suffix doesn't register a
            // phantom reference (which would also corrupt the comment text
            // on rename).
            let line_no_comment = strip_inline_comment(line);
            let stick_refs = collect_stick_table_references(line_no_comment);
            for (table_name, start_col) in stick_refs {
                let end_col = start_col + table_name.len();
                self.add_reference_to_symbol(
                    &mut updated_symbols,
                    &table_name,
                    SymbolKind::StickTable,
                    Reference {
                        range: Range {
                            start: Position { line: line_num as u32, character: start_col as u32 },
                            end: Position { line: line_num as u32, character: end_col as u32 },
                        },
                        uri: uri.to_string(),
                        context: ReferenceContext::StickTable,
                        scope: None,
                    },
                );
            }
        }
        
        // Build fold/outline data into locals before any self.* write so a
        // mid-parse panic cannot leave caches out of sync with each other.
        let folds = compute_folds(content);
        let outline = compute_outline(content);

        self.symbols.insert(uri.to_string(), updated_symbols);
        self.folds.insert(uri.to_string(), folds);
        self.outline.insert(uri.to_string(), outline);
        self.documents.insert(uri.to_string(), content.to_string());
        // Always publish diagnostics (possibly empty) so stale marks clear on
        // the client even when the file is now clean.
        self.collect_diagnostics(uri);
        Ok(())
    }

    fn find_definition(&self, uri: &str, position: &Position, content: &str) -> Option<Symbol> {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }
        let line = lines[line_idx];

        // Resolve the word under the cursor and its byte offset on the line.
        let (word, word_start) = self.word_at_position(line, position.character as usize)?;

        // Walk tokens backwards from the cursor looking for the controlling
        // keyword that disambiguates the word's role. Identifier-like tokens
        // (intermediate ACL names in a chained `if a b c`) and condition
        // operators (`!`, `&&`, `||`) are skipped so the walk can reach the
        // real context keyword (`if`, `use_backend`, `listen`, ...).
        //
        // Additionally, we must distinguish the keyword's "name slot" (the
        // first identifier token following it) from later positional tokens.
        // Example: `listen stats 10.0.0.1:9091` — cursor on the address walks
        // back past `stats` and hits `listen`, but the address is NOT a listen
        // name. Without this guard, if another `listen 10.0.0.1` exists in
        // the file, F12 on the address would misnavigate to it. Same issue
        // for `server s1 10.0.0.1:8080 check` and any trailing option tokens.
        //
        // All supported keywords except `if`/`unless` take exactly one name
        // slot; only ACL conditions (`if`/`unless`) admit multiple subsequent
        // identifier references (`if a && b || c`).
        let prefix = &line[..word_start];
        let prefix_tokens: Vec<&str> = prefix.split_whitespace().collect();

        // Stick-table context detection runs BEFORE the general keyword
        // walk-back because the walk-back would otherwise latch onto a
        // preceding `if`/`unless` (common in `... if { sc0_*(tbl) gt N }`)
        // and mis-classify the cursor word as an ACL reference.
        if let Some(last) = prefix_tokens.last().copied() {
            // Case A: `sc<digit>_<ident>(` immediately before the cursor word.
            // The last token carries the unmatched `(` which means the cursor
            // sits inside the argument list.
            if last.len() > 3
                && last.starts_with("sc")
                && last.as_bytes()[2].is_ascii_digit()
                && last.as_bytes()[3] == b'_'
                && last.contains('(')
            {
                let paren_pos = last.find('(').unwrap();
                // Cursor is inside an unclosed sc<N>_*( call; treat the word
                // as a stick-table name if it's the first positional arg
                // (no comma between `(` and the word).
                let after_paren = &last[paren_pos + 1..];
                if !after_paren.contains(',') && !after_paren.contains(')') {
                    return self.find_symbol_by_name(uri, &word, SymbolKind::StickTable);
                }
            }
            // Case B: `... table <word>` — the immediate preceding token is
            // the `table` keyword. Covers `stick on ... table X`,
            // `http-request track-sc0 src table X`, etc.
            if last == "table" {
                return self.find_symbol_by_name(uri, &word, SymbolKind::StickTable);
            }
        }
        // Note: we deliberately do NOT treat the first positional token after
        // `stick match|store-request|store-response` as a stick-table name.
        // Per HAProxy grammar that token is a sample expression; only the
        // explicit `table <name>` clause (Case B above) carries the table.

        let kw_match = prefix_tokens.iter().enumerate().rev().find_map(|(idx, tok)| {
            match *tok {
                "use_backend" | "default_backend" | "backend" => Some((idx, SymbolKind::Backend)),
                "if" | "unless" => Some((idx, SymbolKind::Acl)),
                "frontend" => Some((idx, SymbolKind::Frontend)),
                "listen" => Some((idx, SymbolKind::Listen)),
                "acl" => Some((idx, SymbolKind::Acl)),
                "server" | "use_server" => Some((idx, SymbolKind::Server)),
                _ => None,
            }
        });

        if let Some((kw_idx, kind)) = kw_match {
            // For single-name-slot keywords, the cursor word must be the
            // immediate next token after the keyword. Any token past that
            // slot (bind address, server address, trailing options, inline
            // comment text) must not resolve, even if it textually matches
            // an existing symbol name.
            let is_condition_kw = matches!(kind, SymbolKind::Acl)
                && matches!(
                    prefix_tokens.get(kw_idx).copied(),
                    Some("if") | Some("unless")
                );
            if !is_condition_kw && kw_idx + 1 != prefix_tokens.len() {
                return None;
            }
            // Inside an `if`/`unless` condition the cursor word may live in a
            // `{ ... }` sample expression — those tokens are fetch names
            // (`src`, `sc0_*`, `hdr(...)`, ...) or literal arguments, never
            // ACL references. Only standalone brace tokens flip depth;
            // HAProxy grammar requires whitespace around `{` / `}`, so braces
            // embedded in other tokens (regex literals like `^/foo\{$`, PCRE
            // quantifiers like `\d{3,}`) are content and must not register.
            if is_condition_kw {
                let mut depth: i32 = 0;
                for tok in &prefix_tokens[kw_idx + 1..] {
                    if *tok == "{" || *tok == "!{" {
                        depth += 1;
                    } else if *tok == "}" && depth > 0 {
                        depth -= 1;
                    }
                }
                if depth > 0 {
                    return None;
                }
            }
            // Servers are section-scoped: resolve against the server declared
            // in the enclosing backend/listen so that `use_server shared` in
            // backend A doesn't navigate to a same-named server in backend B.
            if matches!(kind, SymbolKind::Server) {
                let enclosing = self.enclosing_section_name(content, line_idx);
                if let Some(scope) = enclosing.as_deref() {
                    return self.find_symbol_by_name_scoped(
                        uri,
                        &word,
                        SymbolKind::Server,
                        Some(scope),
                    );
                }
                return None;
            }
            // ACL duplicates: multiple `acl NAME ...` lines define the same
            // ACL. When the cursor is on a definition line, prefer the
            // definition on THAT line so hover / jump / rename don't claim
            // the first duplicate as the canonical one.
            if matches!(kind, SymbolKind::Acl)
                && matches!(prefix_tokens.get(kw_idx).copied(), Some("acl"))
            {
                if let Some(sym) = self.find_acl_symbol_at_line(uri, &word, line_idx as u32) {
                    return Some(sym);
                }
            }
            return self.find_symbol_by_name(uri, &word, kind);
        }

        None
    }

    /// Find an ACL symbol with `name` whose definition line matches
    /// `line_idx`. Used to disambiguate the cursor-on-duplicate-declaration
    /// case so hover/rename key off the actual declaration the cursor sits
    /// on rather than the first lexical match.
    fn find_acl_symbol_at_line(&self, uri: &str, name: &str, line_idx: u32) -> Option<Symbol> {
        let symbols = self.symbols.get(uri)?;
        for sym in symbols {
            if sym.kind == SymbolKind::Acl
                && sym.name == name
                && sym.range.start.line == line_idx
            {
                return Some(sym.clone());
            }
        }
        None
    }
    
    fn extract_backend_from_use_backend(&self, line: &str) -> Option<String> {
        // Parse "use_backend BACKEND_NAME [if condition]"
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() >= 2 && parts[0] == "use_backend" {
            Some(parts[1].to_string())
        } else {
            None
        }
    }
    
    fn extract_backend_from_default_backend(&self, line: &str) -> Option<String> {
        // Parse "default_backend BACKEND_NAME"
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() >= 2 && parts[0] == "default_backend" {
            Some(parts[1].to_string())
        } else {
            None
        }
    }

    fn extract_server_from_use_server(&self, line: &str) -> Option<String> {
        // Parse "use_server SERVER_NAME [if condition]"
        let parts: Vec<&str> = line.trim().split_whitespace().collect();
        if parts.len() >= 2 && parts[0] == "use_server" && is_valid_identifier(parts[1]) {
            Some(parts[1].to_string())
        } else {
            None
        }
    }
    
    fn add_reference_to_symbol(&self, symbols: &mut Vec<Symbol>, symbol_name: &str, symbol_kind: SymbolKind, reference: Reference) {
        // HAProxy permits multiple `acl NAME ...` lines for OR semantics; the
        // same applies to any duplicated definition. Attach the reference to
        // every matching symbol so that resolving from any definition line
        // (or via name lookup) returns the full reference set.
        //
        // Server symbols are scoped to their enclosing section — a
        // `use_server web1` inside backend A must only attach to backend A's
        // `server web1`, never to a backend B that happens to define a server
        // of the same name. Scope-aware matching preserves section identity.
        for symbol in symbols.iter_mut() {
            if symbol.name != symbol_name || symbol.kind != symbol_kind {
                continue;
            }
            if symbol_kind == SymbolKind::Server
                && reference.scope.is_some()
                && symbol.scope != reference.scope
            {
                continue;
            }
            symbol.references.push(reference.clone());
        }
    }
    
    fn extract_acl_names_from_condition(&self, line: &str, condition_type: &str) -> Option<Vec<String>> {
        // Find the condition part after "if" or "unless"
        let condition_start = line.find(&format!(" {} ", condition_type))?;
        let condition_part = &line[condition_start + condition_type.len() + 2..];

        // Strip trailing line comments so words after `#` aren't recorded as
        // spurious ACL references (e.g. `use_backend foo if bar # production`
        // would otherwise register `production` as an ACL name). Use the
        // whitespace-aware helper so `#` embedded inside a token (e.g. a regex
        // literal `^/foo#bar$`) isn't mistaken for a comment start.
        let condition_part = strip_inline_comment(condition_part);

        // Simple parsing: split by whitespace and filter out operators and logical keywords.
        // Track brace depth so that tokens inside inline sample expressions
        // (`{ src 10.0.0.0/8 }`, `{ sc0_http_req_rate(foo) gt 10 }`) are NOT
        // recorded as ACL references — those tokens are sample-fetch names or
        // literal values, not ACLs.
        //
        // HAProxy grammar requires whitespace around the `{` / `}` sample
        // expression delimiters, so only standalone brace tokens (plus the
        // shorthand `!{` negated-open form) count toward depth. Braces
        // embedded inside other tokens are content — typically regex
        // literals such as `^/foo\{$` or PCRE quantifiers `\d{3,}` — and
        // must not flip depth, otherwise post-`}` ACL references are lost.
        let parts: Vec<&str> = condition_part.split_whitespace().collect();
        let mut acl_names = Vec::new();
        let mut brace_depth: u32 = 0;

        for part in parts {
            // Standalone brace tokens adjust depth and are skipped.
            if part == "{" || part == "!{" {
                brace_depth += 1;
                continue;
            }
            if part == "}" {
                brace_depth = brace_depth.saturating_sub(1);
                continue;
            }
            // Inside a sample expression — skip every token until the closing brace.
            if brace_depth > 0 {
                continue;
            }
            // Skip HAProxy operators and logical keywords.
            // Note: do NOT skip tokens that merely *start* with `!` — those are
            // negated ACL references (`if !foo.bar`) and must flow through to
            // the `trim_start_matches('!')` path below so the bare name is
            // recorded as a reference.
            if part == "||" || part == "&&" || part == "!" {
                continue;
            }
            // Remove negation prefix and add ACL name.
            // Grammar permits `.` in identifiers (`[a-zA-Z0-9_.-]+`), so
            // dotted names like `geo.prod.allow` must not be filtered.
            let clean_name = part.trim_start_matches('!').trim();
            if !clean_name.is_empty()
                && clean_name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
            {
                acl_names.push(clean_name.to_string());
            }
        }
        
        if acl_names.is_empty() {
            None
        } else {
            Some(acl_names)
        }
    }
    
    /// Return the word under the cursor plus its byte offset on the line.
    ///
    /// Accepts a cursor that sits just past the end of a word (common for F12
    /// targets) by stepping back one character. Returns `None` if the cursor
    /// is in whitespace and not adjacent to a word.
    fn word_at_position(&self, line: &str, char_pos: usize) -> Option<(String, usize)> {
        let chars: Vec<char> = line.chars().collect();
        // Grammar identifier set is `/[a-zA-Z0-9_.-]+/`, so `.` must
        // count as a word char to resolve dotted names like `foo.bar`.
        let is_word_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-' || c == '.';

        let len = chars.len();
        let mut pos = char_pos.min(len);
        if pos == len || !is_word_char(chars[pos]) {
            if pos > 0 && is_word_char(chars[pos - 1]) {
                pos -= 1;
            } else {
                return None;
            }
        }

        let mut start = pos;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = pos;
        while end < len && is_word_char(chars[end]) {
            end += 1;
        }
        if start >= end {
            return None;
        }

        let word: String = chars[start..end].iter().collect();
        let byte_start = line
            .char_indices()
            .nth(start)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        Some((word, byte_start))
    }

    fn find_symbol_by_name(&self, uri: &str, name: &str, kind: SymbolKind) -> Option<Symbol> {
        self.find_symbol_by_name_scoped(uri, name, kind, None)
    }

    /// Scoped symbol lookup. When `scope` is `Some`, only symbols whose
    /// `scope` matches are returned — used for Server resolution so that
    /// `use_server shared` in backend A does not cross-navigate to
    /// backend B's same-named server. When `scope` is `None`, the first
    /// matching symbol is returned (legacy behaviour).
    fn find_symbol_by_name_scoped(
        &self,
        uri: &str,
        name: &str,
        kind: SymbolKind,
        scope: Option<&str>,
    ) -> Option<Symbol> {
        // Single-file scope: only resolve against the requesting document so
        // that two open files with the same backend/acl name don't silently
        // cross-navigate.
        let symbols = self.symbols.get(uri)?;
        for symbol in symbols {
            if symbol.name != name
                || std::mem::discriminant(&symbol.kind) != std::mem::discriminant(&kind)
            {
                continue;
            }
            if let Some(want) = scope {
                match symbol.scope.as_deref() {
                    Some(have) if have == want => return Some(symbol.clone()),
                    _ => continue,
                }
            }
            return Some(symbol.clone());
        }
        None
    }

    /// Walk up from `line_idx` to the enclosing section header and return
    /// the section's name token (e.g. backend/frontend/listen name). Returns
    /// `None` when the cursor sits inside `global`/`defaults` or before any
    /// named section.
    fn enclosing_section_name(&self, content: &str, line_idx: usize) -> Option<String> {
        let lines: Vec<&str> = content.lines().collect();
        if line_idx >= lines.len() {
            return None;
        }
        let mut i = line_idx;
        loop {
            if is_section_header(lines[i]) {
                let tokens: Vec<&str> = lines[i].split_whitespace().collect();
                let kw = tokens.first().copied().unwrap_or("");
                if matches!(kw, "backend" | "frontend" | "listen" | "peers") {
                    return tokens.get(1).map(|s| s.to_string());
                }
                return None;
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    }

    fn find_declaration(&self, uri: &str, position: &Position, content: &str) -> Option<Vec<Reference>> {
        // Find what symbol is at the given position
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return None;
        }

        let line = lines[position.line as usize];

        // Check if we're on a symbol definition (backend, acl, etc.)
        // If so, return all references to that symbol

        // Section-definition lookups must extract only the first
        // whitespace-delimited token after the keyword so trailing inline
        // bind addresses (`listen stats 127.0.0.1:9000`) or trailing `#`
        // comments don't contaminate the symbol name. This mirrors how
        // parse_document stores the symbol name in the symbol table; any
        // divergence here would silently drop declarations on such lines.
        let trimmed = line.trim();

        // Check if this line defines a backend
        if let Some(rest) = trimmed.strip_prefix("backend ") {
            if let Some(name) = rest.split_whitespace().next() {
                return self.find_references_to_symbol(uri, name, SymbolKind::Backend);
            }
        }

        // Check if this line defines an ACL
        if trimmed.starts_with("acl ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                let name = parts[1];
                return self.find_references_to_symbol(uri, name, SymbolKind::Acl);
            }
        }

        // Check if this line defines a frontend
        if let Some(rest) = trimmed.strip_prefix("frontend ") {
            if let Some(name) = rest.split_whitespace().next() {
                return self.find_references_to_symbol(uri, name, SymbolKind::Frontend);
            }
        }

        // Check if this line defines a listen section
        if let Some(rest) = trimmed.strip_prefix("listen ") {
            if let Some(name) = rest.split_whitespace().next() {
                return self.find_references_to_symbol(uri, name, SymbolKind::Listen);
            }
        }

        // Check if this line defines a server. Scope the lookup to the
        // enclosing backend/listen so two sections that each declare a
        // same-named server keep independent reference sets.
        if line.trim().trim_start().starts_with("server ") {
            let parts: Vec<&str> = line.trim().trim_start().split_whitespace().collect();
            if parts.len() >= 2 {
                let name = parts[1];
                let enclosing = self.enclosing_section_name(content, position.line as usize);
                return self.find_references_to_symbol_scoped(
                    uri,
                    name,
                    SymbolKind::Server,
                    enclosing.as_deref(),
                );
            }
        }

        None
    }

    /// Resolve the symbol at the cursor for `textDocument/references`.
    ///
    /// Accepts both sides of a navigation:
    ///   - Cursor on a definition line (e.g. `backend NAME`, `acl NAME ...`,
    ///     `frontend NAME`, `listen NAME`, `server NAME ...`) — extract the
    ///     name token and look up the cached Symbol directly. This works
    ///     regardless of which column the cursor sits on (keyword, name, or
    ///     trailing address/option tokens), matching how `find_declaration`
    ///     behaves today.
    ///   - Cursor on a reference site (e.g. `use_backend X`, `if acl`,
    ///     `sc0_*(name)`, `... table X`) — delegate to the existing
    ///     cursor-aware `find_definition` walk-back.
    ///
    /// Stick-tables are only reachable via the reference-site path; there is
    /// no bare identifier on the `stick-table` directive line itself to key
    /// off, so callers exercising references for stick-tables must put the
    /// cursor on a call site (`sc0_*(name)` or `... table name`).
    fn find_symbol_at_cursor(&self, uri: &str, position: &Position, content: &str) -> Option<Symbol> {
        let lines: Vec<&str> = content.lines().collect();
        if (position.line as usize) >= lines.len() {
            return None;
        }
        let trimmed = lines[position.line as usize].trim();

        if let Some(rest) = trimmed.strip_prefix("backend ") {
            if let Some(name) = rest.split_whitespace().next() {
                if let Some(sym) = self.find_symbol_by_name(uri, name, SymbolKind::Backend) {
                    return Some(sym);
                }
            }
        }
        if let Some(rest) = trimmed.strip_prefix("frontend ") {
            if let Some(name) = rest.split_whitespace().next() {
                if let Some(sym) = self.find_symbol_by_name(uri, name, SymbolKind::Frontend) {
                    return Some(sym);
                }
            }
        }
        if let Some(rest) = trimmed.strip_prefix("listen ") {
            if let Some(name) = rest.split_whitespace().next() {
                if let Some(sym) = self.find_symbol_by_name(uri, name, SymbolKind::Listen) {
                    return Some(sym);
                }
            }
        }
        if trimmed.starts_with("server ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                // Server identity is section-scoped; resolve against the
                // enclosing backend/listen so duplicate names across
                // sections stay distinct.
                let enclosing = self.enclosing_section_name(content, position.line as usize);
                if let Some(sym) = self.find_symbol_by_name_scoped(
                    uri,
                    parts[1],
                    SymbolKind::Server,
                    enclosing.as_deref(),
                ) {
                    return Some(sym);
                }
            }
        }
        // ACL duplicates: prefer the declaration on the cursor line so a
        // cursor-on-def-line references/hover/rename returns that specific
        // declaration rather than the first lexical match.
        if trimmed.starts_with("acl ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(sym) =
                    self.find_acl_symbol_at_line(uri, parts[1], position.line)
                {
                    return Some(sym);
                }
            }
        }

        self.find_definition(uri, position, content)
    }

    /// Build a markdown hover body for a backend symbol: the definition line
    /// followed by `mode`, `balance`, and up to 5 `server` lines. Surplus
    /// servers are summarised as `… N more`. The body is wrapped in a fenced
    /// code block so Zed renders it as HAProxy config.
    fn backend_hover_body(&self, content: &str, sym: &Symbol) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let def_line_idx = sym.range.start.line as usize;
        let def_line = lines.get(def_line_idx).copied().unwrap_or("").trim();

        let mut mode: Option<String> = None;
        let mut balance: Option<String> = None;
        let mut servers: Vec<String> = Vec::new();
        let mut extra_servers: usize = 0;

        let mut i = def_line_idx + 1;
        while i < lines.len() {
            let raw = lines[i];
            if is_section_header(raw) {
                break;
            }
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                i += 1;
                continue;
            }
            let first = trimmed.split_whitespace().next().unwrap_or("");
            match first {
                "mode" if mode.is_none() => mode = Some(trimmed.to_string()),
                "balance" if balance.is_none() => balance = Some(trimmed.to_string()),
                "server" => {
                    if servers.len() < 5 {
                        servers.push(trimmed.to_string());
                    } else {
                        extra_servers += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }

        let mut body = String::new();
        body.push_str("```haproxy\n");
        body.push_str(def_line);
        body.push('\n');
        if let Some(m) = mode {
            body.push_str("  ");
            body.push_str(&m);
            body.push('\n');
        }
        if let Some(b) = balance {
            body.push_str("  ");
            body.push_str(&b);
            body.push('\n');
        }
        for s in &servers {
            body.push_str("  ");
            body.push_str(s);
            body.push('\n');
        }
        if extra_servers > 0 {
            body.push_str(&format!("  … {} more\n", extra_servers));
        }
        body.push_str("```");
        body
    }

    /// Fenced code block rendering of the line at `line_idx` trimmed. Used for
    /// ACL, stick-table, and server hover paths where the whole directive line
    /// is the most useful summary.
    fn line_hover_body(&self, content: &str, line_idx: u32) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let raw = lines
            .get(line_idx as usize)
            .copied()
            .unwrap_or("")
            .trim();
        let mut body = String::new();
        body.push_str("```haproxy\n");
        body.push_str(raw);
        body.push_str("\n```");
        body
    }

    fn find_hover(&self, uri: &str, position: &Position, content: &str) -> Option<String> {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }
        let line = lines[line_idx];
        let (word, _word_start) = self.word_at_position(line, position.character as usize)?;

        // Cursor-aware resolution: `find_definition` uses the same keyword
        // walk-back as Go-to-Definition. It already covers every navigable
        // hover target (section headers, `use_backend`/`default_backend`
        // references, `if`/`unless` ACL references, `server`/`use_server`,
        // `sc<N>_*(name)` and `table <name>` stick-table call sites) and
        // returns None for tokens the grammar treats as sample expressions
        // (fetches inside `{ ... }`, `stick match <fetch>`). Do NOT fall
        // back to an unconstrained by-name sweep here — that would
        // reintroduce the same false positives `find_definition` was
        // careful to exclude (e.g. hovering `src` inside `if { src ... }`
        // resolving to an unrelated `backend src`).
        if let Some(sym) = self.find_definition(uri, position, content) {
            return Some(self.render_symbol_hover(content, &sym));
        }

        // Directive docs: only fire when the cursor word is the directive
        // token (first whitespace-delimited token on the trimmed line) AND
        // the docs table has an entry for it. This avoids showing
        // directive documentation for positional argument words that happen
        // to collide with a directive name (e.g. a server called `mode`).
        let first_token = line.trim_start().split_whitespace().next().unwrap_or("");
        if first_token == word {
            if let Some(doc) = docs::directive_doc(&word) {
                return Some(doc.to_string());
            }
        }

        None
    }

    /// Dispatch a resolved `Symbol` to the right hover renderer. Backend
    /// summaries include mode/balance/servers; everything else just shows
    /// the symbol's own directive line verbatim.
    fn render_symbol_hover(&self, content: &str, sym: &Symbol) -> String {
        match sym.kind {
            SymbolKind::Backend => self.backend_hover_body(content, sym),
            SymbolKind::Acl
            | SymbolKind::StickTable
            | SymbolKind::Server
            | SymbolKind::Frontend
            | SymbolKind::Listen => self.line_hover_body(content, sym.range.start.line),
        }
    }

    fn find_references_to_symbol(&self, uri: &str, symbol_name: &str, symbol_kind: SymbolKind) -> Option<Vec<Reference>> {
        self.find_references_to_symbol_scoped(uri, symbol_name, symbol_kind, None)
    }

    fn find_references_to_symbol_scoped(
        &self,
        uri: &str,
        symbol_name: &str,
        symbol_kind: SymbolKind,
        scope: Option<&str>,
    ) -> Option<Vec<Reference>> {
        // Single-file scope: look only in the requesting document.
        let symbols = self.symbols.get(uri)?;
        for symbol in symbols {
            if symbol.name != symbol_name
                || std::mem::discriminant(&symbol.kind)
                    != std::mem::discriminant(&symbol_kind)
            {
                continue;
            }
            if let Some(want) = scope {
                match symbol.scope.as_deref() {
                    Some(have) if have == want => {}
                    _ => continue,
                }
            }
            if symbol.references.is_empty() {
                return None;
            } else {
                return Some(symbol.references.clone());
            }
        }
        None
    }

    /// Compute completion items for `textDocument/completion`.
    ///
    /// Context resolution order:
    /// 1. Cursor inside an unclosed `sc<N>_<ident>(` call (first positional
    ///    arg) → stick-table names.
    /// 2. Last effective keyword is `stick match`/`stick store-request`/
    ///    `stick store-response` → stick-table names.
    /// 3. Last effective keyword is `use_backend`/`default_backend` →
    ///    backend names.
    /// 4. Last effective keyword is `use_server` → server names from the
    ///    enclosing backend/listen section body.
    /// 5. Prefix contains `if`/`unless` outside `{}` groups → ACL names.
    /// 6. Cursor is typing the first token on a line inside a known
    ///    section body → directive allowlist for that section.
    ///
    /// Each item carries `sortText` derived from the symbol's in-file
    /// usage frequency (higher reference count → earlier in the list);
    /// directives use a fixed alphabetical ordering.
    fn compute_completions(&self, uri: &str, position: &Position, content: &str) -> Vec<Value> {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return Vec::new();
        }
        let line = lines[line_idx];
        // `position.character` is a char index (matching word_at_position's
        // convention elsewhere in this file). Convert to a byte offset that
        // lands on a valid char boundary so slicing `&line[..byte_pos]` on a
        // line containing multi-byte UTF-8 (comments, description strings)
        // does not panic with "byte index is not a char boundary".
        let char_pos_chars = position.character as usize;
        let byte_pos = line
            .char_indices()
            .nth(char_pos_chars)
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        let prefix = &line[..byte_pos];

        // Case 1: inside an unclosed sc<N>_*(...) call, first positional arg.
        if let Some(open) = prefix.rfind('(') {
            let after_open = &prefix[open + 1..];
            if !after_open.contains(')') && !after_open.contains(',') {
                let before_paren = &prefix[..open];
                let fn_start = before_paren
                    .rfind(|c: char| c.is_whitespace() || c == '{' || c == '[')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                let fn_name = &before_paren[fn_start..];
                let fb = fn_name.as_bytes();
                if fb.len() > 3
                    && fb[0] == b's'
                    && fb[1] == b'c'
                    && fb[2].is_ascii_digit()
                    && fb[3] == b'_'
                {
                    return self.complete_stick_tables(uri);
                }
            }
        }

        let prefix_tokens: Vec<&str> = prefix.split_whitespace().collect();
        let ends_with_whitespace = prefix.is_empty()
            || prefix.ends_with(|c: char| c.is_whitespace());

        // "Effective keyword" is the token the cursor sits immediately after.
        // When the cursor is mid-word, that is the second-to-last token;
        // otherwise it is the last token.
        let kw_idx: Option<usize> = if ends_with_whitespace {
            if prefix_tokens.is_empty() {
                None
            } else {
                Some(prefix_tokens.len() - 1)
            }
        } else if prefix_tokens.len() >= 2 {
            Some(prefix_tokens.len() - 2)
        } else {
            None
        };

        // Note: we deliberately do NOT offer stick-table completions after
        // `stick match|store-request|store-response`. Per HAProxy grammar the
        // next token is a sample expression, not a table name; the table is
        // carried by the optional `table <name>` clause (handled by Case B
        // in find_definition and by the ` table ` lookahead during parsing).

        // Cases 3 and 4: use_backend / default_backend / use_server.
        if let Some(idx) = kw_idx {
            match prefix_tokens[idx] {
                "use_backend" | "default_backend" => {
                    return self.complete_backends(uri, content);
                }
                "use_server" => {
                    return self
                        .complete_servers_in_enclosing_section(uri, content, line_idx);
                }
                _ => {}
            }
        }

        // Case 5: ACL condition after `if`/`unless`, outside of `{...}` group.
        if self.in_acl_condition(&prefix_tokens) {
            return self.complete_acls(uri, content);
        }

        // Case 6: start-of-line directive completion.
        // Triggered when the cursor sits inside the first token of the line
        // (or at col 0 on an otherwise-empty line).
        let is_first_token_context = prefix_tokens.is_empty()
            || (prefix_tokens.len() == 1 && !ends_with_whitespace);
        if is_first_token_context {
            if let Some(section_kw) = self.find_enclosing_section_keyword(content, line_idx) {
                return self.complete_directives(&section_kw);
            }
        }

        Vec::new()
    }

    fn in_acl_condition(&self, tokens: &[&str]) -> bool {
        // Only standalone `{` / `!{` / `}` tokens count as sample expression
        // delimiters — HAProxy requires whitespace around them. Braces
        // embedded in content tokens (regex literals like `^/foo\{$`,
        // PCRE quantifiers like `\d{3,}`) must not flip depth, otherwise
        // completion after a closing `}` would be silently dropped.
        let mut in_braces: i32 = 0;
        let mut saw_cond_kw = false;
        for tok in tokens {
            if *tok == "{" || *tok == "!{" {
                in_braces += 1;
            } else if *tok == "}" && in_braces > 0 {
                in_braces -= 1;
            }
            if in_braces == 0 && (*tok == "if" || *tok == "unless") {
                saw_cond_kw = true;
            }
        }
        saw_cond_kw && in_braces == 0
    }

    /// Walk up from `line_idx` (inclusive of body lines, exclusive of the
    /// header itself) to find the enclosing section keyword. Returns `None`
    /// when the cursor is on the header line or before any section.
    fn find_enclosing_section_keyword(
        &self,
        content: &str,
        line_idx: usize,
    ) -> Option<String> {
        let lines: Vec<&str> = content.lines().collect();
        if line_idx >= lines.len() {
            return None;
        }
        // If cursor line itself is a section header, do not offer directive
        // completions (we'd be typing into the header, not the body).
        if is_section_header(lines[line_idx]) {
            return None;
        }
        let mut i = line_idx;
        loop {
            if is_section_header(lines[i]) {
                let tokens: Vec<&str> = lines[i].split_whitespace().collect();
                return tokens.first().map(|s| s.to_string());
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    }

    fn complete_backends(&self, uri: &str, content: &str) -> Vec<Value> {
        let symbols = match self.symbols.get(uri) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let lines: Vec<&str> = content.lines().collect();
        let mut filtered: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Backend))
            .collect();
        filtered.sort_by(|a, b| {
            b.references
                .len()
                .cmp(&a.references.len())
                .then(a.name.cmp(&b.name))
        });
        filtered
            .iter()
            .map(|s| {
                let def_line = lines
                    .get(s.range.start.line as usize)
                    .copied()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                json!({
                    "label": s.name,
                    "kind": 7, // Class
                    "detail": def_line.clone(),
                    "documentation": {
                        "kind": "markdown",
                        "value": format!("```haproxy\n{}\n```", def_line),
                    },
                    "sortText": frequency_sort_key(s.references.len(), &s.name),
                })
            })
            .collect()
    }

    fn complete_acls(&self, uri: &str, content: &str) -> Vec<Value> {
        let symbols = match self.symbols.get(uri) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let lines: Vec<&str> = content.lines().collect();
        // Deduplicate by name: an ACL name may appear on multiple lines
        // (HAProxy allows multiple `acl NAME ...` declarations that OR
        // together). Collapse them so completion does not emit duplicate
        // labels.
        let mut seen: HashMap<String, (usize, String)> = HashMap::new();
        for s in symbols {
            if !matches!(s.kind, SymbolKind::Acl) {
                continue;
            }
            let def_line = lines
                .get(s.range.start.line as usize)
                .copied()
                .unwrap_or("")
                .trim()
                .to_string();
            let entry = seen
                .entry(s.name.clone())
                .or_insert_with(|| (0, def_line.clone()));
            // HAProxy allows multiple `acl NAME ...` lines for OR semantics,
            // and `add_reference_to_symbol` attaches each call-site to every
            // duplicate symbol. Take the max (all duplicates share the same
            // reference set) rather than summing, which would produce N*M.
            entry.0 = entry.0.max(s.references.len());
        }
        let mut items: Vec<(String, usize, String)> = seen
            .into_iter()
            .map(|(name, (count, line))| (name, count, line))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        items
            .into_iter()
            .map(|(name, count, def_line)| {
                json!({
                    "label": name,
                    "kind": 21, // Constant
                    "detail": def_line.clone(),
                    "documentation": {
                        "kind": "markdown",
                        "value": format!("```haproxy\n{}\n```", def_line),
                    },
                    "sortText": frequency_sort_key(count, &name),
                })
            })
            .collect()
    }

    fn complete_stick_tables(&self, uri: &str) -> Vec<Value> {
        let symbols = match self.symbols.get(uri) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let content = self.documents.get(uri).cloned().unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let mut filtered: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::StickTable))
            .collect();
        filtered.sort_by(|a, b| {
            b.references
                .len()
                .cmp(&a.references.len())
                .then(a.name.cmp(&b.name))
        });
        filtered
            .iter()
            .map(|s| {
                let def_line = lines
                    .get(s.range.start.line as usize)
                    .copied()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                json!({
                    "label": s.name,
                    "kind": 22, // Struct
                    "detail": def_line.clone(),
                    "documentation": {
                        "kind": "markdown",
                        "value": format!("```haproxy\n{}\n```", def_line),
                    },
                    "sortText": frequency_sort_key(s.references.len(), &s.name),
                })
            })
            .collect()
    }

    fn complete_servers_in_enclosing_section(
        &self,
        uri: &str,
        content: &str,
        line_idx: usize,
    ) -> Vec<Value> {
        let lines: Vec<&str> = content.lines().collect();
        if line_idx >= lines.len() {
            return Vec::new();
        }
        // Walk up to the nearest section header; only backend/listen own
        // server pools.
        let mut section_start: Option<usize> = None;
        let mut i = line_idx;
        loop {
            if is_section_header(lines[i]) {
                let tokens: Vec<&str> = lines[i].split_whitespace().collect();
                if let Some(kw) = tokens.first() {
                    if *kw == "backend" || *kw == "listen" {
                        section_start = Some(i);
                    }
                }
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
        let Some(start) = section_start else {
            return Vec::new();
        };
        let mut end = lines.len();
        for (j, ln) in lines.iter().enumerate().skip(start + 1) {
            if is_section_header(ln) {
                end = j;
                break;
            }
        }
        let symbols = match self.symbols.get(uri) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut items: Vec<&Symbol> = Vec::new();
        for s in symbols {
            if !matches!(s.kind, SymbolKind::Server) {
                continue;
            }
            let l = s.range.start.line as usize;
            if l > start && l < end {
                items.push(s);
            }
        }
        items.sort_by(|a, b| {
            b.references
                .len()
                .cmp(&a.references.len())
                .then(a.name.cmp(&b.name))
        });
        items
            .into_iter()
            .map(|s| {
                let def_line = lines
                    .get(s.range.start.line as usize)
                    .copied()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                json!({
                    "label": s.name,
                    "kind": 6, // Variable
                    "detail": def_line.clone(),
                    "documentation": {
                        "kind": "markdown",
                        "value": format!("```haproxy\n{}\n```", def_line),
                    },
                    "sortText": frequency_sort_key(s.references.len(), &s.name),
                })
            })
            .collect()
    }

    fn complete_directives(&self, section_kw: &str) -> Vec<Value> {
        let list = directives_for_section(section_kw);
        let mut items: Vec<&&'static str> = list.iter().collect();
        items.sort();
        items
            .into_iter()
            .map(|name| {
                let doc = docs::directive_doc(name).unwrap_or("");
                json!({
                    "label": name,
                    "kind": 14, // Keyword
                    "detail": format!("{} directive", section_kw),
                    "documentation": {
                        "kind": "markdown",
                        "value": doc,
                    },
                    "sortText": format!("000000_{}", name),
                })
            })
            .collect()
    }

    fn handle_request(&mut self, request: Value) -> Option<Value> {
        let method = request["method"].as_str()?;
        let id = &request["id"];

        match method {
            "initialize" => {
                // Capture workspace root from `initializationOptions.workspace_root`.
                // Zed's extension glue (see `src/lib.rs`) forwards
                // `worktree.root_path()` there so the LSP knows where to stop
                // when walking up looking for `.zed/haproxy.toml`.
                let opts = &request["params"]["initializationOptions"];
                if let Some(root) = opts.get("workspace_root").and_then(|v| v.as_str()) {
                    if !root.is_empty() {
                        self.workspace_root = Some(PathBuf::from(root));
                    }
                }
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "definitionProvider": true,
                            "declarationProvider": true,
                            "referencesProvider": true,
                            "renameProvider": { "prepareProvider": true },
                            "foldingRangeProvider": true,
                            "documentSymbolProvider": true,
                            "hoverProvider": true,
                            "completionProvider": {
                                "triggerCharacters": [" ", "("],
                                "resolveProvider": false
                            },
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 1
                            }
                        }
                    }
                }))
            }
            "textDocument/didOpen" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let content = params["textDocument"]["text"].as_str()?;

                // Resolve and cache the project configuration for this file
                // before parsing. Cross-file resolution (Task 5+) will read
                // from this cache to decide which sibling files to pull in.
                if let Some(file_path) = uri_to_path(uri) {
                    let cfg = resolve_project_config_for_path(
                        &file_path,
                        self.workspace_root.as_deref(),
                    );
                    self.project_configs.insert(uri.to_string(), cfg);
                }

                // Mark this URI as a client-owned buffer so subsequent
                // include-graph walks rooted elsewhere still trust the
                // in-memory copy over on-disk content for unsaved edits.
                self.explicitly_opened.insert(uri.to_string());

                if let Err(_) = self.parse_document(uri, content) {
                    eprintln!("Failed to parse document: {}", uri);
                }

                None // No response needed for notifications
            }
            "textDocument/didClose" => {
                // Evict all per-URI caches so long-lived sessions don't grow
                // unbounded as files are opened and closed.
                let params = &request["params"];
                if let Some(uri) = params["textDocument"]["uri"].as_str() {
                    self.symbols.remove(uri);
                    self.folds.remove(uri);
                    self.outline.remove(uri);
                    self.documents.remove(uri);
                    self.diagnostics.remove(uri);
                    self.project_configs.remove(uri);
                    self.included_files.remove(uri);
                    self.explicitly_opened.remove(uri);
                    // Clear any stale diagnostics the client may still show.
                    let uri_owned = uri.to_string();
                    self.send_notification(
                        "textDocument/publishDiagnostics",
                        json!({ "uri": uri_owned, "diagnostics": [] }),
                    );
                }
                None
            }
            "textDocument/didChange" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let changes = params["contentChanges"].as_array()?;
                
                if let Some(change) = changes.first() {
                    if let Some(content) = change["text"].as_str() {
                        if let Err(_) = self.parse_document(uri, content) {
                            eprintln!("Failed to parse document: {}", uri);
                        }
                    }
                }
                
                None // No response needed for notifications
            }
            "textDocument/definition" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                // Serve from the in-memory document cache populated on didOpen/didChange.
                // Keeps unsaved-buffer navigation correct.
                if let Some(content) = self.documents.get(uri).cloned() {
                    if let Some(symbol) = self.find_definition(uri, &position, &content) {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "uri": symbol.uri,
                                "range": {
                                    "start": {
                                        "line": symbol.range.start.line,
                                        "character": symbol.range.start.character
                                    },
                                    "end": {
                                        "line": symbol.range.end.line,
                                        "character": symbol.range.end.character
                                    }
                                }
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": null
                        }))
                    }
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": null
                    }))
                }
            }
            "textDocument/foldingRange" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let ranges: Vec<Value> = self
                    .folds
                    .get(uri)
                    .map(|v| {
                        v.iter()
                            .map(|r| {
                                json!({
                                    "startLine": r.start_line,
                                    "endLine": r.end_line,
                                    "kind": r.kind,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": ranges,
                }))
            }
            "textDocument/documentSymbol" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let syms: Vec<Value> = self
                    .outline
                    .get(uri)
                    .map(|v| v.iter().map(serialize_document_symbol).collect())
                    .unwrap_or_default();
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": syms,
                }))
            }
            "textDocument/references" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };
                // LSP spec: `context.includeDeclaration` defaults to false
                // when absent. Zed sends it explicitly, but other clients
                // (including the test harness) may omit it.
                let include_declaration = params["context"]["includeDeclaration"]
                    .as_bool()
                    .unwrap_or(false);

                let content_opt = self.documents.get(uri).cloned();
                let symbol = content_opt
                    .as_ref()
                    .and_then(|content| self.find_symbol_at_cursor(uri, &position, content));

                let locations: Vec<Value> = if let (Some(sym), Some(content)) =
                    (symbol, content_opt)
                {
                    // Narrow reference and declaration ranges to the identifier
                    // token so clients like Zed highlight the symbol itself
                    // rather than the whole line. Falls back to the stored
                    // line-span range if the raw line can't be located or the
                    // identifier can't be found in it.
                    let lines: Vec<&str> = content.lines().collect();
                    let mut locs: Vec<Value> = Vec::new();
                    let mut seen_locs: std::collections::HashSet<(u32, u32)> =
                        std::collections::HashSet::new();
                    // Per-(line, context) next-search offset. A line may carry
                    // the same symbol more than once (e.g. a stick-table
                    // `... table rate ... sc0_*(rate) ...` or an ACL repeated
                    // in a condition `if foo || foo`). Each recorded reference
                    // must map to a distinct occurrence, so after every match
                    // we advance the context-scoped search floor past its end.
                    // Keying on context as well as line keeps independent
                    // contexts on the same line (e.g. `use_backend foo if foo`)
                    // from clobbering each other's offsets.
                    let mut ref_search_floor: std::collections::HashMap<
                        (u32, ReferenceContext),
                        usize,
                    > = std::collections::HashMap::new();
                    let push_loc =
                        |line_num: u32,
                         start: u32,
                         end: u32,
                         locs: &mut Vec<Value>,
                         seen: &mut std::collections::HashSet<(u32, u32)>| {
                            if seen.insert((line_num, start)) {
                                locs.push(json!({
                                    "uri": sym.uri,
                                    "range": {
                                        "start": { "line": line_num, "character": start },
                                        "end": { "line": line_num, "character": end },
                                    }
                                }));
                            }
                        };
                    let push_narrow = |line_num: u32,
                                           stored_start: &Position,
                                           stored_end: &Position,
                                           name: &str,
                                           search_from_hint: Option<usize>,
                                           locs: &mut Vec<Value>,
                                           seen: &mut std::collections::HashSet<(u32, u32)>| {
                        let (start_char, end_char) = lines
                            .get(line_num as usize)
                            .and_then(|raw| {
                                let start = search_from_hint.unwrap_or(0);
                                find_identifier_range(raw, name, start)
                            })
                            .unwrap_or((stored_start.character, stored_end.character));
                        push_loc(line_num, start_char, end_char, locs, seen);
                    };

                    // Collect all definition lines with the same name/kind
                    // (+scope for servers). Multiple ACL declarations share
                    // the same name — every one of them is a declaration and
                    // must surface when `includeDeclaration=true`.
                    let matching_defs: Vec<Symbol> = self
                        .symbols
                        .get(uri)
                        .map(|syms| {
                            syms.iter()
                                .filter(|s| {
                                    s.name == sym.name
                                        && s.kind == sym.kind
                                        && (sym.kind != SymbolKind::Server
                                            || s.scope == sym.scope)
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();

                    if include_declaration {
                        for def in &matching_defs {
                            let def_line_idx = def.range.start.line as usize;
                            let search_from = lines
                                .get(def_line_idx)
                                .and_then(|raw| def_line_search_from(raw, &def.kind));
                            push_narrow(
                                def.range.start.line,
                                &def.range.start,
                                &def.range.end,
                                &def.name,
                                search_from,
                                &mut locs,
                                &mut seen_locs,
                            );
                        }
                    }
                    for r in &sym.references {
                        let ref_line_idx = r.range.start.line as usize;
                        let base_search_from = lines
                            .get(ref_line_idx)
                            .map(|raw| ref_line_search_from(raw, r));
                        let key = (r.range.start.line, r.context.clone());
                        let precise = ref_context_has_precise_position(&r.context);
                        let floor = if precise {
                            None
                        } else {
                            ref_search_floor.get(&key).copied()
                        };
                        let search_from = match (base_search_from, floor) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (Some(a), None) => Some(a),
                            (None, Some(b)) => Some(b),
                            (None, None) => None,
                        };
                        if let Some(raw) = lines.get(ref_line_idx) {
                            if let Some((s, e)) = find_identifier_range(
                                raw,
                                &sym.name,
                                search_from.unwrap_or(0),
                            ) {
                                push_loc(
                                    r.range.start.line,
                                    s,
                                    e,
                                    &mut locs,
                                    &mut seen_locs,
                                );
                                if !precise {
                                    ref_search_floor.insert(key, e as usize);
                                }
                                continue;
                            }
                        }
                        push_narrow(
                            r.range.start.line,
                            &r.range.start,
                            &r.range.end,
                            &sym.name,
                            search_from,
                            &mut locs,
                            &mut seen_locs,
                        );
                    }

                    // Cascade stick-table references into section symbols.
                    // A backend/frontend/listen that owns a stick-table is
                    // conceptually one name; references like `sc0_*(X)` and
                    // `... table X` target the table, but operators expect
                    // them to surface when asking for references on the
                    // enclosing section of the same name.
                    if matches!(
                        sym.kind,
                        SymbolKind::Backend | SymbolKind::Frontend | SymbolKind::Listen
                    ) {
                        if let Some(table) =
                            self.find_symbol_by_name(uri, &sym.name, SymbolKind::StickTable)
                        {
                            if include_declaration {
                                let def_line_idx = table.range.start.line as usize;
                                // Stick-table def line has no identifier to
                                // anchor on; fall through to the stored range.
                                push_loc(
                                    table.range.start.line,
                                    table.range.start.character,
                                    lines
                                        .get(def_line_idx)
                                        .map(|l| l.len() as u32)
                                        .unwrap_or(table.range.end.character),
                                    &mut locs,
                                    &mut seen_locs,
                                );
                            }
                            for r in &table.references {
                                let ref_line_idx = r.range.start.line as usize;
                                let base_search_from = lines
                                    .get(ref_line_idx)
                                    .map(|raw| ref_line_search_from(raw, r));
                                let key = (r.range.start.line, r.context.clone());
                                let precise = ref_context_has_precise_position(&r.context);
                                let floor = if precise {
                                    None
                                } else {
                                    ref_search_floor.get(&key).copied()
                                };
                                let search_from = match (base_search_from, floor) {
                                    (Some(a), Some(b)) => Some(a.max(b)),
                                    (Some(a), None) => Some(a),
                                    (None, Some(b)) => Some(b),
                                    (None, None) => None,
                                };
                                if let Some(raw) = lines.get(ref_line_idx) {
                                    if let Some((s, e)) = find_identifier_range(
                                        raw,
                                        &table.name,
                                        search_from.unwrap_or(0),
                                    ) {
                                        push_loc(
                                            r.range.start.line,
                                            s,
                                            e,
                                            &mut locs,
                                            &mut seen_locs,
                                        );
                                        if !precise {
                                            ref_search_floor.insert(key, e as usize);
                                        }
                                        continue;
                                    }
                                }
                                push_narrow(
                                    r.range.start.line,
                                    &r.range.start,
                                    &r.range.end,
                                    &table.name,
                                    search_from,
                                    &mut locs,
                                    &mut seen_locs,
                                );
                            }
                        }
                    }

                    locs
                } else {
                    Vec::new()
                };

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": locations,
                }))
            }
            "textDocument/prepareRename" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                // prepareRename returns the exact identifier range so Zed
                // pre-fills the rename box. Resolution:
                //   1. Cursor must sit on a word.
                //   2. find_symbol_at_cursor must resolve to a renameable
                //      symbol (StickTable intentionally excluded — it is
                //      bound to the enclosing section name).
                //   3. The cursor word must equal the symbol's name. This
                //      guards the cursor-on-definition-line fallback in
                //      find_symbol_at_cursor, which would otherwise claim
                //      the `backend` keyword itself is a renameable token.
                let result: Value = (|| -> Option<Value> {
                    let content = self.documents.get(uri)?.clone();
                    let lines: Vec<&str> = content.lines().collect();
                    let line_idx = position.line as usize;
                    if line_idx >= lines.len() {
                        return None;
                    }
                    let line = lines[line_idx];
                    let (word, word_start) =
                        self.word_at_position(line, position.character as usize)?;
                    let symbol = self.find_symbol_at_cursor(uri, &position, &content)?;
                    let renameable = matches!(
                        symbol.kind,
                        SymbolKind::Backend
                            | SymbolKind::Frontend
                            | SymbolKind::Listen
                            | SymbolKind::Acl
                            | SymbolKind::Server
                    );
                    if !renameable {
                        return None;
                    }
                    if word != symbol.name {
                        return None;
                    }
                    let start_col = word_start as u32;
                    let end_col = start_col + word.len() as u32;
                    Some(json!({
                        "range": {
                            "start": { "line": position.line, "character": start_col },
                            "end": { "line": position.line, "character": end_col },
                        },
                        "placeholder": word,
                    }))
                })()
                .unwrap_or(Value::Null);

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }
            "textDocument/rename" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };
                let new_name = params["newName"].as_str()?;

                // Validate new name against the grammar's identifier charset.
                // Rejecting invalid names with a structured JSON-RPC error
                // lets Zed surface a clear message instead of silently
                // applying a malformed edit.
                if !is_valid_identifier(new_name) {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": format!(
                                "Invalid rename: {:?} must be non-empty and contain only [a-zA-Z0-9_.-]",
                                new_name
                            ),
                        }
                    }));
                }

                let content = match self.documents.get(uri).cloned() {
                    Some(c) => c,
                    None => {
                        return Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": Value::Null,
                        }));
                    }
                };

                let symbol = match self.find_symbol_at_cursor(uri, &position, &content) {
                    Some(s) => s,
                    None => {
                        return Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": Value::Null,
                        }));
                    }
                };

                // Mirror the prepareRename contract: the cursor word must equal
                // the resolved symbol's name. `find_symbol_at_cursor` resolves
                // by scanning the line for the definition token, so it would
                // otherwise accept cursor positions on the keyword, address,
                // or option fields of a definition line — which violates the
                // safe-rename contract for clients that skip prepareRename.
                let word_matches_symbol = {
                    let lines: Vec<&str> = content.lines().collect();
                    let line_idx = position.line as usize;
                    lines
                        .get(line_idx)
                        .and_then(|line| {
                            self.word_at_position(line, position.character as usize)
                        })
                        .map(|(word, _)| word == symbol.name)
                        .unwrap_or(false)
                };
                if !word_matches_symbol {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": Value::Null,
                    }));
                }

                // Stick-tables are bound to the enclosing section name in
                // HAProxy's grammar (one table per section), so renaming a
                // stick-table independent of its section is not meaningful
                // — the user must rename the section instead.
                if matches!(symbol.kind, SymbolKind::StickTable) {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": "Stick-tables cannot be renamed directly; rename the enclosing section instead",
                        }
                    }));
                }

                let lines: Vec<&str> = content.lines().collect();
                let mut edits: Vec<Value> = Vec::new();
                // Dedup by (line, start) so identical edits from duplicate
                // reference entries (e.g. the same ACL appearing twice in a
                // condition) don't produce overlapping TextEdits, which
                // violates the LSP WorkspaceEdit invariant.
                let mut seen: std::collections::HashSet<(u32, u32)> =
                    std::collections::HashSet::new();
                // Per-(line, context) next-search offset. A line may carry
                // the same symbol more than once (`... table rate ...
                // sc0_*(rate) ...`, `if foo || foo`). Each recorded reference
                // must map to its own occurrence; without advancing a
                // per-context floor past the previous match, all duplicate
                // same-name references on a line collapse onto the first
                // occurrence and the second/third/… stay stale after rename.
                let mut ref_search_floor: std::collections::HashMap<
                    (u32, ReferenceContext),
                    usize,
                > = std::collections::HashMap::new();
                let mut push_edit = |line: u32, start: u32, end: u32, edits: &mut Vec<Value>| {
                    if seen.insert((line, start)) {
                        edits.push(json!({
                            "range": {
                                "start": { "line": line, "character": start },
                                "end": { "line": line, "character": end },
                            },
                            "newText": new_name,
                        }));
                    }
                };

                // Definition edits. HAProxy allows multiple `acl NAME ...`
                // lines for OR semantics, so rename must rewrite every same-
                // name/kind definition, not just the one at the cursor — a
                // partial rename would leave an orphan declaration and
                // silently break the config. For servers the match also
                // scope-filters by enclosing section so two backends with a
                // same-named server stay independent.
                let all_defs: Vec<(u32, String)> = self
                    .symbols
                    .get(uri)
                    .map(|syms| {
                        syms.iter()
                            .filter(|s| {
                                s.name == symbol.name
                                    && s.kind == symbol.kind
                                    && (symbol.kind != SymbolKind::Server
                                        || s.scope == symbol.scope)
                            })
                            .map(|s| (s.range.start.line, s.name.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                for (def_line_num, _) in &all_defs {
                    let def_line_idx = *def_line_num as usize;
                    if def_line_idx >= lines.len() {
                        continue;
                    }
                    let def_line = lines[def_line_idx];
                    if let Some(search_from) = def_line_search_from(def_line, &symbol.kind) {
                        if let Some((s, e)) =
                            find_identifier_range(def_line, &symbol.name, search_from)
                        {
                            push_edit(*def_line_num, s, e, &mut edits);
                        }
                    }
                }

                // Reference edits. Advance per-(line, context) floor after
                // each match so multiple same-context references on one line
                // pick up successive occurrences instead of collapsing onto
                // the first.
                for r in &symbol.references {
                    let ref_line_idx = r.range.start.line as usize;
                    if ref_line_idx >= lines.len() {
                        continue;
                    }
                    let ref_line = lines[ref_line_idx];
                    let base_search_from = ref_line_search_from(ref_line, r);
                    let key = (r.range.start.line, r.context.clone());
                    let precise = ref_context_has_precise_position(&r.context);
                    let search_from = if precise {
                        base_search_from
                    } else {
                        let floor = ref_search_floor.get(&key).copied().unwrap_or(0);
                        base_search_from.max(floor)
                    };
                    if let Some((s, e)) =
                        find_identifier_range(ref_line, &symbol.name, search_from)
                    {
                        push_edit(r.range.start.line, s, e, &mut edits);
                        if !precise {
                            ref_search_floor.insert(key, e as usize);
                        }
                    }
                }

                // Cascade stick-table references into section renames.
                // The stick-table is bound to the enclosing section's name,
                // so renaming the section must also rewrite every
                // `sc*_*(X)` / `... table X` / `stick on ... table X`
                // call-site that names the section's table. Without this,
                // renaming the section silently leaves call-sites pointing
                // at a non-existent table.
                if matches!(
                    symbol.kind,
                    SymbolKind::Backend | SymbolKind::Frontend | SymbolKind::Listen
                ) {
                    if let Some(table) =
                        self.find_symbol_by_name(uri, &symbol.name, SymbolKind::StickTable)
                    {
                        for r in &table.references {
                            let ref_line_idx = r.range.start.line as usize;
                            if ref_line_idx >= lines.len() {
                                continue;
                            }
                            let ref_line = lines[ref_line_idx];
                            let base_search_from = ref_line_search_from(ref_line, r);
                            let key = (r.range.start.line, r.context.clone());
                            let precise = ref_context_has_precise_position(&r.context);
                            let search_from = if precise {
                                base_search_from
                            } else {
                                let floor = ref_search_floor.get(&key).copied().unwrap_or(0);
                                base_search_from.max(floor)
                            };
                            if let Some((s, e)) =
                                find_identifier_range(ref_line, &table.name, search_from)
                            {
                                push_edit(r.range.start.line, s, e, &mut edits);
                                if !precise {
                                    ref_search_floor.insert(key, e as usize);
                                }
                            }
                        }
                    }
                }

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "changes": {
                            uri: edits,
                        }
                    }
                }))
            }
            "textDocument/completion" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                let items: Vec<Value> = self
                    .documents
                    .get(uri)
                    .cloned()
                    .map(|content| self.compute_completions(uri, &position, &content))
                    .unwrap_or_default();

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "isIncomplete": false,
                        "items": items,
                    }
                }))
            }
            "textDocument/hover" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                let result = self
                    .documents
                    .get(uri)
                    .cloned()
                    .and_then(|content| self.find_hover(uri, &position, &content))
                    .map(|value| {
                        json!({
                            "contents": {
                                "kind": "markdown",
                                "value": value,
                            }
                        })
                    })
                    .unwrap_or(Value::Null);

                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }
            "textDocument/declaration" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                // Serve from the in-memory document cache populated on didOpen/didChange.
                if let Some(content) = self.documents.get(uri).cloned() {
                    if let Some(references) = self.find_declaration(uri, &position, &content) {
                        // Return array of locations for multiple references
                        let locations: Vec<Value> = references.into_iter().map(|reference| {
                            json!({
                                "uri": reference.uri,
                                "range": {
                                    "start": {
                                        "line": reference.range.start.line,
                                        "character": reference.range.start.character
                                    },
                                    "end": {
                                        "line": reference.range.end.line,
                                        "character": reference.range.end.character
                                    }
                                }
                            })
                        }).collect();

                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": locations
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": []
                        }))
                    }
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": []
                    }))
                }
            }
            "$/haproxy/projectIndex" => {
                // Introspection request used by the test harness to verify the
                // cross-file symbol index. Returns the project root, the list
                // of URIs in the include graph, and a flat list of every
                // symbol aggregated across them. Emitted as stable-sorted by
                // URI then by (line, character) so tests can compare
                // deterministically.
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let root_key = self
                    .project_configs
                    .get(uri)
                    .map(|cfg| cfg.project_root.to_string_lossy().into_owned());
                let result = match root_key.as_deref().and_then(|k| self.project_indices.get(k)) {
                    Some(idx) => {
                        let mut symbols: Vec<Value> = Vec::new();
                        for ((kind, name), refs) in &idx.symbols_by_name {
                            for r in refs {
                                symbols.push(json!({
                                    "name": name,
                                    "kind": symbol_kind_name(kind),
                                    "uri": r.uri,
                                    "range": {
                                        "start": { "line": r.range.start.line, "character": r.range.start.character },
                                        "end": { "line": r.range.end.line, "character": r.range.end.character },
                                    },
                                    "scope": r.scope,
                                }));
                            }
                        }
                        symbols.sort_by(|a, b| {
                            let au = a["uri"].as_str().unwrap_or("");
                            let bu = b["uri"].as_str().unwrap_or("");
                            au.cmp(bu)
                                .then_with(|| {
                                    a["range"]["start"]["line"]
                                        .as_u64()
                                        .unwrap_or(0)
                                        .cmp(&b["range"]["start"]["line"].as_u64().unwrap_or(0))
                                })
                                .then_with(|| {
                                    a["range"]["start"]["character"]
                                        .as_u64()
                                        .unwrap_or(0)
                                        .cmp(&b["range"]["start"]["character"].as_u64().unwrap_or(0))
                                })
                        });
                        json!({
                            "project_root": idx.project_root.to_string_lossy(),
                            "uris": idx.uris,
                            "symbols": symbols,
                        })
                    }
                    None => Value::Null,
                };
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }
            "$/haproxy/projectInfo" => {
                // Introspection request used by the test harness to verify
                // project-config discovery. Returns the cached `ProjectConfig`
                // for the given URI, or a `null` result if the file was never
                // opened / has been closed.
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("");
                let result = match self.project_configs.get(uri) {
                    Some(cfg) => json!({
                        "project_root": cfg.project_root.to_string_lossy(),
                        "follow_includes": cfg.follow_includes,
                        "extra_files": cfg.extra_files,
                        "config_file": cfg.config_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        "workspace_root": self.workspace_root.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    }),
                    None => Value::Null,
                };
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }))
            }
            _ => {
                // LSP requests (those with a non-null `id`) require a response;
                // notifications (null id) do not. Reply with method-not-found
                // for unknown requests so clients don't hang waiting.
                if id.is_null() {
                    None
                } else {
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("Method not found: {}", method),
                        }
                    }))
                }
            }
        }
    }
}


// Cap per-message size to avoid unbounded allocation on malicious/malformed
// Content-Length. 64 MiB is far larger than any reasonable HAProxy config.
const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut lsp = HaproxyLsp::new()?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = io::stdout();

    loop {
        // Read LSP message headers until blank line. Per LSP spec, multiple
        // headers (e.g. Content-Type in addition to Content-Length) may
        // precede the body; header names are case-insensitive.
        let mut content_length: Option<usize> = None;
        let mut eof = false;
        loop {
            let mut header_line = String::new();
            let bytes_read = stdin.read_line(&mut header_line)?;
            if bytes_read == 0 {
                eof = true;
                break;
            }
            let trimmed = header_line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse::<usize>().ok();
                }
            }
        }
        if eof {
            break;
        }

        let content_length = match content_length {
            Some(n) if n > 0 && n <= MAX_CONTENT_LENGTH => n,
            Some(n) if n > MAX_CONTENT_LENGTH => {
                eprintln!("Content-Length {} exceeds cap {}; dropping frame", n, MAX_CONTENT_LENGTH);
                // Drain the oversized body so the next read doesn't
                // consume mid-JSON bytes as header bytes and desync the
                // stream. Errors here are fatal (stream is already lost).
                io::copy(&mut (&mut stdin).take(n as u64), &mut io::sink())?;
                continue;
            }
            _ => continue, // missing/zero/invalid length: resync on next header block
        };

        // Read the JSON content
        let mut buffer = vec![0; content_length];
        stdin.read_exact(&mut buffer)?;
        let content = match String::from_utf8(buffer) {
            Ok(s) => s,
            Err(err) => {
                // Malformed UTF-8: log and continue. Don't kill the server on
                // one bad message — subsequent frames may be fine.
                eprintln!("Skipping frame with invalid UTF-8: {}", err);
                continue;
            }
        };

        // Parse JSON-RPC request
        if let Ok(request) = serde_json::from_str::<Value>(&content) {
            let response = lsp.handle_request(request);
            if let Some(response) = response {
                let response_str = serde_json::to_string(&response)?;
                let response_len = response_str.len();

                // Write LSP response with headers
                write!(stdout, "Content-Length: {}\r\n\r\n{}", response_len, response_str)?;
                stdout.flush()?;
            }
            // Drain any notifications queued by the handler (e.g.
            // `textDocument/publishDiagnostics` emitted from `parse_document`).
            // Written after the response so request/response ordering stays
            // intact; stdout is single-writer so framing is never interleaved.
            for notification in lsp.drain_notifications() {
                let msg = serde_json::to_string(&notification)?;
                let msg_len = msg.len();
                write!(stdout, "Content-Length: {}\r\n\r\n{}", msg_len, msg)?;
                stdout.flush()?;
            }
        }
    }

    Ok(())
}