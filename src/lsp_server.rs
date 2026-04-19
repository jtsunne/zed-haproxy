use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

mod docs;

#[derive(Debug, Clone)]
struct Symbol {
    name: String,
    kind: SymbolKind,
    range: Range,
    uri: String,
    references: Vec<Reference>,
}

#[derive(Debug, Clone)]
struct Reference {
    range: Range,
    uri: String,
    context: ReferenceContext,
}

#[derive(Debug, Clone, PartialEq)]
enum ReferenceContext {
    UseBackend,
    DefaultBackend,
    AclCondition,
    AclUnlessCondition,
    StickTable,
}

#[derive(Debug, Clone, PartialEq)]
enum SymbolKind {
    Backend,
    Frontend,
    Listen,
    Acl,
    Server,
    StickTable,
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
}

const SECTION_KEYWORDS: &[&str] = &[
    "global", "defaults", "frontend", "backend", "listen", "resolvers",
    "userlist", "peers", "mailers", "cache", "program", "ring",
];

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
fn collect_stick_table_references(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
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
                            let inside = &line[paren_abs + 1..close_abs];
                            let first_arg = inside.split(',').next().unwrap_or("").trim();
                            if !first_arg.is_empty() && is_valid_identifier(first_arg) {
                                out.push(first_arg.to_string());
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

    // `stick match X`, `stick store-request X`, `stick store-response X`.
    // Note: per the plan, the token after `match`/`store-*` is treated as the
    // table name. In real HAProxy configs this token is usually a sample
    // expression (e.g. `src`), not a table — the explicit `table <name>`
    // clause is what carries the name. Spurious names get filtered by the
    // existence check in `add_reference_to_symbol`.
    if let Some(rest) = line.strip_prefix("stick ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() >= 2 {
            let kw = parts[0];
            if matches!(kw, "match" | "store-request" | "store-response") {
                if is_valid_identifier(parts[1]) {
                    out.push(parts[1].to_string());
                }
            }
        }
    }

    // Generic ` table <name>` anywhere on the line.
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].find(" table ") {
        let abs = search_from + rel + " table ".len();
        let tail = &line[abs..];
        if let Some(name) = tail.split_whitespace().next() {
            if is_valid_identifier(name) {
                out.push(name.to_string());
            }
        }
        search_from = abs;
    }

    out
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

