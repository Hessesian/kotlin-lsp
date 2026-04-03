use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tower_lsp::lsp_types::*;

// ─── LSP progress notification helper ────────────────────────────────────────

mod progress {
    use tower_lsp::lsp_types::ProgressParams;

    /// `$/progress` notification — used to report workspace indexing status.
    pub(super) enum KotlinProgress {}
    impl tower_lsp::lsp_types::notification::Notification for KotlinProgress {
        type Params = ProgressParams;
        const METHOD: &'static str = "$/progress";
    }
}

use crate::parser;
use crate::types::{FileData, SymbolEntry};

/// Hard cap on workspace files indexed eagerly.
/// Files beyond this limit are resolved on-demand via `rg` when first needed.
/// Override by setting the `KOTLIN_LSP_MAX_FILES` environment variable.
const DEFAULT_MAX_INDEX_FILES: usize = 2000;



pub struct Indexer {
    /// URI string → parsed file data.
    pub(crate) files:       DashMap<String, Arc<FileData>>,
    /// Short name → definition locations  (fast first-pass lookup).
    pub(crate) definitions: DashMap<String, Vec<Location>>,
    /// Fully-qualified name → location   (e.g. "com.example.Foo" → …).
    pub(crate) qualified:   DashMap<String, Location>,
    /// Package name → vec of URI strings (for same-package resolution).
    pub(crate) packages:    DashMap<String, Vec<String>>,
    /// Absolute path to the workspace root, set once on first `index_workspace`.
    pub(crate) workspace_root: OnceLock<PathBuf>,
    /// URI string → xxHash of last indexed content (skip identical re-parses).
    content_hashes: DashMap<String, u64>,
    /// Semaphore capping concurrent parse workers.
    parse_sem: Arc<tokio::sync::Semaphore>,
    /// Times tree-sitter actually ran (used in tests).
    pub parse_count: AtomicU64,
    /// URI string → pre-built completion items for that file.
    /// Populated lazily on first dot-completion hit; cleared on re-index.
    pub(crate) completion_cache: DashMap<String, Arc<Vec<CompletionItem>>>,
    /// URI string → lines of the CURRENT document content.
    /// Updated synchronously on every did_change, bypassing the 120ms debounce.
    /// Used by `completions()` so dot-detection always sees the latest text.
    pub(crate) live_lines: DashMap<String, Vec<String>>,
}

