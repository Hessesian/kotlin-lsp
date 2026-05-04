use std::sync::Arc;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, InsertTextFormat, SymbolKind, Url,
};

use crate::indexer::Indexer;
use crate::types::Visibility;
use crate::LinesExt;
use crate::StrExt;
use crate::parser::parse_by_extension;
use crate::stdlib::bare_completions;
use crate::stdlib_tail::dot_completions_for_lang;

use super::{fqns_for_name, already_imported,
            resolve_symbol_inner, resolve_symbol_no_rg};
use super::infer::{ReceiverKind, ReceiverType, infer_receiver_type};

// ─── match scoring ────────────────────────────────────────────────────────────

/// Returns true if `name` is SCREAMING_SNAKE_CASE (all letters are uppercase).
/// Used to suppress constants/enum variants when the user types a CamelCase prefix.
pub(crate) fn is_screaming_snake(name: &str) -> bool {
    name.chars().any(|c| c.is_alphabetic()) && name.chars().all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
}

/// Score how well `name` matches `prefix`. Lower = better.
///
/// - `0` — `name` starts with `prefix` (case-insensitive, fastest/best)
/// - `1` — camelCase acronym: every character in `prefix` (uppercase-as-given)
///   matches the first letter of successive CamelCase/underscore word
///   segments in `name` (e.g. `CB` → `ColumnButton`, `mSF` → `myStateFlow`)
/// - `2` — `name` contains `prefix` as a case-insensitive substring
/// - `None` — no match; exclude this symbol
pub(crate) fn match_score(name: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() { return Some(0); }
    let name_lower  = name.to_lowercase();
    let prefix_lower = prefix.to_lowercase();
    if name_lower.starts_with(&prefix_lower)        { return Some(0); }
    if camel_acronym_match(name, prefix)            { return Some(1); }
    if name_lower.contains(&prefix_lower)           { return Some(2); }
    None
}

/// True if every character in `prefix` matches the first character of a successive
/// CamelCase or underscore-delimited word in `name`.
///
/// Matching is case-insensitive: both `prefix` and the collected word starts are
/// compared in lowercase.
///
/// Examples:
///   `CB`  vs `ColumnButton`    → true  (C=Column, B=Button)
///   `mSF` vs `myStateFlow`     → true  (m=my, S=State, F=Flow)
///   `CB`  vs `CoolBar`         → false (C=C ok, B must start next word; 'oolBar' has no word-start at 'B')
///   `CB`  vs `coolBar`         → true  (case-insensitive: c=cool, b=Bar)
fn camel_acronym_match(name: &str, prefix: &str) -> bool {
    // Collect the first character of each CamelCase / underscore segment.
    let mut word_starts: Vec<char> = Vec::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let is_word_start = i == 0
            || c == '_'
            || (i > 0 && chars[i - 1] == '_')          // char immediately after underscore
            || (c.is_uppercase() && i > 0 && chars[i - 1].is_lowercase())
            || (c.is_uppercase() && i > 0 && chars[i - 1].is_uppercase()
                && i + 1 < chars.len() && chars[i + 1].is_lowercase());
        if is_word_start && c != '_' {
            word_starts.push(c.to_lowercase().next().unwrap_or(c));
        }
    }

    // Every prefix char must match successive word starts (in order, not necessarily consecutive).
    let prefix_chars: Vec<char> = prefix.to_lowercase().chars().collect();
    let mut wi = 0;
    for &pc in &prefix_chars {
        loop {
            if wi >= word_starts.len() { return false; }
            if word_starts[wi] == pc   { wi += 1; break; }
            wi += 1;
        }
    }
    true
}

// ─── completion entry point ───────────────────────────────────────────────────

/// Maximum completion items returned per response.
/// When capped, `is_incomplete` should be set so the client re-queries.
pub(crate) const COMPLETION_CAP: usize = 150;

/// Prefix length at which local-symbol relevance score is reduced (longer prefix → more confident match).
const MIN_PREFIX_SCORE_REDUCTION: usize = 4;

/// Minimum prefix length to enable case-insensitive completion matching.
const MIN_CASE_INSENSITIVE_PREFIX: usize = 2;

/// Provide completion candidates for `prefix` at the current position.
///
/// Returns `(items, hit_cap)` — when `hit_cap` is true the caller should set
/// `CompletionList.is_incomplete = true` so the client re-requests completions
/// as the user types more characters.
///
/// Two modes:
/// - **Dot-completion** (`dot_receiver = Some("obj")`): infer the receiver's type
///   and return all its members (symbols + line-scanned constructor params).
/// - **Bare-word** (`dot_receiver = None`): return all symbols from the current
///   file, same-package files, and the whole project index whose name starts with
///   `prefix` (case-insensitive).
pub fn complete_symbol(
    idx: &Indexer,
    prefix: &str,
    dot_receiver: Option<&str>,
    from_uri: &Url,
    snippets: bool,
) -> (Vec<CompletionItem>, bool) {
    complete_symbol_with_context(idx, prefix, dot_receiver, from_uri, snippets, false)
}