/// Byte offset just past the reference-context keyword on a reference line.
/// For contexts without a fixed leading keyword (server references,
/// stick-table references) the search starts at the first non-whitespace
/// column, relying on the word-bounded match in `find_identifier_range` to
/// skip stray substring hits.
fn ref_line_search_from(line: &str, ctx: &ReferenceContext) -> usize {
    let trimmed_start = line.len() - line.trim_start().len();
    match ctx {
        ReferenceContext::UseBackend => line
            .find("use_backend")
            .map(|p| p + "use_backend".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::DefaultBackend => line
            .find("default_backend")
            .map(|p| p + "default_backend".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::AclCondition => line
            .find(" if ")
            .map(|p| p + " if ".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::AclUnlessCondition => line
            .find(" unless ")
            .map(|p| p + " unless ".len())
            .unwrap_or(trimmed_start),
        ReferenceContext::StickTable => trimmed_start,
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
        })
    }

    fn parse_document(&mut self, uri: &str, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        // For now, use simple regex-based parsing until tree-sitter integration is complete
        let mut symbols = Vec::new();
        // Track the enclosing named section so `stick-table` directives can
        // be attributed to the correct backend/frontend/listen/peers name
        // (HAProxy binds one table per section, keyed by the section name).
        let mut current_section_name: Option<String> = None;

        for (line_num, line) in content.lines().enumerate() {
            let line = line.trim();

            // Update section tracker before per-directive parsing so that
            // `stick-table` on a subsequent line attributes to this section.
            let first_tok = line.split_whitespace().next().unwrap_or("");
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

            // Parse backend definitions
            if line.starts_with("backend ") {
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
                    });
                }
            }
            // Parse frontend definitions
            else if line.starts_with("frontend ") {
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
                    });
                }
            }
            // Parse listen definitions
            else if line.starts_with("listen ") {
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
                    });
                }
            }
            // Parse server definitions
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
                    });
                }
            }
        }
        
        // Second pass: collect references to symbols
        let mut updated_symbols = symbols;
        for (line_num, line) in content.lines().enumerate() {
            let line = line.trim();
            
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
                                                  });
                    }
                }
            }

            // Collect stick-table references. Skip comment lines so commented
            // sample config in fixtures doesn't leak spurious references.
            if line.starts_with('#') {
                continue;
            }
            let stick_refs = collect_stick_table_references(line);
            for table_name in stick_refs {
                self.add_reference_to_symbol(
                    &mut updated_symbols,
                    &table_name,
                    SymbolKind::StickTable,
                    Reference {
                        range: Range {
                            start: Position { line: line_num as u32, character: 0 },
                            end: Position { line: line_num as u32, character: line.len() as u32 },
                        },
                        uri: uri.to_string(),
                        context: ReferenceContext::StickTable,
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
        // Case C: `stick match <word>`, `stick store-request <word>`,
        // `stick store-response <word>`. The plan treats the first positional
        // token after these keywords as the stick-table name.
        if prefix_tokens.len() >= 2 {
            let last = prefix_tokens[prefix_tokens.len() - 1];
            let prev = prefix_tokens[prefix_tokens.len() - 2];
            if prev == "stick"
                && matches!(last, "match" | "store-request" | "store-response")
            {
                return self.find_symbol_by_name(uri, &word, SymbolKind::StickTable);
            }
        }

        let kw_match = prefix_tokens.iter().enumerate().rev().find_map(|(idx, tok)| {
            match *tok {
                "use_backend" | "default_backend" | "backend" => Some((idx, SymbolKind::Backend)),
                "if" | "unless" => Some((idx, SymbolKind::Acl)),
                "frontend" => Some((idx, SymbolKind::Frontend)),
                "listen" => Some((idx, SymbolKind::Listen)),
                "acl" => Some((idx, SymbolKind::Acl)),
                "server" => Some((idx, SymbolKind::Server)),
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
            return self.find_symbol_by_name(uri, &word, kind);
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
    
    fn add_reference_to_symbol(&self, symbols: &mut Vec<Symbol>, symbol_name: &str, symbol_kind: SymbolKind, reference: Reference) {
        for symbol in symbols.iter_mut() {
            if symbol.name == symbol_name && std::mem::discriminant(&symbol.kind) == std::mem::discriminant(&symbol_kind) {
                symbol.references.push(reference);
                break;
            }
        }
    }
    
    fn extract_acl_names_from_condition(&self, line: &str, condition_type: &str) -> Option<Vec<String>> {
        // Find the condition part after "if" or "unless"
        let condition_start = line.find(&format!(" {} ", condition_type))?;
        let condition_part = &line[condition_start + condition_type.len() + 2..];
        
        // Simple parsing: split by whitespace and filter out operators and logical keywords
        let parts: Vec<&str> = condition_part.split_whitespace().collect();
        let mut acl_names = Vec::new();
        
        for part in parts {
            // Skip HAProxy operators and keywords.
            // Note: do NOT skip tokens that merely *start* with `!` — those are
            // negated ACL references (`if !foo.bar`) and must flow through to
            // the `trim_start_matches('!')` path below so the bare name is
            // recorded as a reference.
            if part == "||" || part == "&&" || part == "!" || part == "{" {
                continue;
            }
            // Stop at opening brace or other control characters
            if part.contains('{') {
                break;
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
        // Single-file scope: only resolve against the requesting document so
        // that two open files with the same backend/acl name don't silently
        // cross-navigate.
        if let Some(symbols) = self.symbols.get(uri) {
            for symbol in symbols {
                if symbol.name == name && std::mem::discriminant(&symbol.kind) == std::mem::discriminant(&kind) {
                    return Some(symbol.clone());
                }
            }
        }
        None
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

        // Check if this line defines a server
        if line.trim().trim_start().starts_with("server ") {
            let parts: Vec<&str> = line.trim().trim_start().split_whitespace().collect();
            if parts.len() >= 2 {
                let name = parts[1];
                return self.find_references_to_symbol(uri, name, SymbolKind::Server);
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
        if trimmed.starts_with("acl ") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(sym) = self.find_symbol_by_name(uri, parts[1], SymbolKind::Acl) {
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
                if let Some(sym) = self.find_symbol_by_name(uri, parts[1], SymbolKind::Server) {
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

        // Prefer cursor-aware resolution: `find_definition` uses the same
        // keyword walk-back as Go-to-Definition, so `sc0_*(name)` routes to
        // the StickTable kind even when a Backend of the same name exists
        // (stick-tables are conventionally co-named with their enclosing
        // backend). When it returns None, fall back to a by-name sweep
        // across kinds for the bare identifier paths (e.g. hovering the
        // backend name on its own header line, where the walk-back already
        // handles it — this fallback exists for defensive coverage of any
        // future call site that lacks a leading keyword).
        if let Some(sym) = self.find_definition(uri, position, content) {
            return Some(self.render_symbol_hover(content, &sym));
        }

        if let Some(sym) = self.find_symbol_by_name(uri, &word, SymbolKind::Backend) {
            return Some(self.backend_hover_body(content, &sym));
        }
        if let Some(sym) = self.find_symbol_by_name(uri, &word, SymbolKind::Acl) {
            return Some(self.line_hover_body(content, sym.range.start.line));
        }
        if let Some(sym) = self.find_symbol_by_name(uri, &word, SymbolKind::StickTable) {
            return Some(self.line_hover_body(content, sym.range.start.line));
        }
        if let Some(sym) = self.find_symbol_by_name(uri, &word, SymbolKind::Server) {
            return Some(self.line_hover_body(content, sym.range.start.line));
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
        // Single-file scope: look only in the requesting document.
        let symbols = self.symbols.get(uri)?;
        for symbol in symbols {
            if symbol.name == symbol_name && std::mem::discriminant(&symbol.kind) == std::mem::discriminant(&symbol_kind) {
                if symbol.references.is_empty() {
                    return None;
                } else {
                    return Some(symbol.references.clone());
                }
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
        let char_pos = (position.character as usize).min(line.len());
        let prefix = &line[..char_pos];

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

        // Case 2: `stick match|store-request|store-response <table>`.
        if let Some(idx) = kw_idx {
            if idx >= 1
                && prefix_tokens[idx - 1] == "stick"
                && matches!(
                    prefix_tokens[idx],
                    "match" | "store-request" | "store-response"
                )
            {
                return self.complete_stick_tables(uri);
            }
        }

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
        let mut in_braces: i32 = 0;
        let mut saw_cond_kw = false;
        for tok in tokens {
            for ch in tok.chars() {
                if ch == '{' {
                    in_braces += 1;
                } else if ch == '}' {
                    if in_braces > 0 {
                        in_braces -= 1;
                    }
                }
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
            entry.0 += s.references.len();
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

                let symbol = self
                    .documents
                    .get(uri)
                    .cloned()
                    .and_then(|content| self.find_symbol_at_cursor(uri, &position, &content));

                let locations: Vec<Value> = if let Some(sym) = symbol {
                    let mut locs: Vec<Value> = Vec::new();
                    if include_declaration {
                        locs.push(json!({
                            "uri": sym.uri,
                            "range": {
                                "start": {
                                    "line": sym.range.start.line,
                                    "character": sym.range.start.character,
                                },
                                "end": {
                                    "line": sym.range.end.line,
                                    "character": sym.range.end.character,
                                },
                            }
                        }));
                    }
                    for r in &sym.references {
                        locs.push(json!({
                            "uri": r.uri,
                            "range": {
                                "start": {
                                    "line": r.range.start.line,
                                    "character": r.range.start.character,
                                },
                                "end": {
                                    "line": r.range.end.line,
                                    "character": r.range.end.character,
                                },
                            }
                        }));
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

                // Definition edit.
                let def_line_idx = symbol.range.start.line as usize;
                if def_line_idx < lines.len() {
                    let def_line = lines[def_line_idx];
                    if let Some(search_from) = def_line_search_from(def_line, &symbol.kind) {
                        if let Some((s, e)) =
                            find_identifier_range(def_line, &symbol.name, search_from)
                        {
                            push_edit(symbol.range.start.line, s, e, &mut edits);
                        }
                    }
                }

                // Reference edits.
                for r in &symbol.references {
                    let ref_line_idx = r.range.start.line as usize;
                    if ref_line_idx >= lines.len() {
                        continue;
                    }
                    let ref_line = lines[ref_line_idx];
                    let search_from = ref_line_search_from(ref_line, &r.context);
                    if let Some((s, e)) =
                        find_identifier_range(ref_line, &symbol.name, search_from)
                    {
                        push_edit(r.range.start.line, s, e, &mut edits);
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
            _ => None,
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
            if let Some(response) = lsp.handle_request(request) {
                let response_str = serde_json::to_string(&response)?;
                let response_len = response_str.len();

                // Write LSP response with headers
                write!(stdout, "Content-Length: {}\r\n\r\n{}", response_len, response_str)?;
                stdout.flush()?;
            }
        }
    }

    Ok(())
}