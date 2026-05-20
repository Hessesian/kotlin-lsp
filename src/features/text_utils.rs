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

/// Kotlin hard keywords and soft keywords — never valid rename targets in `.kt` files.
///
/// **Must remain sorted** — `is_keyword_for_file` uses binary search.
pub(crate) const KOTLIN_KEYWORDS: &[&str] = &[
    "abstract",
    "actual",
    "annotation",
    "as",
    "break",
    "by",
    "catch",
    "class",
    "companion",
    "const",
    "constructor",
    "continue",
    "crossinline",
    "data",
    "delegate",
    "do",
    "dynamic",
    "else",
    "enum",
    "expect",
    "external",
    "false",
    "field",
    "file",
    "final",
    "finally",
    "for",
    "fun",
    "get",
    "if",
    "import",
    "in",
    "infix",
    "init",
    "inline",
    "inner",
    "interface",
    "internal",
    "is",
    "it",
    "lateinit",
    "noinline",
    "null",
    "object",
    "open",
    "operator",
    "out",
    "override",
    "package",
    "param",
    "private",
    "property",
    "protected",
    "public",
    "receiver",
    "reified",
    "return",
    "sealed",
    "set",
    "setparam",
    "super",
    "suspend",
    "tailrec",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "value",
    "var",
    "vararg",
    "when",
    "where",
    "while",
];

/// Java-only reserved words that are NOT Kotlin keywords (valid Kotlin identifiers).
/// Applied in addition to `KOTLIN_KEYWORDS` for `.java` files.
///
/// **Must remain sorted** — `is_keyword_for_file` uses binary search.
pub(crate) const JAVA_EXTRA_KEYWORDS: &[&str] = &[
    "assert",
    "boolean",
    "byte",
    "case",
    "char",
    "default",
    "double",
    "extends",
    "float",
    "goto",
    "implements",
    "instanceof",
    "int",
    "long",
    "native",
    "new",
    "short",
    "static",
    "strictfp",
    "switch",
    "synchronized",
    "throws",
    "transient",
    "void",
    "volatile",
];

/// Returns `true` when `name` is a reserved keyword for the given file path —
/// i.e. not a valid rename target. Java files check both `KOTLIN_KEYWORDS` and
/// `JAVA_EXTRA_KEYWORDS`; all other files check `KOTLIN_KEYWORDS` only.
pub(crate) fn is_keyword_for_file(name: &str, file_path: &str) -> bool {
    let in_kotlin = KOTLIN_KEYWORDS.binary_search(&name).is_ok();
    if in_kotlin {
        return true;
    }
    file_path.ends_with(".java") && JAVA_EXTRA_KEYWORDS.binary_search(&name).is_ok()
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