/// Like `complete_symbol` but with explicit annotation context flag.
/// Called from `indexer::completions` after detecting a `@` trigger.
pub fn complete_symbol_with_context(
    idx: &Indexer,
    prefix: &str,
    dot_receiver: Option<&str>,
    from_uri: &Url,
    snippets: bool,
    annotation_only: bool,
) -> (Vec<CompletionItem>, bool) {
    if let Some(receiver) = dot_receiver {
        return (complete_dot(idx, receiver, from_uri, snippets), false);
    }
    complete_bare(idx, prefix, from_uri, snippets, annotation_only)
}

/// Detect whether the character immediately before `prefix` in `line` is `@`.
/// Used to restrict completions to annotation/class kinds only.
pub(crate) fn is_annotation_context(line: &str, prefix: &str) -> bool {
    line.strip_suffix(prefix)
        .map(|before| before.ends_with('@'))
        .unwrap_or(false)
}

/// Completion for `super.` — gather all members from the parent hierarchy.
/// Scan the index for extension functions whose `extension_receiver` matches `receiver_type`
/// and return them as `CompletionItem`s with auto-import `additionalTextEdits` when needed.
///
/// Only called for Kotlin files; Java files don't consume Kotlin extension functions.
fn extension_fn_completions(
    idx:           &Indexer,
    receiver_type: &str,
    from_uri:      &Url,
    snippets:      bool,
) -> Vec<CompletionItem> {
    if receiver_type.is_empty() { return vec![]; }

    // Gather current imports + package once (to avoid repeating per symbol).
    let is_java = false;
    let live = idx.live_lines.get(from_uri.as_str()).map(|ll| ll.clone());
    let (cur_imports, cur_pkg, cur_lines) = idx.files.get(from_uri.as_str())
        .map(|f| {
            let lines = live.clone().unwrap_or_else(|| f.lines.clone());
            let imports = if live.is_some() { lines.parse_imports() } else { f.imports.clone() };
            (imports, f.package.clone().unwrap_or_default(), lines)
        })
        .unwrap_or_else(|| {
            let lines = live.clone().unwrap_or_default();
            let imports = lines.parse_imports();
            (imports, String::new(), lines)
        });

    let mut items = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for file_entry in idx.files.iter() {
        let file_uri_str = file_entry.key().clone();
        let file = file_entry.value();
        // Skip the current file — its top-level extensions are already in scope.
        if file_uri_str == from_uri.as_str() { continue; }
        // Only look at Kotlin files.
        if !file_uri_str.ends_with(".kt") && !file_uri_str.ends_with(".kts") { continue; }

        for sym in &file.symbols {
            if sym.extension_receiver != receiver_type { continue; }
            // Skip private/protected — not accessible from other files.
            if matches!(sym.visibility, Visibility::Private | Visibility::Protected) { continue; }

            let dedup_key = format!("{}:{}", sym.name, file_uri_str);
            if !seen.insert(dedup_key) { continue; }

            // Build FQN for auto-import: package.funcName
            let pkg = file.package.as_deref().unwrap_or("");
            let fqn = if pkg.is_empty() { sym.name.clone() } else { format!("{pkg}.{}", sym.name) };

            let pkg_of_fqn = match fqn.rfind('.') { Some(i) => &fqn[..i], None => "" };
            let needs_import = !already_imported(&fqn, &cur_imports)
                && !cur_imports.iter().any(|imp| imp.is_star && imp.full_path == pkg_of_fqn)
                && pkg_of_fqn != cur_pkg;

            let import_edit = if needs_import {
                Some(vec![cur_lines.make_import_edit(&fqn, is_java)])
            } else {
                None
            };

            let insert_text = if snippets { Some(format!("{}($1)", sym.name)) } else { None };
            let detail = if !sym.detail.is_empty() { Some(sym.detail.clone()) }
                         else if needs_import { Some(pkg_of_fqn.to_string()) }
                         else { None };

            items.push(CompletionItem {
                label:                sym.name.clone(),
                kind:                 Some(CompletionItemKind::FUNCTION),
                insert_text,
                insert_text_format:   if snippets { Some(InsertTextFormat::SNIPPET) } else { None },
                sort_text:            Some(format!("01:ext:{}", sym.name)),
                detail,
                command:              if snippets { Some(trigger_parameter_hints()) } else { None },
                additional_text_edits: import_edit,
                ..Default::default()
            });
        }
    }

    items
}

