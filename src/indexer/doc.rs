//! KDoc / Javadoc comment extraction and rendering.
//!
//! All functions here are pure string transformations — no `Indexer` dependency,
//! no I/O, no hidden state.  The single public entry-point is
//! [`extract_doc_comment`]; everything else is a private helper.

/// Extract the doc comment associated with the declaration at `decl_line`.
///
/// Walk backward from `decl_line` (exclusive) skipping blank lines, annotations
/// (`@…`), and visibility/modifier keywords, then collect either a `/** … */`
/// block-doc comment or a run of `//` line-doc comments immediately preceding
/// the declaration.
///
/// Returns cleaned Markdown text, or `None` when no doc comment is found.
///
/// Handles:
/// - Kotlin: `/** ... */` (KDoc) and `//` line comments above annotations
/// - Java:   `/** ... */` (Javadoc)
/// - Strips leading `*` and `/** ` / ` */` markers
/// - Converts `@param`, `@return`, `@throws` tags to bold Markdown headings
/// - Skips `@suppress`, `@hide`, `@internal` — not user-facing
/// - Strips `[LinkText](url)` Markdown links from KDoc `[Symbol]` references
pub(super) fn extract_doc_comment(lines: &[String], decl_line: usize) -> Option<String> {
    if decl_line == 0 {
        return None;
    }

    // Find the end of the doc comment block by scanning backward over
    // annotations, blank lines, and modifier-only lines.
    let mut search_end = decl_line;
    loop {
        if search_end == 0 {
            return None;
        }
        search_end -= 1;
        let trimmed = lines[search_end].trim();
        if trimmed.is_empty() || trimmed.starts_with('@') || is_modifier_line(trimmed) {
            if search_end == 0 {
                return None;
            }
            continue;
        }
        break;
    }

    let end_line = &lines[search_end];
    let end_trim = end_line.trim();

    // ── Block doc comment `/** ... */` ───────────────────────────────────────
    if end_trim.ends_with("*/") {
        // Find the opening `/**`
        let mut start = search_end;
        loop {
            let t = lines[start].trim();
            if t.starts_with("/**") || t.starts_with("/*") {
                break;
            }
            if start == 0 {
                return None;
            }
            start -= 1;
        }

        let raw_lines: Vec<&str> = lines[start..=search_end]
            .iter()
            .map(|l| l.as_str())
            .collect();
        return Some(render_block_doc(&raw_lines));
    }

    // ── Line doc comments `// …` ──────────────────────────────────────────────
    if end_trim.starts_with("//") {
        let mut start = search_end;
        while start > 0 && lines[start - 1].trim().starts_with("//") {
            start -= 1;
        }
        let text = lines[start..=search_end]
            .iter()
            .map(|l| {
                let t = l.trim();
                let stripped = t
                    .strip_prefix("/// ")
                    .or_else(|| t.strip_prefix("//! "))
                    .or_else(|| t.strip_prefix("// "))
                    .or_else(|| t.strip_prefix("//"))
                    .unwrap_or(t);
                stripped.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = format_doc_tags(&text);
        return if rendered.trim().is_empty() {
            None
        } else {
            Some(rendered)
        };
    }

    None
}

/// Returns `true` for lines that contain only Kotlin/Java modifiers/keywords
/// (e.g. `override`, `public final`) — we skip these when hunting for docs.
fn is_modifier_line(s: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "public",
        "private",
        "protected",
        "internal",
        "override",
        "open",
        "abstract",
        "sealed",
        "final",
        "static",
        "inline",
        "tailrec",
        "external",
        "suspend",
        "operator",
        "infix",
        "data",
        "inner",
        "companion",
        "lateinit",
        "const",
    ];
    s.split_whitespace().all(|w| MODIFIERS.contains(&w))
}

/// Strip `/** … */` markers and leading `*` from each line, then format tags.
fn render_block_doc(raw_lines: &[&str]) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in raw_lines {
        let t = line.trim();
        let t = t.strip_prefix("/**").unwrap_or(t);
        let t = t.strip_suffix("*/").unwrap_or(t);
        let t = t.strip_prefix("/*").unwrap_or(t);
        let t = if let Some(rest) = t.strip_prefix('*') {
            rest
        } else {
            t
        };
        let t = t.trim();
        // Skip the lone opening/closing marker lines that become empty
        if !t.is_empty() {
            out.push(t.to_owned());
        }
    }
    let joined = out.join("\n");
    format_doc_tags(&joined)
}

#[derive(Default)]
struct ParsedDocTags {
    description: Vec<String>,
    params: Vec<(String, String)>,
    returns: Option<String>,
    throws: Vec<(String, String)>,
    see: Vec<String>,
    since: Option<String>,
}

/// Convert KDoc/Javadoc tags to readable Markdown.
///
/// - `@param name desc`   → `**Parameters**\n- \`name\` desc`
/// - `@return desc`       → `**Returns**\n desc`
/// - `@throws T desc`     → `**Throws**\n- \`T\` desc`
/// - `@see ref`           → `**See also:** ref`
/// - `@since ver`         → `**Since:** ver`
/// - `[Symbol]` (KDoc)    → `` `Symbol` ``
/// - `{@code …}` (Java)   → `` `…` ``
/// - `{@link T}` (Java)   → `` `T` ``
/// - Suppressed: `@suppress`, `@hide`, `@internal`
fn format_doc_tags(text: &str) -> String {
    render_doc_markdown(&parse_doc_tags(text)).trim().to_owned()
}

