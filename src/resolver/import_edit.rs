//! Auto-import helpers — edit generation and import presence checks.

use tower_lsp::lsp_types::{Range, TextEdit};

use crate::types::ImportEntry;
use crate::LinesExt;

/// True if `fqn` is already usable in the file without an additional import:
/// - exact non-alias import: `import pkg.Name` where local_name == last segment
/// - star import covering the package: `import pkg.*`
pub(crate) fn already_imported(fqn: &str, imports: &[ImportEntry]) -> bool {
    let last_seg = fqn.rsplit('.').next().unwrap_or(fqn);
    let pkg = match fqn.rfind('.') {
        Some(i) => &fqn[..i],
        None => "",
    };
    imports.iter().any(|imp| {
        if imp.is_star {
            imp.full_path == pkg
        } else {
            // Only count as "already imported" when not aliased to a different name.
            imp.full_path == fqn && imp.local_name == last_seg
        }
    })
}

/// Find the line number after which to insert a new import statement.
/// Returns the 0-based line index of the *new* import line (the line we'll insert before).
/// Priority: after the last existing `import` line; else after the `package` line; else line 0.
pub(crate) fn import_insertion_line(lines: &[String]) -> u32 {
    // Find the last import line.
    let last_import = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.trim_start().starts_with("import "))
        .map(|(i, _)| i);
    if let Some(i) = last_import {
        return (i + 1) as u32;
    }
    // No imports — insert after package declaration (with a blank line gap).
    let pkg_line = lines
        .iter()
        .enumerate()
        .find(|(_, l)| l.trim_start().starts_with("package "))
        .map(|(i, _)| i);
    if let Some(i) = pkg_line {
        return (i + 1) as u32;
    }
    0
}

/// Build a TextEdit that inserts `import {fqn}\n` at the correct position.
pub(crate) fn make_import_edit(fqn: &str, lines: &[String], needs_semicolon: bool) -> TextEdit {
    let line = lines.import_insertion_line();
    // When inserting right after the package line (no existing imports), add a blank line.
    let needs_blank = line > 0
        && lines
            .get((line - 1) as usize)
            .map(|l| l.trim_start().starts_with("package "))
            .unwrap_or(false)
        && lines
            .get(line as usize)
            .map(|l| !l.trim().is_empty())
            .unwrap_or(false);
    let stmt = if needs_semicolon {
        format!("import {fqn};")
    } else {
        format!("import {fqn}")
    };
    let new_text = if needs_blank {
        format!("\n{stmt}\n")
    } else {
        format!("{stmt}\n")
    };
    TextEdit {
        range: Range {
            start: tower_lsp::lsp_types::Position { line, character: 0 },
            end: tower_lsp::lsp_types::Position { line, character: 0 },
        },
        new_text,
    }
}