fn complete_super(idx: &Indexer, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
    if idx.files.get(from_uri.as_str()).is_none() { return vec![]; }
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut visited: Vec<String> = vec![from_uri.as_str().to_owned()];
    collect_hierarchy_completions(idx, from_uri, &mut visited, 0, &mut items, snippets);
    // Filter out private members — inaccessible even via super.
    items.retain(|i| i.sort_text.as_deref().map(|s| !s.starts_with("prv:")).unwrap_or(true));
    items.sort_by_key(|i| (kind_sort_rank(i.kind), i.label.clone()));
    items.dedup_by_key(|i| i.label.clone());
    items
}

fn collect_hierarchy_completions(
    idx: &Indexer,
    from_uri: &Url,
    visited: &mut Vec<String>,
    depth: u8,
    out: &mut Vec<CompletionItem>,
    snippets: bool,
) {
    const MAX_DEPTH: u8 = 4;
    if depth >= MAX_DEPTH { return; }

    let supers: Vec<String> = match idx.files.get(from_uri.as_str()) {
        Some(f) => f.supers.iter().map(|(_, n, _)| n.clone()).collect(),
        None => return,
    };

    for super_name in supers {
        let super_locs = resolve_symbol_inner(idx, &super_name, from_uri, false);
        for super_loc in &super_locs {
            let uri_str = super_loc.uri.as_str();
            if visited.contains(&uri_str.to_owned()) { continue; }
            visited.push(uri_str.to_owned());
            let mut new_items = symbols_from_uri_as_completions(idx, uri_str);
            if !snippets {
                for item in &mut new_items { item.insert_text = None; item.insert_text_format = None; }
            }
            out.extend(new_items);
            collect_hierarchy_completions(idx, &super_loc.uri, visited, depth + 1, out, snippets);
        }
    }
}

/// Dot-completion: return all members of the receiver's inferred type,
/// sorted: methods first, then fields/vars, then class-level names last.
pub(crate) fn complete_dot(idx: &Indexer, receiver: &str, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
    // `super.` — collect all members from the parent class hierarchy.
    if receiver == "super" {
        return complete_super(idx, from_uri, snippets);
    }

    // Infer and normalise the receiver's type.
    let rt = match infer_receiver_type(idx, ReceiverKind::Variable(receiver), from_uri) {
        Some(r) => r,
        None => {
            // Could be an uppercase class/object — look it up directly.
            if receiver.starts_with_uppercase() {
                ReceiverType::from_raw(receiver.to_string())
            } else {
                return vec![];
            }
        }
    };

    // Resolve the outer type to its source file (no rg — per-keystroke path).
    let file_uri = match resolve_symbol_no_rg(idx, &rt.outer, from_uri).first() {
        Some(loc) => loc.uri.to_string(),
        None      => return vec![],
    };

    // For dotted types (e.g. `Outer.Inner`), show only the inner type's members.
    // `rt.leaf` is the last segment, so it works for both plain and dotted types.
    let mut items = symbols_from_nested_type(idx, &file_uri, &rt.leaf, Some(from_uri.as_str()));

    // Filter out private members — they are inaccessible from outside the class.
    items.retain(|i| i.sort_text.as_deref().map(|s| !s.starts_with("prv:")).unwrap_or(true));

    // Walk the inheritance hierarchy to include members from parent classes/interfaces.
    // Tracks "file_uri#TypeName" to prevent cycles; seed with the direct type.
    let mut visited = vec![format!("{}#{}", file_uri, rt.leaf)];
    collect_inherited_members(idx, &file_uri, &rt.leaf, from_uri, &mut visited, 0, &mut items, snippets);

    // Deduplicate by label: direct members win over inherited ones (they come first).
    let mut seen_labels = std::collections::HashSet::new();
    items.retain(|i| seen_labels.insert(i.label.clone()));

    // Strip snippet fields if client doesn't support them.
    if !snippets {
        for item in &mut items {
            item.insert_text        = None;
            item.insert_text_format = None;
        }
    }

    // Sort: functions/methods first, then fields/vars, then everything else.
    items.sort_by_key(|i| kind_sort_rank(i.kind));

    // Append stdlib extensions filtered to the receiver type. Only add Kotlin stdlib
    // when the current file is a Kotlin file; add Swift-specific snippets for Swift.
    let from_path = from_uri.path();
    items.extend(dot_completions_for_lang(from_path, &rt.qualified, snippets));

    // Append indexed extension functions whose receiver matches `rt.outer`.
    // These may be defined in other files and need an auto-import.
    if from_path.ends_with(".kt") || from_path.ends_with(".kts") {
        items.extend(extension_fn_completions(idx, &rt.outer, from_uri, snippets));
    }

    items
}

