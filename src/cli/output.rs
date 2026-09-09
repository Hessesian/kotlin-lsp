//! Result types and output formatting for CLI.

use serde::Serialize;
use tower_lsp::lsp_types::Location;

/// A single CLI result entry.
#[derive(Debug, Serialize)]
pub(crate) struct CliResult {
    pub file: String,
    pub line: u32,
    pub col: u32,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<ContextLine>,
}

/// One line of source text attached to a result by `--context`.
#[derive(Debug, Serialize)]
pub(crate) struct ContextLine {
    pub line: u32,
    pub text: String,
}

impl CliResult {
    pub(crate) fn from_location(loc: &Location, name: &str, kind: &str) -> Option<Self> {
        // Regular file:// URI — extract the local path.
        if let Ok(file) = loc.uri.to_file_path() {
            return Some(Self {
                file: file.to_string_lossy().into_owned(),
                line: loc.range.start.line + 1,
                col: loc.range.start.character + 1,
                kind: kind.to_owned(),
                name: name.to_owned(),
                context: Vec::new(),
            });
        }
        // jar:file:// URI — show the JAR path as a pseudo-location so library
        // symbols are visible in CLI output rather than silently dropped.
        let uri_str = loc.uri.as_str();
        if let Some(jar_path) = uri_str.strip_prefix("jar:file://") {
            return Some(Self {
                file: format!("jar:{jar_path}"),
                line: loc.range.start.line + 1,
                col: 1,
                kind: kind.to_owned(),
                name: name.to_owned(),
                context: Vec::new(),
            });
        }
        None
    }
}

/// Read `n` lines of source before and after each result's matched line
/// (inclusive of the matched line) and attach them as `context`.
///
/// Skipped for `jar:`-prefixed pseudo-paths, which aren't readable files.
/// `file_lines_cache` is caller-owned so it can be shared with other passes
/// (e.g. `refs --exclude-imports`) that also read matched files by line.
pub(crate) fn attach_context(
    results: &mut [CliResult],
    n: u32,
    file_lines_cache: &mut std::collections::HashMap<String, Vec<String>>,
) {
    for result in results.iter_mut() {
        if result.file.starts_with("jar:") {
            continue;
        }
        let lines = file_lines_cache
            .entry(result.file.clone())
            .or_insert_with(|| {
                std::fs::read_to_string(&result.file)
                    .map(|src| src.lines().map(String::from).collect())
                    .unwrap_or_default()
            });
        if lines.is_empty() {
            continue;
        }
        let start = result.line.saturating_sub(n).max(1);
        let end = result.line.saturating_add(n).min(lines.len() as u32);
        result.context = (start..=end)
            .filter_map(|line| {
                lines.get((line - 1) as usize).map(|text| ContextLine {
                    line,
                    text: text.clone(),
                })
            })
            .collect();
    }
}

pub(crate) fn print_results(results: &[CliResult], json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(results).unwrap_or_default()
        );
    } else {
        for r in results {
            if r.kind.is_empty() {
                println!("{}:{}:{}: {}", r.file, r.line, r.col, r.name);
            } else {
                println!("{}:{}:{}: {} {}", r.file, r.line, r.col, r.kind, r.name);
            }
            for ctx in &r.context {
                let marker = if ctx.line == r.line { ':' } else { ' ' };
                println!("    {:>5}{marker} {}", ctx.line, ctx.text);
            }
        }
    }
}