impl Indexer {
    pub fn parse_sem(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.parse_sem)
    }

    pub fn new() -> Self {
        Self {
            files:          DashMap::new(),
            definitions:    DashMap::new(),
            qualified:      DashMap::new(),
            packages:       DashMap::new(),
            workspace_root: OnceLock::new(),
            content_hashes: DashMap::new(),
            // Allow at most 4 concurrent parse workers; workspace batch indexing
            // uses a dedicated await loop so this mainly guards did_change bursts.
            parse_sem:      Arc::new(tokio::sync::Semaphore::new(4)),
            parse_count:    AtomicU64::new(0),
            completion_cache: DashMap::new(),
            live_lines:     DashMap::new(),
        }
    }

    /// Update the live-lines cache for `uri` without any debounce.
    /// Called from `did_change` before the debounced re-index so that
    /// `completions()` always sees the current document text.
    pub fn set_live_lines(&self, uri: &Url, content: &str) {
        let lines: Vec<String> = content.lines().map(String::from).collect();
        self.live_lines.insert(uri.to_string(), lines);
    }

    /// Discover and index *.kt / *.java files under `root`, bounded by MAX_INDEX_FILES.
    /// Sends LSP `$/progress` notifications so the editor shows a status bar spinner.
    pub async fn index_workspace(self: Arc<Self>, root: &Path, client: tower_lsp::Client) {
        // Record workspace root so rg/fd always search within the project.
        let _ = self.workspace_root.set(root.to_path_buf());

        let max = std::env::var("KOTLIN_LSP_MAX_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_INDEX_FILES);

        let mut paths = find_source_files(root);
        let total = paths.len();

        // Shallower paths first.
        paths.sort_by_key(|p| p.components().count());
        let paths: Vec<_> = paths.into_iter().take(max).collect();
        let indexed_count = paths.len();

        let truncated = total > max;
        if truncated {
            log::warn!(
                "Large project: eagerly indexing {indexed_count}/{total} files \
                 (shallowest first). Deeper files resolved on-demand via rg. \
                 Set KOTLIN_LSP_MAX_FILES env var to raise the limit."
            );
        } else {
            log::info!("Indexing {total} source files under {}", root.display());
        }

        // ── LSP progress: begin ──────────────────────────────────────────────
        let token = NumberOrString::String("kotlin-lsp/indexing".into());
        // Ask the client to create a progress token. Use a short timeout — some
        // editors (older Helix versions) never reply, which would stall indexing.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.send_request::<tower_lsp::lsp_types::request::WorkDoneProgressCreate>(
                WorkDoneProgressCreateParams { token: token.clone() }
            )
        ).await;

        let begin_msg = if truncated {
            format!("Indexing {indexed_count}/{total} Kotlin files (shallowest first)…")
        } else {
            format!("Indexing {total} Kotlin files…")
        };
        client.send_notification::<progress::KotlinProgress>(ProgressParams {
            token: token.clone(),
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "kotlin-lsp".into(),
                cancellable: Some(false),
                message: Some(begin_msg),
                percentage: Some(0),
            })),
        }).await;

        // ── concurrent parse (up to 8 workers) ──────────────────────────────
        let sem          = Arc::new(tokio::sync::Semaphore::new(8));
        let done_count   = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::with_capacity(indexed_count);

        for path in paths {
            let sem        = Arc::clone(&sem);
            let idx        = Arc::clone(&self);
            let done       = Arc::clone(&done_count);
            let client2    = client.clone();
            let token2     = token.clone();
            let total_cnt  = indexed_count;

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        if let Ok(uri) = Url::from_file_path(&path) {
                            tokio::task::spawn_blocking(move || idx.index_content(&uri, &content))
                                .await
                                .ok();
                        }
                    }
                    Err(e) => log::warn!("Could not read {}: {e}", path.display()),
                }

                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                // Report progress every ~5% (but at least every 50 files).
                // Never send percentage=100 here — some editors (Helix) treat that
                // as "done" and clear the spinner before the End notification fires.
                // The End notification below carries the final summary.
                let report_every = (total_cnt / 20).max(50);
                if n % report_every == 0 && n < total_cnt {
                    let pct = ((n * 100) / total_cnt).min(99) as u32;
                    client2.send_notification::<progress::KotlinProgress>(ProgressParams {
                        token: token2,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                            WorkDoneProgressReport {
                                cancellable: Some(false),
                                message: Some(format!("{n}/{total_cnt} files")),
                                percentage: Some(pct),
                            }
                        )),
                    }).await;
                }
            }));
        }

        for h in handles { h.await.ok(); }

        // ── LSP progress: end ────────────────────────────────────────────────
        let sym_count = self.definitions.len();
        client.send_notification::<progress::KotlinProgress>(ProgressParams {
            token,
            value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: Some(format!(
                    "Indexed {} files, {} symbols{}",
                    self.files.len(), sym_count,
                    if truncated { format!(" (+{} lazy)", total - indexed_count) } else { String::new() }
                )),
            })),
        }).await;

        log::info!(
            "Done — {} files indexed, {} distinct symbols, {} packages",
            self.files.len(),
            self.definitions.len(),
            self.packages.len(),
        );
    }

    /// (Re-)parse and index a single file's content in-place.
    /// Returns immediately (no-op) when content is identical to the last indexed version.
    pub fn index_content(&self, uri: &Url, content: &str) {
        // Fast-path: skip re-parse if content hasn't changed since last index.
        let hash = hash_str(content);
        let uri_str = uri.to_string();
        if self.content_hashes.get(&uri_str).map(|h| *h == hash).unwrap_or(false) {
            return;
        }
        self.content_hashes.insert(uri_str.clone(), hash);
        self.parse_count.fetch_add(1, Ordering::Relaxed);
        // Invalidate cached completion items — the file is changing.
        self.completion_cache.remove(&uri_str);
        let is_kotlin = uri.path().ends_with(".kt");
        let data = if is_kotlin {
            parser::parse_kotlin(content)
        } else {
            parser::parse_java(content)
        };



        // ── Remove stale entries for this URI ─────────────────────────────────
        if let Some(old) = self.files.get(&uri_str) {
            for sym in &old.symbols {
                if let Some(mut locs) = self.definitions.get_mut(&sym.name) {
                    locs.retain(|l| l.uri.as_str() != uri.as_str());
                }
                if let Some(ref pkg) = old.package {
                    self.qualified.remove(&format!("{pkg}.{}", sym.name));
                }
            }
            if let Some(ref pkg) = old.package {
                if let Some(mut uris) = self.packages.get_mut(pkg) {
                    uris.retain(|u| u != &uri_str);
                }
            }
        }

        // ── Register fresh definitions ────────────────────────────────────────
        // Derive the file stem (e.g. "AccountContract" from "AccountContract.kt")
        // so we can also store nested-class qualified keys like
        // "com.example.AccountContract.State" in addition to "com.example.State".
        let file_stem: Option<String> = uri.to_file_path().ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));

        for sym in &data.symbols {
            let loc = Location { uri: uri.clone(), range: sym.selection_range };

            // short-name index (guarded against duplicates)
            let mut locs = self.definitions.entry(sym.name.clone()).or_default();
            if !locs.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                locs.push(loc.clone());
            }
            drop(locs);

            // qualified index: "com.example.Foo" → Location
            if let Some(ref pkg) = data.package {
                // Primary key: pkg.SymbolName
                self.qualified.insert(format!("{pkg}.{}", sym.name), loc.clone());

                // Secondary key: pkg.FileStem.SymbolName — covers nested/inner classes
                // whose import path includes the outer class name, e.g.
                // `import com.example.AccountContract.State` where State is nested
                // inside AccountContract.kt.
                if let Some(ref stem) = file_stem {
                    if *stem != sym.name {
                        self.qualified.insert(format!("{pkg}.{stem}.{}", sym.name), loc);
                    }
                }
            }
        }

        // ── Package membership ────────────────────────────────────────────────
        if let Some(ref pkg) = data.package {
            let mut uris = self.packages.entry(pkg.clone()).or_default();
            if !uris.contains(&uri_str) {
                uris.push(uri_str.clone());
            }
        }

        self.files.insert(uri_str, Arc::new(data));
    }

    /// Spawn background tasks to pre-warm the completion cache for all types
    /// declared in `uri` as constructor parameters or properties.
    ///
    /// This runs after `index_content` so that when the user types `repo.` the
    /// cache is already populated and the response is instant.
    pub fn prewarm_completion_cache(self: Arc<Self>, uri: &Url) {
        let Some(data) = self.files.get(uri.as_str()) else { return };
        let from_uri = uri.clone();

        // Collect unique type names from this file's lines.
        let mut type_names: Vec<String> = Vec::new();
        {
            let mut seen = std::collections::HashSet::new();
            for line in &data.lines {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with('*') { continue; }
                let mut rest = t;
                while let Some(ci) = rest.find(':') {
                    let after = rest[ci + 1..].trim_start();
                    let type_name: String = after.chars()
                        .take_while(|&c| c.is_alphanumeric() || c == '_')
                        .collect();
                    if !type_name.is_empty()
                        && type_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                        && seen.insert(type_name.clone())
                    {
                        type_names.push(type_name);
                    }
                    rest = &rest[ci + 1..];
                }
            }
        }
        drop(data);

        // Spawn a background task per type (capped to avoid bursts).
        let limit = Arc::new(tokio::sync::Semaphore::new(4));
        for type_name in type_names {
            // Skip if already cached or a primitive/stdlib type.
            let idx = Arc::clone(&self);
            let uri2 = from_uri.clone();
            let sem = Arc::clone(&limit);
            tokio::spawn(async move {
                let _permit = sem.acquire_owned().await;
                tokio::task::spawn_blocking(move || {
                    // Resolve the type to a file URI.
                    let locs = crate::resolver::resolve_symbol(&idx, &type_name, None, &uri2);
                    if let Some(loc) = locs.first() {
                        let file_uri = loc.uri.to_string();
                        // Only bother if not already cached.
                        if idx.completion_cache.contains_key(&file_uri) { return; }
                        // This call builds + caches the items.
                        crate::resolver::symbols_from_uri_as_completions_pub(&idx, &file_uri);
                    }
                }).await.ok();
            });
        }
    }
    ///
    /// LSP positions are UTF-16; for ASCII-heavy Kotlin/Java identifiers the
    /// character offset is identical to the UTF-16 unit offset.
    pub fn word_at(&self, uri: &Url, position: Position) -> Option<String> {
        self.word_and_qualifier_at(uri, position).map(|(w, _)| w)
    }

    /// Like `word_at` but also returns the single dot-qualifier immediately
    /// preceding the word, if any.
    ///
    /// `AccountPickerMapper.Content`  cursor on `Content`
    ///   → `Some(("Content", Some("AccountPickerMapper")))`
    ///
    /// `List<StaticDocument>` cursor on `StaticDocument`
    ///   → `Some(("StaticDocument", None))`
    pub fn word_and_qualifier_at(
        &self,
        uri: &Url,
        position: Position,
    ) -> Option<(String, Option<String>)> {
        let data = self.files.get(uri.as_str())?;
        let line = data.lines.get(position.line as usize)?;

        // UTF-16 → char index
        let target_utf16 = position.character as usize;
        let mut utf16_acc = 0usize;
        let mut char_idx  = 0usize;
        for ch in line.chars() {
            if utf16_acc >= target_utf16 { break; }
            utf16_acc += ch.len_utf16();
            char_idx  += 1;
        }

        let chars: Vec<char> = line.chars().collect();
        let effective = if char_idx < chars.len() && is_id_char(chars[char_idx]) {
            char_idx
        } else if char_idx > 0 && is_id_char(chars[char_idx - 1]) {
            char_idx - 1
        } else {
            return None;
        };

        let start = (0..=effective)
            .rev()
            .find(|&i| !is_id_char(chars[i]))
            .map(|i| i + 1)
            .unwrap_or(0);

        let end = (effective..chars.len())
            .find(|&i| !is_id_char(chars[i]))
            .unwrap_or(chars.len());

        if start >= end { return None; }
        let word: String = chars[start..end].iter().collect();
        if word == "_" { return None; }

        // Scan back over the full dot-chain preceding the word.
        // `A.B.C.D` cursor on `D` → qualifier `"A.B.C"`, not just `"C"`.
        // `resolve_qualified` then uses the ROOT segment ("A") to locate the file
        // and searches that file for the word ("D"), handling arbitrary nesting depth.
        let qualifier = if start >= 2 && chars[start - 1] == '.' {
            let mut scan = start - 1; // pointing at the final dot
            while scan > 0 && (is_id_char(chars[scan - 1]) || chars[scan - 1] == '.') {
                scan -= 1;
            }
            let q: String = chars[scan..start - 1].iter().collect();
            let q = q.trim_start_matches('.').to_string();
            if !q.is_empty() && q != "_" { Some(q) } else { None }
        } else { None };

        Some((word, qualifier))
    }

    /// Resolve definition locations for `name` (with optional dot-qualifier).
    #[allow(dead_code)]
    pub fn find_definition(&self, name: &str, from_uri: &Url) -> Vec<Location> {
        crate::resolver::resolve_symbol(self, name, None, from_uri)
    }

    pub fn find_definition_qualified(
        &self,
        name: &str,
        qualifier: Option<&str>,
        from_uri: &Url,
    ) -> Vec<Location> {
        crate::resolver::resolve_symbol(self, name, qualifier, from_uri)
    }

    /// Completion candidates at `position` in `uri`.
    ///
    /// Uses `live_lines` (updated synchronously on every keystroke) for the
    /// current file's line text, falling back to indexed lines or disk.
    pub fn completions(&self, uri: &Url, position: Position, snippets: bool) -> Vec<CompletionItem> {
        // Ensure the file is indexed — on first open, did_open's spawn_blocking
        // may not have finished by the time the first completion request arrives.
        if !self.files.contains_key(uri.as_str()) {
            if let Ok(path) = uri.to_file_path() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    self.index_content(uri, &content);
                }
            }
        }

        // Use live_lines (no debounce delay) for the current line so dot-detection
        // is always accurate even when the user types faster than the 120ms debounce.
        let line_owned: String;
        let line: &str = if let Some(ll) = self.live_lines.get(uri.as_str()) {
            if let Some(l) = ll.get(position.line as usize) {
                line_owned = l.clone();
                &line_owned
            } else { return vec![]; }
        } else if let Some(data) = self.files.get(uri.as_str()) {
            if let Some(l) = data.lines.get(position.line as usize) {
                line_owned = l.clone();
                &line_owned
            } else { return vec![]; }
        } else { return vec![]; };

        // Slice line up to cursor (UTF-16 aware).
        let target = position.character as usize;
        let mut utf16 = 0usize;
        let mut byte_end = line.len();
        for (bi, ch) in line.char_indices() {
            if utf16 >= target { byte_end = bi; break; }
            utf16 += ch.len_utf16();
        }
        let before = &line[..byte_end];

        // Extract the identifier fragment the user is currently typing.
        let prefix: String = before.chars()
            .rev()
            .take_while(|&c| c.is_alphanumeric() || c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        // Check if the char before the prefix is a dot.
        let before_prefix = &before[..before.len() - prefix.len()];
        let dot_receiver = if before_prefix.ends_with('.') {
            // Grab the identifier immediately preceding the dot.
            let recv: String = before_prefix[..before_prefix.len() - 1]
                .chars()
                .rev()
                .take_while(|&c| c.is_alphanumeric() || c == '_')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if recv.is_empty() { None } else { Some(recv) }
        } else {
            None
        };

        crate::resolver::complete_symbol(
            self,
            &prefix,
            dot_receiver.as_deref(),
            uri,
            snippets,
        )
    }

    /// Build a Markdown hover snippet for a symbol name.
    pub fn hover_info(&self, name: &str) -> Option<String> {
        // Check stdlib first so well-known symbols (run, apply, map, …) get
        // proper signatures even when no project source contains them.
        if let Some(md) = crate::stdlib::hover(name) { return Some(md); }

        // Drop the dashmap ref before taking the second one.
        let loc: Location = {
            let r = self.definitions.get(name)?;
            r.first()?.clone()
        };
        self.hover_info_at_location(&loc, name)
    }

    /// Build hover markdown for `name` at a specific resolved `Location`.
    /// Used by the hover handler so it shows the same symbol as go-to-definition.
    pub fn hover_info_at_location(&self, loc: &Location, name: &str) -> Option<String> {
        let data = self.files.get(loc.uri.as_str())?;
        let sym  = data.symbols.iter().find(|s| s.name == name)?;

        // Try to show the actual source line as context.
        let line_txt = data
            .lines
            .get(sym.selection_range.start.line as usize)
            .map(|l| l.trim().to_owned())
            .unwrap_or_default();

        let lang = if loc.uri.path().ends_with(".kt") { "kotlin" } else { "java" };

        if line_txt.is_empty() {
            Some(format!(
                "```{}\n{} {}\n```",
                lang,
                symbol_kw(sym.kind),
                name
            ))
        } else {
            Some(format!("```{}\n{}\n```", lang, line_txt))
        }
    }

    /// All symbols declared in the given file (for `documentSymbol`).
    pub fn file_symbols(&self, uri: &Url) -> Vec<SymbolEntry> {
        self.files
            .get(uri.as_str())
            .map(|d| d.symbols.clone())
            .unwrap_or_default()
    }
}