/// Recursively collect members from parent classes/interfaces of `type_name`
/// (defined in `file_uri`) and append them to `out`.
///
/// `visited` tracks `"file_uri#TypeName"` pairs to prevent infinite cycles.
/// Only non-private members are added (matching `complete_dot` behaviour).
fn collect_inherited_members(
    idx:         &Indexer,
    file_uri:    &str,
    type_name:   &str,
    calling_uri: &Url,
    visited:     &mut Vec<String>,
    depth:       u8,
    out:         &mut Vec<CompletionItem>,
    snippets:    bool,
) {
    const MAX_DEPTH: u8 = 4;
    if depth >= MAX_DEPTH { return; }

    // Find the class's declaration line, then fetch its supertype names.
    let supers: Vec<String> = {
        let data = match idx.files.get(file_uri) {
            Some(d) => d,
            None    => return,
        };
        let class_line = data.symbols.iter()
            .find(|s| s.name == type_name)
            .map(|s| s.selection_range.start.line);
        match class_line {
            Some(line) => data.supers.iter()
                .filter(|(l, _, _)| *l == line)
                .map(|(_, n, _)| n.clone())
                .collect(),
            // Fallback if type not found: walk all supers in file.
            None => data.supers.iter().map(|(_, n, _)| n.clone()).collect(),
        }
    };

    let type_url = match Url::parse(file_uri) { Ok(u) => u, Err(_) => return };

    for super_name in supers {
        let super_locs = resolve_symbol_inner(idx, &super_name, &type_url, false);
        for loc in &super_locs {
            let key = format!("{}#{}", loc.uri.as_str(), super_name);
            if visited.contains(&key) { continue; }
            visited.push(key);

            let mut inherited = symbols_from_nested_type(
                idx, loc.uri.as_str(), &super_name, Some(calling_uri.as_str()),
            );
            inherited.retain(|i| i.sort_text.as_deref().map(|s| !s.starts_with("prv:")).unwrap_or(true));
            if !snippets {
                for item in &mut inherited { item.insert_text = None; item.insert_text_format = None; }
            }
            out.extend(inherited);

            collect_inherited_members(idx, loc.uri.as_str(), &super_name, calling_uri, visited, depth + 1, out, snippets);
        }
    }
}

