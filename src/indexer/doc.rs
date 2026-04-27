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
    if decl_line == 0 { return None; }

    // Find the end of the doc comment block by scanning backward over
    // annotations, blank lines, and modifier-only lines.
    let mut search_end = decl_line;
    loop {
        if search_end == 0 { return None; }
        search_end -= 1;
        let trimmed = lines[search_end].trim();
        if trimmed.is_empty()
            || trimmed.starts_with('@')
            || is_modifier_line(trimmed)
        {
            if search_end == 0 { return None; }
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
            if t.starts_with("/**") || t.starts_with("/*") { break; }
            if start == 0 { return None; }
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
                let stripped = if t.starts_with("/// ") {
                    &t[4..]
                } else if t.starts_with("//! ") {
                    &t[4..]
                } else if t.starts_with("// ") {
                    &t[3..]
                } else if t.starts_with("//") {
                    &t[2..]
                } else {
                    t
                };
                stripped.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = format_doc_tags(&text);
        return if rendered.trim().is_empty() { None } else { Some(rendered) };
    }

    None
}

/// Returns `true` for lines that contain only Kotlin/Java modifiers/keywords
/// (e.g. `override`, `public final`) — we skip these when hunting for docs.
fn is_modifier_line(s: &str) -> bool {
    const MODIFIERS: &[&str] = &[
        "public", "private", "protected", "internal", "override", "open",
        "abstract", "sealed", "final", "static", "inline", "tailrec",
        "external", "suspend", "operator", "infix", "data", "inner",
        "companion", "lateinit", "const",
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
        let t = if let Some(rest) = t.strip_prefix('*') { rest } else { t };
        let t = t.trim();
        // Skip the lone opening/closing marker lines that become empty
        if !t.is_empty() {
            out.push(t.to_owned());
        }
    }
    let joined = out.join("\n");
    format_doc_tags(&joined)
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
    // Split on Javadoc/KDoc tag boundaries (lines starting with @).
    // We need to preserve multi-line tag bodies.
    let mut description: Vec<String> = Vec::new();
    let mut params:  Vec<(String, String)> = Vec::new();
    let mut returns: Option<String> = None;
    let mut throws:  Vec<(String, String)> = Vec::new();
    let mut see:     Vec<String> = Vec::new();
    let mut since:   Option<String> = None;

    // Accumulate current tag body across newlines.
    let mut cur_tag: Option<String>  = None;
    let mut cur_body: Vec<String>    = Vec::new();

    let flush = |cur_tag: &Option<String>, cur_body: &Vec<String>,
                  params: &mut Vec<(String, String)>,
                  returns: &mut Option<String>,
                  throws: &mut Vec<(String, String)>,
                  see: &mut Vec<String>,
                  since: &mut Option<String>| {
        let body = cur_body.join(" ").trim().to_owned();
        if let Some(tag) = cur_tag {
            match tag.as_str() {
                "param" | "property" => {
                    let (name, rest) = split_first_word(&body);
                    params.push((name.to_owned(), rest.trim().to_owned()));
                }
                "return" | "returns" => *returns = Some(body),
                "throws" | "exception" => {
                    let (name, rest) = split_first_word(&body);
                    throws.push((name.to_owned(), rest.trim().to_owned()));
                }
                "see"   => see.push(body),
                "since" => *since = Some(body),
                _ => {} // suppress, hide, internal, author, etc.
            }
        }
    };

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            // Flush previous tag
            flush(&cur_tag, &cur_body, &mut params, &mut returns,
                  &mut throws, &mut see, &mut since);
            cur_body.clear();

            let (tag, body) = split_first_word(rest);
            cur_tag = Some(tag.to_lowercase());
            if !body.is_empty() { cur_body.push(body.trim().to_owned()); }
        } else if cur_tag.is_some() {
            if !trimmed.is_empty() { cur_body.push(trimmed.to_owned()); }
        } else {
            description.push(trimmed.to_owned());
        }
    }
    flush(&cur_tag, &cur_body, &mut params, &mut returns,
          &mut throws, &mut see, &mut since);

    // Reassemble as Markdown.
    let mut md = description.join("\n").trim().to_owned();

    // Inline substitutions (KDoc links + Java {@code} / {@link})
    md = inline_doc_markup(&md);

    if !params.is_empty() {
        md.push_str("\n\n**Parameters**");
        for (name, desc) in &params {
            let desc = inline_doc_markup(desc);
            if desc.is_empty() {
                md.push_str(&format!("\n- `{name}`"));
            } else {
                md.push_str(&format!("\n- `{name}` — {desc}"));
            }
        }
    }
    if let Some(ret) = returns {
        md.push_str(&format!("\n\n**Returns** {}", inline_doc_markup(&ret)));
    }
    if !throws.is_empty() {
        md.push_str("\n\n**Throws**");
        for (ty, desc) in &throws {
            let desc = inline_doc_markup(desc);
            if desc.is_empty() {
                md.push_str(&format!("\n- `{ty}`"));
            } else {
                md.push_str(&format!("\n- `{ty}` — {desc}"));
            }
        }
    }
    if !see.is_empty() {
        let refs = see.iter().map(|s| format!("`{}`", s.trim())).collect::<Vec<_>>().join(", ");
        md.push_str(&format!("\n\n**See also:** {refs}"));
    }
    if let Some(s) = since {
        md.push_str(&format!("\n\n**Since:** {s}"));
    }

    md.trim().to_owned()
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
            let inner = inner.trim_start_matches("@code").trim_start_matches("@link").trim();
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
    let out = regex_replace_kdoc_links(&out);
    out
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
        None    => (s, ""),
    }
}

#[cfg(test)]
#[path = "doc_tests.rs"]
mod tests;