// ─── file discovery ──────────────────────────────────────────────────────────

fn find_source_files(root: &Path) -> Vec<std::path::PathBuf> {
    let root_str = root.to_string_lossy();

    // Prefer `fd` — order of magnitude faster for large trees.
    let fd_result = Command::new("fd")
        .args([
            "--type", "f",
            "--extension", "kt",
            "--extension", "java",
            "--absolute-path",
            "--exclude", ".git",
            "--exclude", "build",
            "--exclude", "target",
            "--exclude", ".gradle",
            ".",
            root_str.as_ref(),
        ])
        .output();

    if let Ok(out) = fd_result {
        if out.status.success() {
            let paths: Vec<_> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(std::path::PathBuf::from)
                .collect();
            if !paths.is_empty() {
                return paths;
            }
        }
    }

    log::info!("fd not available or found nothing; falling back to walkdir");
    walkdir_find(root)
}

fn walkdir_find(root: &Path) -> Vec<std::path::PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        // filter_entry prunes directories — don't descend into VCS / build dirs.
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            if e.file_type().is_dir() {
                !matches!(
                    name.as_ref(),
                    ".git" | "build" | "target" | "node_modules"
                        | ".gradle" | ".idea" | ".kotlin"
                )
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && matches!(
                    e.path().extension().and_then(|s| s.to_str()),
                    Some("kt") | Some("java")
                )
        })
        .map(|e| e.into_path())
        .collect()
}

