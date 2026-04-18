use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

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
    ServerReference,
}

#[derive(Debug, Clone, PartialEq)]
enum SymbolKind {
    Backend,
    Frontend,
    Listen,
    Acl,
    Server,
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
        let name_start_col = if !name.is_empty() {
            line.find(&name).map(|n| n as u32).unwrap_or(keyword.len() as u32)
        } else {
            0
        };
        headers.push(HeaderInfo {
            keyword,
            name,
            name_start_col,
            header_line: i as u32,
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
                    "acl" if (h.keyword == "frontend" || h.keyword == "listen")
                        && tokens.len() >= 3 =>
                    {
                        let acl_name = tokens[1];
                        let criterion: String = tokens[2..].join(" ");
                        let detail = truncate_detail(&criterion, 40);
                        let name_start = ln.find(acl_name).map(|n| n as u32).unwrap_or(0);
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
                        let name_start = ln.find(srv_name).map(|n| n as u32).unwrap_or(0);
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
                        let name_start = ln.find(ns_name).map(|n| n as u32).unwrap_or(0);
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
                if binds.is_empty() {
                    None
                } else {
                    Some(binds.join(", "))
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
        
        for (line_num, line) in content.lines().enumerate() {
            let line = line.trim();
            
            // Parse backend definitions
            if line.starts_with("backend ") {
                let name = line.strip_prefix("backend ").unwrap_or("").trim();
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
                let name = line.strip_prefix("frontend ").unwrap_or("").trim();
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
                let name = line.strip_prefix("listen ").unwrap_or("").trim();
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

    fn find_definition(&self, _uri: &str, position: &Position, content: &str) -> Option<Symbol> {
        let lines: Vec<&str> = content.lines().collect();
        let line_idx = position.line as usize;
        if line_idx >= lines.len() {
            return None;
        }
        let line = lines[line_idx];

        // Resolve the word under the cursor and its byte offset on the line.
        let (word, word_start) = self.word_at_position(line, position.character as usize)?;

        // The last non-whitespace token before the word tells us what role the
        // word is playing, which disambiguates e.g. `use_backend X if Y` where
        // X is a backend reference and Y is an ACL reference.
        let preceding = line[..word_start].split_whitespace().last();

        let hint: Option<SymbolKind> = match preceding {
            Some("use_backend") | Some("default_backend") => Some(SymbolKind::Backend),
            Some("if") | Some("unless") | Some("&&") | Some("||") | Some("!") => {
                Some(SymbolKind::Acl)
            }
            // F12 on a definition line's name resolves to the definition itself.
            Some("backend") => Some(SymbolKind::Backend),
            Some("frontend") => Some(SymbolKind::Frontend),
            Some("listen") => Some(SymbolKind::Listen),
            Some("acl") => Some(SymbolKind::Acl),
            Some("server") => Some(SymbolKind::Server),
            _ => None,
        };

        if let Some(kind) = hint {
            if let Some(sym) = self.find_symbol_by_name(&word, kind) {
                return Some(sym);
            }
        }

        // Fallback: if the preceding-token heuristic didn't match, try every
        // symbol kind. This covers cursor placement on tokens whose context we
        // don't explicitly recognize.
        for kind in [
            SymbolKind::Backend,
            SymbolKind::Acl,
            SymbolKind::Frontend,
            SymbolKind::Listen,
            SymbolKind::Server,
        ] {
            if let Some(sym) = self.find_symbol_by_name(&word, kind) {
                return Some(sym);
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
            // Skip HAProxy operators and keywords
            if part == "||" || part == "&&" || part == "!" || part.starts_with('!') || part == "{" {
                continue;
            }
            // Stop at opening brace or other control characters
            if part.contains('{') {
                break;
            }
            // Remove negation prefix and add ACL name
            let clean_name = part.trim_start_matches('!').trim();
            if !clean_name.is_empty() && clean_name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
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
        let is_word_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-';

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

    fn find_symbol_by_name(&self, name: &str, kind: SymbolKind) -> Option<Symbol> {
        for symbols in self.symbols.values() {
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
        
        // Check if this line defines a backend
        if line.trim().starts_with("backend ") {
            let name = line.trim().strip_prefix("backend ").unwrap_or("").trim();
            if !name.is_empty() {
                return self.find_references_to_symbol(name, SymbolKind::Backend);
            }
        }
        
        // Check if this line defines an ACL
        if line.trim().starts_with("acl ") {
            let parts: Vec<&str> = line.trim().split_whitespace().collect();
            if parts.len() >= 2 {
                let name = parts[1];
                return self.find_references_to_symbol(name, SymbolKind::Acl);
            }
        }
        
        // Check if this line defines a frontend
        if line.trim().starts_with("frontend ") {
            let name = line.trim().strip_prefix("frontend ").unwrap_or("").trim();
            if !name.is_empty() {
                return self.find_references_to_symbol(name, SymbolKind::Frontend);
            }
        }
        
        // Check if this line defines a listen section
        if line.trim().starts_with("listen ") {
            let name = line.trim().strip_prefix("listen ").unwrap_or("").trim();
            if !name.is_empty() {
                return self.find_references_to_symbol(name, SymbolKind::Listen);
            }
        }
        
        // Check if this line defines a server
        if line.trim().trim_start().starts_with("server ") {
            let parts: Vec<&str> = line.trim().trim_start().split_whitespace().collect();
            if parts.len() >= 2 {
                let name = parts[1];
                return self.find_references_to_symbol(name, SymbolKind::Server);
            }
        }
        
        None
    }
    
    fn find_references_to_symbol(&self, symbol_name: &str, symbol_kind: SymbolKind) -> Option<Vec<Reference>> {
        for symbols in self.symbols.values() {
            for symbol in symbols {
                if symbol.name == symbol_name && std::mem::discriminant(&symbol.kind) == std::mem::discriminant(&symbol_kind) {
                    if symbol.references.is_empty() {
                        return None;
                    } else {
                        return Some(symbol.references.clone());
                    }
                }
            }
        }
        None
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
                            "foldingRangeProvider": true,
                            "documentSymbolProvider": true,
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