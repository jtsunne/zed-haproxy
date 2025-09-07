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
                    });
                }
            }
        }
        
        self.symbols.insert(uri.to_string(), symbols);
        Ok(())
    }

    fn find_definition(&self, _uri: &str, position: &Position, content: &str) -> Option<Symbol> {
        // Find what symbol is at the given position using simple text parsing
        let lines: Vec<&str> = content.lines().collect();
        if position.line as usize >= lines.len() {
            return None;
        }
        
        let line = lines[position.line as usize];
        let char_pos = position.character as usize;
        
        // Find word boundaries around the cursor position
        let word = self.get_word_at_position(line, char_pos)?;
        
        // Check if this line contains a reference pattern
        if line.contains("use_backend") {
            // Extract the backend name from "use_backend BACKEND_NAME"
            if let Some(backend_name) = self.extract_backend_from_use_backend(line) {
                return self.find_symbol_by_name(&backend_name, SymbolKind::Backend);
            }
        } else if line.contains("default_backend") {
            // Extract the backend name from "default_backend BACKEND_NAME"
            if let Some(backend_name) = self.extract_backend_from_default_backend(line) {
                return self.find_symbol_by_name(&backend_name, SymbolKind::Backend);
            }
        } else if line.contains(" if ") && !word.is_empty() {
            // Check if this might be an ACL reference in a condition
            return self.find_symbol_by_name(&word, SymbolKind::Acl);
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
    
    fn get_word_at_position(&self, line: &str, char_pos: usize) -> Option<String> {
        if char_pos >= line.len() {
            return None;
        }
        
        // Find word boundaries
        let chars: Vec<char> = line.chars().collect();
        
        // If we're on whitespace, try to find the next word
        let mut pos = char_pos;
        if pos < chars.len() && chars[pos].is_whitespace() {
            while pos < chars.len() && chars[pos].is_whitespace() {
                pos += 1;
            }
        }
        
        if pos >= chars.len() {
            return None;
        }
        
        // Now find word boundaries from this position
        let mut start = pos;
        let mut end = pos;
        
        // Find start of word
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_' || chars[start - 1] == '-') {
            start -= 1;
        }
        
        // Find end of word
        while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '-') {
            end += 1;
        }
        
        if start < end {
            Some(chars[start..end].iter().collect())
        } else {
            None
        }
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