// ─── hash helper ─────────────────────────────────────────────────────────────

/// Fast non-cryptographic hash of a string (FNV-1a, no extra dependencies).
fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ─── rg cross-file fallback ──────────────────────────────────────────────────

/// Run `rg` to find definition sites for `name`, scoped to `root`.
///
/// When `root` is an absolute path, rg outputs absolute paths in results.
/// Passing workspace root here is essential; without it rg would search
/// from CWD which may not be the project when spawned by the editor.
pub(crate) fn rg_find_definition(name: &str, root: Option<&Path>) -> Vec<Location> {
    let pattern = crate::resolver::build_rg_pattern(name);

    // Use the provided root, or fall back to CWD (which editors like Helix
    // set to the workspace root when spawning the LSP server).
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading",
        "--with-filename",
        "--line-number",
        "--column",
        // NOTE: rg has no --absolute-path flag; absolute output comes from
        // passing an absolute search root as the positional argument.
        "--glob", "*.kt",
        "--glob", "*.java",
        "-e", &pattern,
    ]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line)
        .collect()
}

/// Run `rg` to find all *usages* of `name` in the project.
///
/// Uses `--word-regexp` so only whole-word matches are returned.
/// If `include_decl` is false, declaration lines are filtered out by
/// excluding lines that contain declaration keywords before `name`.
/// If `from_uri` is provided, the source file is excluded when
/// `include_decl` is false (the definition is already known).
pub fn rg_find_references(
    name:         &str,
    root:         Option<&Path>,
    include_decl: bool,
    from_uri:     &Url,
) -> Vec<Location> {
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    // Escape name for use as a literal rg pattern.
    let safe: String = name.chars().flat_map(|c| {
        if c.is_alphanumeric() || c == '_' { vec![c] } else { vec!['\\', c] }
    }).collect();

    let mut cmd = Command::new("rg");
    cmd.args([
        "--no-heading",
        "--with-filename",
        "--line-number",
        "--column",
        "--word-regexp",
        "--glob", "*.kt",
        "--glob", "*.java",
        "-e", &safe,
    ]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };

    let decl_kws = ["class ", "interface ", "object ", "fun ", "val ", "var ",
                    "typealias ", "enum class ", "enum "];

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_rg_line_with_content)
        .filter(|(loc, content)| {
            // Optionally skip declaration lines.
            if include_decl { return true; }
            let is_decl = decl_kws.iter().any(|kw| content.contains(kw))
                && loc.uri.as_str() == from_uri.as_str();
            !is_decl
        })
        .map(|(loc, _)| loc)
        .collect()
}