/// Build a `CompletionItem` for a symbol found inside a nested type body.
///
/// Functions/methods get a snippet `name($1)`; all other kinds are plain-text.
/// The `sort_text` prefix is the `kind_sort_rank` value so the list is ordered
/// consistently with the rest of the completion results.
fn completion_item_for_nested_symbol(
    idx:         &Indexer,
    s:           &crate::types::SymbolEntry,
    uri_str:     &str,
    calling_uri: Option<&str>,
) -> CompletionItem {
    let kind  = symbol_kind_to_completion(s.kind);
    let is_fn = matches!(kind, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
    // Apply generic type param substitution when the symbol is from a different file.
    let detail_raw = if s.detail.is_empty() { None } else { Some(s.detail.clone()) };
    let detail = detail_raw.map(|d| {
        if let Some(cu) = calling_uri {
            idx.type_subst_sig(uri_str, s.selection_range.start.line, cu, &d)
        } else {
            d
        }
    });
    let mut data = serde_json::json!({"u": uri_str, "l": s.selection_range.start.line, "c": s.selection_range.start.character});
    if let Some(cu) = calling_uri { data["cu"] = serde_json::Value::String(cu.to_owned()); }
    CompletionItem {
        label:              s.name.clone(),
        kind:               Some(kind),
        insert_text:        if is_fn { Some(format!("{}($1)", s.name)) } else { None },
        insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
        sort_text:          Some(format!("{:02}:{}", kind_sort_rank(Some(kind)), s.name)),
        detail,
        command:            if is_fn { Some(trigger_parameter_hints()) } else { None },
        data:               Some(data),
        ..Default::default()
    }
}

/// Return completions for symbols declared INSIDE `type_name` within the given file.
/// Uses the symbol's range end (the closing `}` of the class body) to determine
/// membership — no indentation heuristics needed.
fn symbols_from_nested_type(
    idx:         &Indexer,
    file_uri:    &str,
    inner_name:  &str,
    calling_uri: Option<&str>,
) -> Vec<CompletionItem> {
    // Try in-memory index first; fall back to on-demand disk parse.
    let owned: crate::types::FileData;
    let symbols_ref: &[crate::types::SymbolEntry] = if let Some(d) = idx.files.get(file_uri) {
        // Clone symbols out of the DashMap guard so we can drop the guard.
        owned = d.value().as_ref().clone();
        &owned.symbols
    } else {
        // File not yet indexed — parse it on demand.
        let url = match Url::parse(file_uri) { Ok(u) => u, Err(_) => return vec![] };
        let path = match url.to_file_path() { Ok(p) => p, Err(_) => return vec![] };
        let content = match std::fs::read_to_string(&path) { Ok(c) => c, Err(_) => return vec![] };
        owned = parse_by_extension(file_uri, &content);
        &owned.symbols
    };

    // Find the type's declaration to get its body span.
    let type_sym = match symbols_ref.iter().find(|s| s.name == inner_name) {
        Some(s) => s,
        None    => {
            // Unknown type — return all non-private symbols as a fallback.
            return symbols_ref.iter()
                .filter(|s| s.visibility != Visibility::Private)
                .map(|s| completion_item_for_nested_symbol(idx, s, file_uri, calling_uri))
                .collect();
        }
    };

    let type_start = type_sym.range.start;
    let type_end   = type_sym.range.end;

    // Collect symbols whose start position falls within the type's body span.
    // Compare both line and character so one-line declarations like
    // `class Foo { fun bar() {} }` still include same-line members.
    symbols_ref.iter()
        .filter(|s| {
            let start = s.range.start;
            let starts_after = start.line > type_start.line
                || (start.line == type_start.line && start.character > type_start.character);
            let starts_before = start.line < type_end.line
                || (start.line == type_end.line && start.character <= type_end.character);
            starts_after && starts_before
        })
        .filter(|s| s.visibility != Visibility::Private)
        .map(|s| completion_item_for_nested_symbol(idx, s, file_uri, calling_uri))
        .collect()
}

/// Sort rank for completion item kinds: lower = appears earlier.
fn kind_sort_rank(kind: Option<CompletionItemKind>) -> u8 {
    match kind {
        Some(CompletionItemKind::FUNCTION) | Some(CompletionItemKind::METHOD) => 0,
        Some(CompletionItemKind::FIELD)    | Some(CompletionItemKind::VARIABLE)
        | Some(CompletionItemKind::CONSTANT) | Some(CompletionItemKind::ENUM_MEMBER) => 1,
        Some(CompletionItemKind::CLASS)    | Some(CompletionItemKind::INTERFACE)
        | Some(CompletionItemKind::ENUM)   | Some(CompletionItemKind::MODULE) => 3,
        _ => 2,
    }
}

/// Returns the `sort_text` visibility prefix.
/// Private symbols get the `"prv:"` tag so `complete_dot` can filter them out.
fn vis_tag(vis: Visibility) -> &'static str {
    match vis {
        Visibility::Private   => "prv:",
        Visibility::Protected => "prt:",
        _                     => "",
    }
}

