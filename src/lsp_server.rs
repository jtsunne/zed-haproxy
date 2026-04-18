use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};
use tree_sitter::{Parser};

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

struct HaproxyLsp {
    parser: Parser,
    symbols: HashMap<String, Vec<Symbol>>,
}

impl HaproxyLsp {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let parser = Parser::new();
        // TODO: Set up the HAProxy language when tree-sitter integration is ready
        
        Ok(HaproxyLsp {
            parser,
            symbols: HashMap::new(),
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
        
        self.symbols.insert(uri.to_string(), updated_symbols);
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

                // For this basic implementation, we'll need to re-read the file content
                // In production, we'd cache the content from didOpen/didChange events
                if let Ok(content) = std::fs::read_to_string(uri.strip_prefix("file://").unwrap_or(uri)) {
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
            "textDocument/declaration" => {
                let params = &request["params"];
                let uri = params["textDocument"]["uri"].as_str()?;
                let position = Position {
                    line: params["position"]["line"].as_u64()? as u32,
                    character: params["position"]["character"].as_u64()? as u32,
                };

                // For this basic implementation, we'll need to re-read the file content
                if let Ok(content) = std::fs::read_to_string(uri.strip_prefix("file://").unwrap_or(uri)) {
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


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut lsp = HaproxyLsp::new()?;
    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        // Read LSP message with Content-Length header
        let mut header_line = String::new();
        let bytes_read = stdin.read_line(&mut header_line)?;
        
        if bytes_read == 0 {
            break; // EOF
        }
        
        // Parse Content-Length header
        let content_length = if header_line.starts_with("Content-Length: ") {
            header_line
                .strip_prefix("Content-Length: ")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0)
        } else {
            continue;
        };
        
        if content_length == 0 {
            continue;
        }
        
        // Read empty line separator
        let mut empty_line = String::new();
        stdin.read_line(&mut empty_line)?;
        
        // Read the JSON content
        let mut buffer = vec![0; content_length];
        stdin.read_exact(&mut buffer)?;
        let content = String::from_utf8(buffer)?;
        
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