/// Like `parse_rg_line` but also returns the matched line content.
fn parse_rg_line_with_content(line: &str) -> Option<(Location, String)> {
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;
    let content  = parts.next().unwrap_or("").to_string();

    let path = std::path::Path::new(file);
    if !path.is_absolute() { return None; }

    let uri = Url::from_file_path(path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some((Location { uri, range: Range::new(pos, pos) }, content))
}
pub(crate) fn parse_rg_line(line: &str) -> Option<Location> {
    // format: /abs/path/to/File.kt:line:col:content
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;

    let path = std::path::Path::new(file);
    // Silently skip if rg somehow gave us a relative path.
    if !path.is_absolute() { return None; }

    let uri = Url::from_file_path(path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some(Location { uri, range: Range::new(pos, pos) })
}

// ─── misc helpers ────────────────────────────────────────────────────────────

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn symbol_kw(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::CLASS          => "class",
        SymbolKind::INTERFACE      => "interface",
        SymbolKind::FUNCTION       => "fun",
        SymbolKind::METHOD         => "fun",
        SymbolKind::VARIABLE       => "var",
        SymbolKind::CONSTANT       => "val",
        SymbolKind::OBJECT         => "object",
        SymbolKind::TYPE_PARAMETER => "typealias",
        SymbolKind::ENUM           => "enum class",
        SymbolKind::FIELD          => "field",
        _                          => "symbol",
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(path: &str) -> Url {
        Url::parse(&format!("file:///test{path}")).unwrap()
    }

    fn indexed(path: &str, src: &str) -> (Url, Indexer) {
        let u = uri(path);
        let idx = Indexer::new();
        idx.index_content(&u, src);
        (u, idx)
    }

    // ── word_at ──────────────────────────────────────────────────────────────

    #[test]
    fn word_at_middle() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 5)), Some("foo".into()));
    }

    #[test]
    fn word_at_end_of_word() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        // character=7 is just past the 'o'; should still return "foo"
        assert_eq!(idx.word_at(&u, Position::new(0, 7)), Some("foo".into()));
    }

    #[test]
    fn word_at_operator_returns_none() {
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 8)), None); // '='
    }

    #[test]
    fn word_at_angle_bracket_steps_back_to_word() {
        let (u, idx) = indexed("/t.kt", "List<String>");
        // '<' at position 4 is not an id char, but 't' at position 3 is.
        // word_at steps back and returns the word ending there.
        assert_eq!(idx.word_at(&u, Position::new(0, 4)), Some("List".into()));
    }

    #[test]
    fn word_at_space_between_operator_and_number_returns_none() {
        // "val foo = 1"
        //  0123456789A
        // pos 9 = ' ' between '=' and '1'; prev char[8]='=' is also non-ident → None
        let (u, idx) = indexed("/t.kt", "val foo = 1");
        assert_eq!(idx.word_at(&u, Position::new(0, 9)), None);
    }

    // ── word_and_qualifier_at ────────────────────────────────────────────────

    #[test]
    fn qualifier_none_plain_word() {
        let (u, idx) = indexed("/t.kt", "val x: Bar = y");
        // cursor on 'B' of 'Bar'
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 7)),
            Some(("Bar".into(), None))
        );
    }

    #[test]
    fn qualifier_dot_access() {
        let (u, idx) = indexed("/t.kt", "val x = Outer.Inner");
        // cursor on 'I' of 'Inner'
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 14)),
            Some(("Inner".into(), Some("Outer".into())))
        );
    }

    #[test]
    fn qualifier_in_type_param() {
        let (u, idx) = indexed("/t.kt", "val x: List<Outer.Content>");
        // cursor on 'C' of 'Content' (position 18) — full chain captured
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 18)),
            Some(("Content".into(), Some("Outer".into())))
        );
    }

    #[test]
    fn qualifier_full_chain() {
        // A.B.C with cursor on C → full chain "A.B" not just "B"
        let (u, idx) = indexed("/t.kt", "A.B.C");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 4)),
            Some(("C".into(), Some("A.B".into())))
        );
    }

    #[test]
    fn qualifier_deep_chain() {
        // A.B.C.D.E cursor on E → qualifier is the full "A.B.C.D"
        let (u, idx) = indexed("/t.kt", "A.B.C.D.E");
        assert_eq!(
            idx.word_and_qualifier_at(&u, Position::new(0, 8)),
            Some(("E".into(), Some("A.B.C.D".into())))
        );
    }

    // ── index_content ────────────────────────────────────────────────────────

    #[test]
    fn symbol_found_after_indexing() {
        let (u, idx) = indexed("/t.kt", "class MyViewModel");
        assert!(!idx.find_definition("MyViewModel", &u).is_empty());
    }

    #[test]
    fn data_class_single_definition() {
        let (u, idx) = indexed("/t.kt", "data class Foo(val x: Int)");
        assert_eq!(idx.find_definition("Foo", &u).len(), 1);
    }

    #[test]
    fn stale_removed_on_reindex() {
        let u = uri("/t.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "class OldName");
        idx.index_content(&u, "class NewName");
        assert!(idx.find_definition("OldName", &u).is_empty(), "stale entry not removed");
        assert!(!idx.find_definition("NewName", &u).is_empty());
    }

    #[test]
    fn qualified_index_populated() {
        let (_, idx) = indexed("/t.kt", "package com.example\nclass Foo");
        assert!(idx.qualified.contains_key("com.example.Foo"));
    }

    #[test]
    fn qualified_removed_on_reindex() {
        let u = uri("/t.kt");
        let idx = Indexer::new();
        idx.index_content(&u, "package com.example\nclass OldName");
        idx.index_content(&u, "package com.example\nclass NewName");
        assert!(!idx.qualified.contains_key("com.example.OldName"), "stale qualified entry");
        assert!(idx.qualified.contains_key("com.example.NewName"));
    }

    #[test]
    fn packages_map_populated() {
        let (u, idx) = indexed("/t.kt", "package com.example\nclass Foo");
        let uris = idx.packages.get("com.example").unwrap();
        assert!(uris.contains(&u.to_string()));
    }

    // ── parse_rg_line ────────────────────────────────────────────────────────

    #[test]
    fn rg_line_absolute_path_parsed() {
        let line = "/home/user/project/Foo.kt:10:5:class Foo {";
        let loc = parse_rg_line(line).unwrap();
        assert_eq!(loc.range.start.line,      9); // 1-indexed → 0-indexed
        assert_eq!(loc.range.start.character, 4);
        assert_eq!(loc.uri.path(), "/home/user/project/Foo.kt");
    }

    #[test]
    fn rg_line_relative_path_ignored() {
        // Before the fix this would panic / produce a wrong URI
        let line = "src/Foo.kt:10:5:class Foo {";
        assert!(parse_rg_line(line).is_none(), "relative paths must be ignored");
    }

    // ── parse_count: verify deduplication ───────────────────────────────────

    #[test]
    fn index_same_content_parses_only_once() {
        let u   = uri("/Dedup.kt");
        let idx = Indexer::new();
        let src = "package com.test\nclass Dedup";

        // Call index_content 50 times with identical content.
        for _ in 0..50 {
            idx.index_content(&u, src);
        }
        assert_eq!(
            idx.parse_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "identical content should only trigger one tree-sitter parse"
        );
    }

    #[test]
    fn index_changed_content_reparses() {
        let u   = uri("/Changed.kt");
        let idx = Indexer::new();

        idx.index_content(&u, "class A");
        idx.index_content(&u, "class A"); // same — skipped
        idx.index_content(&u, "class B"); // different — must reparse
        idx.index_content(&u, "class B"); // same again — skipped

        assert_eq!(
            idx.parse_count.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "should parse exactly twice: once for 'class A', once for 'class B'"
        );
    }

    // ── completions ──────────────────────────────────────────────────────────

    #[test]
    fn dot_completion_triggers_on_dot() {
        let vm_uri   = uri("/ViewModel.kt");
        let repo_uri = uri("/Repository.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg\nclass Repository {\n  fun findById(id: Int) {}\n  fun save(obj: Any) {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg\nclass ViewModel(\n  private val repo: Repository\n) {\n  fun load() { return repo. }\n}");

        // Position after the dot on line 4
        let line = "  fun load() { return repo. }";
        let dot_col = (line.find("repo.").unwrap() + "repo.".len()) as u32;
        let items = idx.completions(&vm_uri, Position::new(4, dot_col), true);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findById"), "findById missing; got: {labels:?}");
        assert!(labels.contains(&"save"),     "save missing; got: {labels:?}");
    }

    #[test]
    fn dot_completion_with_prefix() {
        let vm_uri   = uri("/ViewModel2.kt");
        let repo_uri = uri("/Repo2.kt");
        let idx = Indexer::new();
        idx.index_content(&repo_uri,
            "package com.pkg2\nclass Repo2 {\n  fun findAll() {}\n  fun save() {}\n}");
        idx.index_content(&vm_uri,
            "package com.pkg2\nclass ViewModel2(\n  private val repo: Repo2\n) {\n  fun run() { repo.fin }\n}");

        let line = "  fun run() { repo.fin }";
        let col = (line.find("repo.fin").unwrap() + "repo.fin".len()) as u32;
        let items = idx.completions(&vm_uri, Position::new(4, col), true);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"findAll"), "findAll missing; got: {labels:?}");
    }

    #[test]
    fn nested_class_qualified_key() {
        // AccountContract.kt defines a sealed class State nested inside it.
        // The qualified index should store BOTH:
        //   "com.example.State"                    (primary)
        //   "com.example.AccountContract.State"    (nested — matches import path)
        let uri = uri("/AccountContract.kt");
        let idx = Indexer::new();
        idx.index_content(&uri,
            "package com.example\nclass AccountContract {\n  sealed class State\n  sealed class Event\n}");

        assert!(idx.qualified.contains_key("com.example.State"),
            "primary qualified key missing");
        assert!(idx.qualified.contains_key("com.example.AccountContract.State"),
            "nested qualified key missing");
        assert!(idx.qualified.contains_key("com.example.AccountContract.Event"),
            "nested Event qualified key missing");
    }
}