/// Bare-word completion: match-scored across local file + same-package + index.
///
/// Case heuristic:
/// - **Lowercase prefix** → only return symbols whose name starts with a
///   lowercase letter (local vars, params, fields, fun names).  Class names are
///   excluded because they are rarely what the user wants when typing `acc…`.
/// - **Uppercase prefix or empty** → return everything (class names + members).
///
/// Returns `(items, hit_cap)` — callers should propagate `hit_cap` to
/// `CompletionList.is_incomplete` so the client re-queries each keystroke.
pub(crate) fn complete_bare(idx: &Indexer, prefix: &str, from_uri: &Url, snippets: bool, annotation_only: bool)
    -> (Vec<CompletionItem>, bool)
{
    let first_char = prefix.chars().next();
    let lowercase_mode = first_char.map(|c| c.is_lowercase()).unwrap_or(false);
    // Symmetric to lowercase_mode: user is deliberately typing a CamelCase name.
    let uppercase_mode = first_char.map(|c| c.is_uppercase()).unwrap_or(false);
    // True when the prefix is clearly CamelCase (uppercase first char + at least one
    // lowercase letter), meaning the user cannot be typing a SCREAMING_SNAKE constant.
    let camel_mode = uppercase_mode && prefix.chars().any(|c| c.is_lowercase());
    // For longer prefixes the user knows what they want: restrict tier-0/1 to
    // prefix/acronym matches only (no substring).  This prevents noisy substring
    // hits from filling the cap and crowding out precise tier-2 cross-package matches
    // (e.g. typing "ChildDash" must surface ChildDashboardViewModel even if the same
    // package has many classes that contain "child" as a substring).
    let local_max_score: u8 = if prefix.len() >= MIN_PREFIX_SCORE_REDUCTION { 1 } else { 2 };
    let mut seen = std::collections::HashSet::new();
    let mut items: Vec<CompletionItem> = Vec::new();

    // `src_tier` encodes symbol origin (0=same file, 1=same pkg, 2=cross-pkg, 3=stdlib).
    // Full sort_text: "{src_tier}{match_score}:{name_lower}" so that
    //   same-file exact match ("000:column") beats same-file acronym ("001:columnbutton")
    //   which beats same-pkg exact ("010:column"), etc.
    let mut add = |name: &str, kind: CompletionItemKind, src_tier: u8, max_score: u8, detail: &str, item_data: Option<serde_json::Value>| {
        // In annotation context (@Foo), only emit class/interface/type items.
        if annotation_only && matches!(kind,
            CompletionItemKind::FUNCTION | CompletionItemKind::METHOD |
            CompletionItemKind::VARIABLE | CompletionItemKind::FIELD  |
            CompletionItemKind::PROPERTY)
        {
            return;
        }
        // Case gates: match user intent by the capitalisation of what they typed.
        if lowercase_mode && name.starts_with_uppercase() {
            return;
        }
        if uppercase_mode && name.starts_with_lowercase() {
            return;
        }
        // CamelCase prefix → hide SCREAMING_SNAKE_CASE names (constants, enum variants).
        if camel_mode && is_screaming_snake(name) {
            return;
        }
        let score = match match_score(name, prefix) {
            Some(s) if s <= max_score => s,
            _ => return,
        };
        if !seen.insert(name.to_string()) { return; }
        let is_fn = snippets && matches!(kind, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
        items.push(CompletionItem {
            label:              name.to_string(),
            kind:               Some(kind),
            filter_text:        Some(name.to_string()),
            sort_text:          Some(format!("{}{}{}", src_tier, score, name.to_lowercase())),
            insert_text:        if is_fn { Some(format!("{}($1)", name)) } else { None },
            insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
            detail:             if detail.is_empty() { None } else { Some(detail.to_string()) },
            command:            if is_fn { Some(trigger_parameter_hints()) } else { None },
            data:               item_data,
            ..Default::default()
        });
    };

    // 1. Local file symbols — src_tier 0, substring fallback only for short prefixes.
    if let Some(f) = idx.files.get(from_uri.as_str()) {
        for sym in &f.symbols {
            add(&sym.name, symbol_kind_to_completion(sym.kind), 0, local_max_score,
                &sym.detail,
                Some(serde_json::json!({"u": from_uri.as_str(), "l": sym.selection_range.start.line, "c": sym.selection_range.start.character})));
        }
        // Constructor params / local vars from declared_names (lowercase only).
        if lowercase_mode {
            for name in &f.declared_names {
                add(name, CompletionItemKind::VARIABLE, 0, local_max_score, "", None);
            }
        }
    }

    // 2. Same-package symbols — src_tier 1, substring fallback only for short prefixes.
    let pkg = idx.files.get(from_uri.as_str())
        .and_then(|f| f.package.clone())
        .unwrap_or_default();
    if !pkg.is_empty() {
        if let Some(uris) = idx.packages.get(&pkg) {
            for uri_str in uris.iter() {
                if uri_str == from_uri.as_str() { continue; }
                if let Some(f) = idx.files.get(uri_str.as_str()) {
                    for sym in &f.symbols {
                        add(&sym.name, symbol_kind_to_completion(sym.kind), 1, local_max_score,
                            &sym.detail,
                            Some(serde_json::json!({"u": uri_str.as_str(), "l": sym.selection_range.start.line, "c": sym.selection_range.start.character})));
                    }
                }
            }
        }
    }

    // 3. Cross-package symbols — src_tier 2, uppercase mode only, prefix ≥ 2 chars,
    //    prefix/acronym matches only (max_score=1) — no substring flood.
    // Emits one CompletionItem per distinct FQN, with additionalTextEdits for auto-import.
    if !lowercase_mode && prefix.len() >= MIN_CASE_INSENSITIVE_PREFIX {
        let is_java = from_uri.as_str().ends_with(".java");
        // Prefer live_lines (updated on every keystroke) over the indexed snapshot so that
        // import deduplication and insertion position are based on the current buffer state.
        let live = idx.live_lines.get(from_uri.as_str()).map(|ll| ll.clone());
        let (cur_imports, cur_pkg, cur_lines) = idx.files.get(from_uri.as_str())
            .map(|f| {
                let lines = live.clone().unwrap_or_else(|| f.lines.clone());
                // Re-scan live lines for imports so we don't use a stale snapshot.
                let imports = if live.is_some() {
                    lines.parse_imports()
                } else {
                    f.imports.clone()
                };
                (imports, f.package.clone().unwrap_or_default(), lines)
            })
            .unwrap_or_else(|| {
                let lines = live.clone().unwrap_or_default();
                let imports = lines.parse_imports();
                (imports, String::new(), lines)
            });

        if let Ok(cache) = idx.bare_name_cache.read() {
            for name in cache.iter() {
                // Case gate + match quality gate (prefix or acronym only).
                if name.starts_with_lowercase() { continue; }
                if camel_mode && is_screaming_snake(name) { continue; }
                let score = match match_score(name, prefix) {
                    Some(s) if s <= 1 => s,
                    _ => continue,
                };

                // Already visible via tier-0 or tier-1 — skip, no import needed.
                if seen.contains(name.as_str()) { continue; }

                let fqns = fqns_for_name(idx, name);

                if fqns.is_empty() {
                    if seen.insert(name.clone()) {
                        items.push(CompletionItem {
                            label:       name.clone(),
                            kind:        Some(CompletionItemKind::CLASS),
                            filter_text: Some(name.clone()),
                            sort_text:   Some(format!("2{}:{}", score, name.to_lowercase())),
                            ..Default::default()
                        });
                    }
                    continue;
                }

                for fqn in &fqns {
                    let pkg_of_fqn = match fqn.rfind('.') {
                        Some(i) => &fqn[..i],
                        None => "",
                    };

                    let needs_import = !already_imported(fqn, &cur_imports)
                        && !cur_imports.iter().any(|imp| imp.is_star && imp.full_path == pkg_of_fqn)
                        && pkg_of_fqn != cur_pkg;

                    let item_key = format!("{}:{}", name, fqn);
                    if !seen.insert(item_key) { continue; }

                    let import_edit = if needs_import {
                        Some(vec![cur_lines.make_import_edit(fqn, is_java)])
                    } else {
                        None
                    };
                    let detail = if needs_import { Some(pkg_of_fqn.to_string()) } else { None };

                    items.push(CompletionItem {
                        label:                name.clone(),
                        kind:                 Some(CompletionItemKind::CLASS),
                        filter_text:          Some(name.clone()),
                        sort_text:            Some(format!("2{}:{}", score, name.to_lowercase())),
                        detail,
                        additional_text_edits: import_edit,
                        ..Default::default()
                    });
                }
            }
        }
    }

    // 4. Stdlib top-level / scope functions — src_tier 3.
    for mut item in bare_completions(snippets) {
        let label = item.label.clone();
        if lowercase_mode && label.starts_with_uppercase() {
            continue;
        }
        if camel_mode && is_screaming_snake(&label) { continue; }
        let score = match match_score(&label, prefix) {
            Some(s) if s <= 2 => s,
            _ => continue,
        };
        if seen.insert(label.clone()) {
            item.filter_text = Some(label.clone());
            item.sort_text   = Some(format!("3{}:{}", score, label.to_lowercase()));
            items.push(item);
        }
    }

    // Sort by computed sort_text so the best matches are first even before capping.
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));

    let hit_cap = items.len() > COMPLETION_CAP;
    items.truncate(COMPLETION_CAP);
    (items, hit_cap)
}