fn parse_doc_tags(text: &str) -> ParsedDocTags {
    let mut parsed = ParsedDocTags::default();
    let mut current_tag: Option<String> = None;
    let mut current_body: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            flush_doc_tag(current_tag.as_deref(), &current_body, &mut parsed);
            current_body.clear();

            let (tag, body) = split_first_word(rest);
            current_tag = Some(tag.to_lowercase());
            if !body.is_empty() {
                current_body.push(body.trim().to_owned());
            }
        } else if current_tag.is_some() {
            if !trimmed.is_empty() {
                current_body.push(trimmed.to_owned());
            }
        } else {
            parsed.description.push(trimmed.to_owned());
        }
    }

    flush_doc_tag(current_tag.as_deref(), &current_body, &mut parsed);
    parsed
}

fn flush_doc_tag(current_tag: Option<&str>, current_body: &[String], parsed: &mut ParsedDocTags) {
    let body = current_body.join(" ").trim().to_owned();
    if let Some(tag) = current_tag {
        match tag {
            "param" | "property" => {
                let (name, rest) = split_first_word(&body);
                parsed
                    .params
                    .push((name.to_owned(), rest.trim().to_owned()));
            }
            "return" | "returns" => parsed.returns = Some(body),
            "throws" | "exception" => {
                let (type_name, rest) = split_first_word(&body);
                parsed
                    .throws
                    .push((type_name.to_owned(), rest.trim().to_owned()));
            }
            "see" => parsed.see.push(body),
            "since" => parsed.since = Some(body),
            _ => {}
        }
    }
}

fn render_doc_markdown(parsed: &ParsedDocTags) -> String {
    let description = parsed.description.join("\n");
    let mut markdown = inline_doc_markup(description.trim());

    append_markdown_section(&mut markdown, format_param_section(&parsed.params));
    append_markdown_section(
        &mut markdown,
        parsed.returns.as_deref().map(format_return_tag),
    );
    append_markdown_section(&mut markdown, format_throws_section(&parsed.throws));
    append_markdown_section(&mut markdown, format_see_section(&parsed.see));
    append_markdown_section(&mut markdown, parsed.since.as_deref().map(format_since_tag));

    markdown
}

fn append_markdown_section(markdown: &mut String, section: Option<String>) {
    if let Some(section) = section {
        markdown.push_str("\n\n");
        markdown.push_str(&section);
    }
}

fn format_param_section(params: &[(String, String)]) -> Option<String> {
    if params.is_empty() {
        return None;
    }

    let mut section = String::from("**Parameters**");
    for (name, body) in params {
        section.push('\n');
        section.push_str(&format_param_tag(name, body));
    }
    Some(section)
}

fn format_param_tag(name: &str, body: &str) -> String {
    let description = inline_doc_markup(body);
    if description.is_empty() {
        format!("- `{name}`")
    } else {
        format!("- `{name}` — {description}")
    }
}

fn format_return_tag(body: &str) -> String {
    format!("**Returns** {}", inline_doc_markup(body))
}

fn format_throws_section(throws: &[(String, String)]) -> Option<String> {
    if throws.is_empty() {
        return None;
    }

    let mut section = String::from("**Throws**");
    for (type_name, body) in throws {
        section.push('\n');
        section.push_str(&format_throws_tag(type_name, body));
    }
    Some(section)
}

fn format_throws_tag(type_name: &str, body: &str) -> String {
    let description = inline_doc_markup(body);
    if description.is_empty() {
        format!("- `{type_name}`")
    } else {
        format!("- `{type_name}` — {description}")
    }
}

fn format_see_section(see: &[String]) -> Option<String> {
    if see.is_empty() {
        return None;
    }

    let refs = see
        .iter()
        .map(|value| format!("`{}`", value.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("**See also:** {refs}"))
}

fn format_since_tag(body: &str) -> String {
    format!("**Since:** {body}")
}

/// Apply inline markup substitutions.
fn inline_doc_markup(s: &str) -> String {
    // `{@code expr}` and `{@link Type}` → `expr` / `Type`
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('{') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if let Some(end) = rest.find('}') {
            let inner = &rest[1..end]; // strip braces
            let inner = inner
                .trim_start_matches("@code")
                .trim_start_matches("@link")
                .trim();
            out.push('`');
            out.push_str(inner);
            out.push('`');
            rest = &rest[end + 1..];
        } else {
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);

    // KDoc `[Symbol]` → `Symbol`
    // Avoid matching Markdown links `[text](url)` — only bare `[Word]`

    regex_replace_kdoc_links(&out)
}

/// Replace KDoc `[SymbolName]` (not followed by `(`) with `` `SymbolName` ``.
fn regex_replace_kdoc_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            // Find the closing `]`
            if let Some(rel) = bytes[i + 1..].iter().position(|&b| b == b']') {
                let end = i + 1 + rel;
                let inner = &s[i + 1..end];
                // Only treat as KDoc link if inner has no spaces (symbol name)
                // and is NOT followed by `(` (which would be a Markdown link)
                let next = bytes.get(end + 1).copied();
                if !inner.contains(' ') && next != Some(b'(') {
                    out.push('`');
                    out.push_str(inner);
                    out.push('`');
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Split `"word rest of string"` → `("word", "rest of string")`.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

#[cfg(test)]
#[path = "doc_tests.rs"]
mod tests;
