//! Shared text-processing utilities for feature modules.

/// Iterates over the byte offsets in `line` where `word` appears as a whole
/// word (not as a substring of a longer identifier).
pub(crate) fn word_byte_offsets<'a>(
    line: &'a str,
    word: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    let word_len = word.len();
    let is_id = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = 0;
    std::iter::from_fn(move || {
        while let Some(rel) = line[search_from..].find(word) {
            let pos = search_from + rel;
            search_from = pos + word_len;
            let before_ok = pos == 0 || !is_id(line[..pos].chars().next_back()?);
            let after_ok =
                pos + word_len >= line.len() || !is_id(line[pos + word_len..].chars().next()?);
            if before_ok && after_ok {
                return Some(pos);
            }
        }
        None
    })
}

/// Counts UTF-16 code units in `text` (for LSP column offsets).
pub(crate) fn utf16_column(text: &str) -> u32 {
    text.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Returns `true` when `name` is a block-opening keyword that is NOT callable —
/// i.e. signature help and rename should skip it.
pub(crate) fn is_non_call_keyword(name: &str) -> bool {
    matches!(
        name,
        "fun"
            | "val"
            | "var"
            | "if"
            | "while"
            | "for"
            | "when"
            | "catch"
            | "constructor"
            | "override"
            | "else"
            | "return"
            | "throw"
            | "try"
            | "finally"
            | "object"
            | "class"
            | "interface"
            | "enum"
            | "init"
            | "data"
            | "sealed"
            | "open"
            | "abstract"
            | "private"
            | "public"
            | "protected"
            | "internal"
            | "companion"
            | "suspend"
            | "inline"
            | "const"
            | "lateinit"
            | "typealias"
            | "import"
            | "package"
            | "this"
            | "super"
            | "null"
            | "true"
            | "false"
            | "is"
            | "as"
            | "in"
            | "by"
            | "get"
            | "set"
            | "it"
    )
}

/// Replace all whole-word occurrences of `word` with `replacement` across
/// `lines`, joining them back into a single string with `\n`.
///
/// Skips `import` and `package` lines unchanged (preserves qualified names).
/// Uses char-by-char scanning — no regex dependency.
pub(crate) fn whole_word_replace_file(lines: &[String], word: &str, replacement: &str) -> String {
    if word.is_empty() {
        return lines.join("\n");
    }

    let wchars: Vec<char> = word.chars().collect();
    let wlen = wchars.len();
    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
            result.push_str(line);
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut j = 0usize;
        while j < chars.len() {
            if chars[j..].starts_with(&wchars) {
                let before_ok = j == 0 || !(chars[j - 1].is_alphanumeric() || chars[j - 1] == '_');
                let end = j + wlen;
                let after_ok =
                    end >= chars.len() || !(chars[end].is_alphanumeric() || chars[end] == '_');
                if before_ok && after_ok {
                    result.push_str(replacement);
                    j = end;
                    continue;
                }
            }
            result.push(chars[j]);
            j += 1;
        }
    }
    result
}