/// Collect all symbols from a file URI as completion items.
/// Results are cached in `idx.completion_cache` so the file is only parsed
/// (or converted) once; subsequent calls for the same URI return instantly.
fn symbols_from_uri_as_completions(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    // Fast path: already computed.
    if let Some(cached) = idx.completion_cache.get(file_uri) {
        return cached.as_ref().clone();
    }

    let items = build_completion_items(idx, file_uri);
    let arc = Arc::new(items.clone());
    idx.completion_cache.insert(file_uri.to_string(), arc);
    items
}

/// Build completion items for a file, from index or on-demand disk parse.
/// Always builds with snippet fields set; callers strip them if the client
/// doesn't support snippets.
fn build_completion_items(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // From index if available.
    if let Some(f) = idx.files.get(file_uri) {
        for sym in &f.symbols {
            let ck       = symbol_kind_to_completion(sym.kind);
            let vt       = vis_tag(sym.visibility);
            let sort_txt = format!("{vt}{}{}", kind_sort_rank(Some(ck)), sym.name);
            items.push(make_completion_item(&sym.name, ck, sort_txt, true));
        }
        for name in &f.declared_names {
            if !items.iter().any(|i: &CompletionItem| i.label == *name) {
                items.push(make_completion_item(name, CompletionItemKind::FIELD, format!("1{name}"), true));
            }
        }
        return items;
    }

    // Fall back to on-demand parse.
    if let Ok(url) = Url::parse(file_uri) {
        if let Ok(path) = url.to_file_path() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let file_data = parse_by_extension(file_uri, &content);
                for sym in &file_data.symbols {
                    let ck       = symbol_kind_to_completion(sym.kind);
                    let vt       = vis_tag(sym.visibility);
                    let sort_txt = format!("{vt}{}{}", kind_sort_rank(Some(ck)), sym.name);
                    items.push(make_completion_item(&sym.name, ck, sort_txt, true));
                }
                for name in &file_data.declared_names {
                    if !items.iter().any(|i: &CompletionItem| i.label == *name) {
                        items.push(make_completion_item(name, CompletionItemKind::FIELD, format!("1{name}"), true));
                    }
                }
            }
        }
    }
    items
}

