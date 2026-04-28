use tower_lsp::lsp_types::{Location, Url};

use crate::indexer::Indexer;
use super::infer::find_declaration_range_in_lines;

/// Search for `name` in a specific file identified by its URI string.
///
/// Checks the in-memory symbol index first; falls back to raw line scanning
/// (for constructor parameters) and finally on-demand tree-sitter parsing.
pub(crate) fn find_name_in_uri(idx: &Indexer, name: &str, file_uri: &str) -> Vec<Location> {
    let Ok(uri) = Url::parse(file_uri) else { return vec![]; };

    // a) Indexed file – symbol table
    if let Some(f) = idx.files.get(file_uri) {
        if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
            return vec![Location { uri, range: sym.selection_range }];
        }
        // b) Line scan for constructor params / un-indexed declarations
        if let Some(range) = find_declaration_range_in_lines(&f.lines, name) {
            return vec![Location { uri, range }];
        }
        return vec![];
    }

    // c) File not yet indexed — parse on demand using the correct parser
    if let Ok(path) = uri.to_file_path() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let file_data = crate::parser::parse_by_extension(file_uri, &content);
            if let Some(sym) = file_data.symbols.iter().find(|s| s.name == name) {
                return vec![Location { uri, range: sym.selection_range }];
            }
            let lines: Vec<String> = content.lines().map(String::from).collect();
            if let Some(range) = find_declaration_range_in_lines(&lines, name) {
                return vec![Location { uri, range }];
            }
        }
    }
    vec![]
}

/// Like `find_name_in_uri` but prefers declarations at or after `after_line`.
///
/// Used when we already know the qualifier class lives at `after_line` — we
/// want the parameter/field of THAT class, not a same-named field in a
/// different class that happens to appear earlier in the same file.
///
/// Strategy:
///   1. Symbol table — pick the symbol at or after `after_line` with the
///      smallest line number (closest match).  Fall back to any match if none
///      found after the hint line.
///   2. Line scan — search only lines >= `after_line`.
///   3. On-demand parse (same as `find_name_in_uri`).
pub(crate) fn find_name_in_uri_after_line(idx: &Indexer, name: &str, file_uri: &str, after_line: u32) -> Vec<Location> {
    let Ok(uri) = Url::parse(file_uri) else { return vec![]; };

    if let Some(f) = idx.files.get(file_uri) {
        // a) Symbol table: find the closest symbol at or after `after_line`.
        let best = f.symbols.iter()
            .filter(|s| s.name == name && s.selection_range.start.line >= after_line)
            .min_by_key(|s| s.selection_range.start.line);

        if let Some(sym) = best {
            return vec![Location { uri, range: sym.selection_range }];
        }

        // Fallback: any symbol with this name (different class, same file)
        if let Some(sym) = f.symbols.iter().find(|s| s.name == name) {
            return vec![Location { uri, range: sym.selection_range }];
        }

        // b) Line scan scoped to after_line first, then the whole file.
        if let Some(range) = find_declaration_range_after_line(&f.lines, name, after_line) {
            return vec![Location { uri, range }];
        }
        if let Some(range) = find_declaration_range_in_lines(&f.lines, name) {
            return vec![Location { uri, range }];
        }
        return vec![];
    }

    // c) On-demand parse
    find_name_in_uri(idx, name, file_uri)
}

/// Like `find_declaration_range_in_lines` but only searches from `start_line`.
fn find_declaration_range_after_line(lines: &[String], name: &str, start_line: u32) -> Option<tower_lsp::lsp_types::Range> {
    use tower_lsp::lsp_types::{Position, Range};
    let start = start_line as usize;
    if start >= lines.len() { return None; }
    find_declaration_range_in_lines(&lines[start..], name)
        .map(|r| Range {
            start: Position { line: r.start.line + start_line, character: r.start.character },
            end:   Position { line: r.end.line   + start_line, character: r.end.character   },
        })
}

///
/// Returns the location of `name:` in the current file.  This catches function
/// parameters that lack `val`/`var` and are therefore absent from the symbol index.
pub(crate) fn find_local_declaration(idx: &Indexer, name: &str, uri: &Url) -> Vec<Location> {
    let Some(data) = idx.files.get(uri.as_str()) else { return vec![]; };
    if let Some(range) = find_declaration_range_in_lines(&data.lines, name) {
        return vec![Location { uri: uri.clone(), range }];
    }
    vec![]
}