fn symbol_kind_to_completion(kind: SymbolKind) -> CompletionItemKind {
    match kind {
        SymbolKind::FUNCTION | SymbolKind::METHOD => CompletionItemKind::FUNCTION,
        SymbolKind::CLASS                          => CompletionItemKind::CLASS,
        SymbolKind::INTERFACE                      => CompletionItemKind::INTERFACE,
        SymbolKind::ENUM                           => CompletionItemKind::ENUM,
        SymbolKind::ENUM_MEMBER                    => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::CONSTANT                       => CompletionItemKind::CONSTANT,
        SymbolKind::VARIABLE                       => CompletionItemKind::VARIABLE,
        SymbolKind::OBJECT | SymbolKind::MODULE    => CompletionItemKind::MODULE,
        _                                          => CompletionItemKind::VALUE,
    }
}

/// Build a single `CompletionItem` for a named symbol.
///
/// Functions and methods get a snippet `name($1)` so the cursor lands inside
/// the parentheses after accepting the completion.  All other kinds are plain
/// text insertions.
fn make_completion_item(name: &str, ck: CompletionItemKind, sort_text: String, snippets: bool) -> CompletionItem {
    let is_fn = snippets && matches!(ck, CompletionItemKind::FUNCTION | CompletionItemKind::METHOD);
    CompletionItem {
        label:              name.to_string(),
        kind:               Some(ck),
        sort_text:          Some(sort_text),
        insert_text:        if is_fn { Some(format!("{}($1)", name)) } else { None },
        insert_text_format: if is_fn { Some(InsertTextFormat::SNIPPET) } else { None },
        command:            if is_fn { Some(trigger_parameter_hints()) } else { None },
        ..Default::default()
    }
}

/// Public wrapper around `symbols_from_uri_as_completions` for use by the
/// pre-warmer in `indexer.rs`.  Builds + caches completion items for a file.
pub fn symbols_from_uri_as_completions_pub(idx: &Indexer, file_uri: &str) -> Vec<CompletionItem> {
    symbols_from_uri_as_completions(idx, file_uri)
}

/// LSP `Command` that tells the editor to open the parameter-hints (signature
/// help) popup immediately after a function completion is accepted.
/// Mirrors VS Code's built-in `editor.action.triggerParameterHints` command,
/// which is also what rust-analyzer emits.
fn trigger_parameter_hints() -> tower_lsp::lsp_types::Command {
    tower_lsp::lsp_types::Command {
        title:     "triggerParameterHints".into(),
        command:   "editor.action.triggerParameterHints".into(),
        arguments: None,
    }
}

// ─── impl Indexer wrappers ────────────────────────────────────────────────────

#[allow(dead_code)]
impl crate::indexer::Indexer {
    pub(crate) fn complete_dot(&self, receiver: &str, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
        complete_dot(self, receiver, from_uri, snippets)
    }
    pub(crate) fn complete_bare(&self, prefix: &str, from_uri: &Url, snippets: bool, annotation_only: bool) -> (Vec<CompletionItem>, bool) {
        complete_bare(self, prefix, from_uri, snippets, annotation_only)
    }
    pub(super) fn complete_super_w(&self, from_uri: &Url, snippets: bool) -> Vec<CompletionItem> {
        complete_super(self, from_uri, snippets)
    }
    pub(super) fn symbols_from_uri_as_completions_w(&self, file_uri: &str) -> Vec<CompletionItem> {
        symbols_from_uri_as_completions(self, file_uri)
    }
    pub(super) fn build_completion_items_w(&self, file_uri: &str) -> Vec<CompletionItem> {
        build_completion_items(self, file_uri)
    }
    pub fn symbols_from_uri_as_completions_pub(&self, file_uri: &str) -> Vec<CompletionItem> {
        symbols_from_uri_as_completions_pub(self, file_uri)
    }
}
