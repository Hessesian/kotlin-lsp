//! ripgrep / glob helpers — workspace-wide symbol search.
//!
//! This module owns every item that shells out to `rg`:
//! - [`IgnoreMatcher`]   — compile and apply workspace ignore patterns
//! - [`SOURCE_EXTENSIONS`] — file extensions searched by `rg`/`fd`
//! - [`build_rg_pattern`] — build the regex passed to `rg -e`
//! - [`effective_rg_root`] — pick the best search root for a given open file
//! - [`rg_find_definition`] — locate declaration sites
//! - [`rg_find_references`] — locate all usages
//! - [`rg_find_implementors`] — heuristic implementor finder
//! - [`parse_rg_line`]   — parse one `rg --with-filename` output line

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use tower_lsp::lsp_types::{Location, Position, Range, Url};

// ─── Ignore pattern matcher ───────────────────────────────────────────────────

/// Compiled workspace-level ignore patterns from `initializationOptions`.
///
/// Patterns follow gitignore glob semantics:
/// - A bare pattern with no `/` (e.g. `bazel-*`) matches at any depth.
/// - A pattern containing `/` (e.g. `build/**`) matches relative to the workspace root.
/// - Absolute paths under the workspace root are normalized to relative before matching.
pub struct IgnoreMatcher {
    /// Original patterns as provided by the client (passed to `fd --exclude` as-is).
    pub patterns: Vec<String>,
    /// Arc-wrapped so the compiled set can be shared into `filter_entry` closures.
    glob_set: Arc<globset::GlobSet>,
    /// Workspace root this matcher was built for — used to relativize absolute paths.
    root: std::path::PathBuf,
}

impl IgnoreMatcher {
    /// Build an `IgnoreMatcher` from raw client patterns against `root`.
    pub fn new(patterns: Vec<String>, root: &Path) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        for pat in &patterns {
            // Normalize absolute paths that fall under the workspace root.
            let normalized = if Path::new(pat.as_str()).is_absolute() {
                match Path::new(pat.as_str()).strip_prefix(root) {
                    Ok(rel) => rel.to_string_lossy().into_owned(),
                    Err(_) => {
                        log::warn!("ignorePatterns: absolute path {:?} is not under workspace root, skipping", pat);
                        continue;
                    }
                }
            } else {
                pat.clone()
            };

            // Bare patterns (no `/`) match at any depth.
            // Compile two variants:
            //   `**/pattern`    — matches the directory entry itself (used in walkdir filter_entry)
            //   `**/pattern/**` — matches all files inside a matching directory (used in filter_locs)
            let glob_pats: Vec<String> = if !normalized.contains('/') {
                vec![
                    format!("**/{}", normalized),
                    format!("**/{}/", normalized),   // trailing / for dir match
                    format!("**/{normalized}/**"),
                ]
            } else {
                vec![normalized]
            };

            for glob_pat in glob_pats {
                match globset::Glob::new(&glob_pat) {
                    Ok(g) => { builder.add(g); }
                    Err(e) => { log::warn!("ignorePatterns: invalid pattern {:?}: {}", pat, e); }
                }
            }
        }
        let glob_set = builder.build().unwrap_or_else(|e| {
            log::warn!("ignorePatterns: failed to build glob set: {}", e);
            globset::GlobSetBuilder::new().build().unwrap()
        });
        Self { patterns, glob_set: Arc::new(glob_set), root: root.to_path_buf() }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Returns `true` if `rel_path` (relative to workspace root) should be excluded.
    pub fn matches(&self, rel_path: &Path) -> bool {
        self.glob_set.is_match(rel_path)
    }

    /// Clone the Arc-wrapped glob set for use in `filter_entry` closures.
    pub fn glob_set(&self) -> Arc<globset::GlobSet> {
        Arc::clone(&self.glob_set)
    }

    /// Remove locations whose file path is inside an ignored directory.
    /// Paths are relativized against the workspace root this matcher was built for.
    pub fn filter_locs(&self, locs: Vec<Location>) -> Vec<Location> {
        locs.into_iter()
            .filter(|loc| {
                if let Ok(path) = loc.uri.to_file_path() {
                    let rel = path.strip_prefix(&self.root).unwrap_or(&path);
                    !self.matches(rel)
                } else {
                    true
                }
            })
            .collect()
    }

    /// Remove file paths (absolute strings) that fall inside an ignored directory.
    pub fn filter_file_strings(&self, files: Vec<String>) -> Vec<String> {
        files
            .into_iter()
            .filter(|f| {
                let path = Path::new(f);
                let rel = path.strip_prefix(&self.root).unwrap_or(path);
                !self.matches(rel)
            })
            .collect()
    }

    /// Remove `PathBuf` entries that fall inside an ignored directory.
    pub fn filter_paths(&self, paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
        paths
            .into_iter()
            .filter(|p| {
                let rel = p.strip_prefix(&self.root).unwrap_or(p);
                !self.matches(rel)
            })
            .collect()
    }
}

// ─── Constants ────────────────────────────────────────────────────────────────

/// Supported file extensions for indexing and rg/fd searches.
pub const SOURCE_EXTENSIONS: &[&str] = &["kt", "java", "swift"];

// ─── Pattern builder ─────────────────────────────────────────────────────────

/// Build the regex pattern used by `rg` for declaration sites.
///
/// Matches both Kotlin and Java declaration keywords followed by `NAME`.
///
/// Kotlin: `fun`, `class`, `object`, `val`, `var`, `typealias`, `enum class`,
///         extension functions `fun ReceiverType.name`
/// Java:   `class`, `interface`, `enum` (standalone, no `class` suffix),
///         with any leading access/modifier keywords ignored
pub(crate) fn build_rg_pattern(name: &str) -> String {
    let safe: String = name.chars().flat_map(|c| {
        if c.is_alphanumeric() || c == '_' { vec![c] } else { vec!['\\', c] }
    }).collect();
    // Kotlin: standard keywords + `enum class` + extension function receiver
    // Java:   `enum NAME` (Java enums have no `class` after `enum`)
    // Swift:  struct, protocol, extension, let (in addition to shared keywords)
    format!(
        r"(?:(?:class|struct|interface|object|protocol|fun|func|val|var|let|typealias|enum\s+class)\s+|fun\s+\w[\w.]*\.|(?:public|private|protected|fileprivate|open|internal|static|abstract|final|\s)+(?:enum|class|struct|protocol)\s+|extension\s+){safe}\b"
    )
}

// ─── Root helpers ─────────────────────────────────────────────────────────────

/// Walk up from `file` until a `.git` directory is found, returning that
/// ancestor as the project root.  Returns `None` if no `.git` is found.
fn walk_to_git_root(file: &Path) -> Option<PathBuf> {
    let mut cur = file.parent()?;
    loop {
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

/// Return the best search root for rg/fd fallbacks given:
/// - `workspace_root` — the globally configured root (may point to a different project)
/// - `open_file`      — the file the user has open right now
///
/// If `open_file` lives inside `workspace_root`, use `workspace_root`.
/// Otherwise walk up from `open_file` to find a `.git` root and use that,
/// so rg searches the *actual* project even when the workspace config is stale.
pub(crate) fn effective_rg_root(
    workspace_root: Option<&Path>,
    open_file:      Option<&Path>,
) -> Option<PathBuf> {
    match (workspace_root, open_file) {
        (Some(root), Some(fp)) if fp.starts_with(root) => Some(root.to_path_buf()),
        (_, Some(fp)) => walk_to_git_root(fp)
            .or_else(|| fp.parent().map(|p| p.to_path_buf()))
            .or_else(|| std::env::current_dir().ok()),
        (Some(root), None) => Some(root.to_path_buf()),
        (None, None) => std::env::current_dir().ok(),
    }
}

// ─── Public rg search functions ──────────────────────────────────────────────

/// Run `rg` to find definition sites for `name`, scoped to `root`.
///
/// When `root` is an absolute path, rg outputs absolute paths in results.
/// Passing workspace root here is essential; without it rg would search
/// from CWD which may not be the project when spawned by the editor.
///
/// Results in directories matched by `matcher` are filtered out.
pub(crate) fn rg_find_definition(name: &str, root: Option<&Path>, matcher: Option<&IgnoreMatcher>) -> Vec<Location> {
    let pattern = build_rg_pattern(name);

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
    ]);
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", &pattern]);
    cmd.arg(search_root.as_ref());

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let locs: Vec<Location> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, &search_root).map(|(loc, _)| loc))
        .collect();

    if let Some(m) = matcher { m.filter_locs(locs) } else { locs }
}

/// Run `rg` to find all *usages* of `name` in the project.
///
/// Uses `--word-regexp` so only whole-word matches are returned.
/// If `include_decl` is false, declaration lines are filtered out by
/// excluding lines that contain declaration keywords before `name`.
/// If `from_uri` is provided, the source file is excluded when
/// `include_decl` is false (the definition is already known).
///
/// Results in directories matched by `matcher` are filtered out.
pub fn rg_find_references(
    name:         &str,
    parent_class: Option<&str>,
    declared_pkg: Option<&str>,
    root:         Option<&Path>,
    include_decl: bool,
    from_uri:     &Url,
    // Absolute file paths where `name` is declared — always included in bare-word
    // search so the declaration site itself is never missed (it uses bare `Name`,
    // not the qualified `Parent.Name` form that Pass A searches for).
    decl_files:   &[String],
    matcher:      Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let search_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None    => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let safe_name: String = regex_escape(name);
    let decl_kws = ["class ", "interface ", "object ", "fun ", "val ", "var ",
                    "typealias ", "enum class ", "enum ",
                    // Swift
                    "struct ", "protocol ", "func ", "let ", "extension "];

    let filter = |(loc, content): (Location, String)| -> Option<Location> {
        let trimmed = content.trim_start();
        // Import and package lines are never real references.
        if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
            return None;
        }
        if !include_decl {
            let is_decl = decl_kws.iter().any(|kw| content.contains(kw))
                && loc.uri.as_str() == from_uri.as_str();
            if is_decl { return None; }
        }
        Some(loc)
    };

    let result = if let Some(parent) = parent_class {
        // ── Scoped references: parent class is known ──────────────────────────
        //
        // Pass A: qualified form `ParentClass.Name` — works in any file.
        let safe_parent = regex_escape(parent);
        let qualified_pat = format!(r"\b{}\.\b{}\b", safe_parent, safe_name);
        let mut locs: Vec<Location> = rg_raw(&qualified_pat, &search_root)
            .into_iter()
            .filter_map(filter)
            .collect();

        // Pass B: bare `Name` restricted to files that directly import the inner
        // class itself (`import …ParentClass.Name` or `import …ParentClass.*`)
        // OR are in the same package.
        //
        // NOTE: we intentionally do NOT match files that only import the parent
        // class itself (`import …ParentClass`) — those files use the qualified
        // form `ParentClass.Name` which is already captured by Pass A, and
        // including them causes massive false-positive counts (e.g. every
        // ViewModel importing another ViewModel that also has a sealed `Effect`).
        //
        // Step B1 — files with explicit inner-class import.
        // Pattern must match the parent and name as ADJACENT dot-segments:
        //   import …ParentClass.Name   or   import …ParentClass.*
        // NOT files that merely mention both words (e.g. OtherContract.State).
        let direct_import_pat = format!(
            r"import[^\n]*\b{}\.(?:{}\b|\*)",
            safe_parent, safe_name
        );
        let candidate_files = rg_files_with_matches(&direct_import_pat, &search_root);
        // Filter candidate files against ignore patterns before searching them.
        let candidate_files = matcher
            .map_or(candidate_files.clone(), |m| m.filter_file_strings(candidate_files));

        // Step B2 — files in the same package as the parent class declaration.
        // NOTE: for inner classes, same-package files use the QUALIFIED form
        // `ParentClass.Name` which is already caught by Pass A. Adding them to
        // the bare-name search causes false positives (e.g. AbilitiesSectionViewModel
        // in the same package has its own `State`). So we skip same-package here.

        // Merge candidate file sets.
        // Always include declaration files so the declaration site itself is
        // never missed (it uses bare `Name`, not the qualified `Parent.Name` form).
        let mut all_files: Vec<String> = candidate_files;
        for f in decl_files {
            if !all_files.contains(f) { all_files.push(f.clone()); }
        }

        if !all_files.is_empty() {
            let bare_hits = rg_word_in_files(&safe_name, &all_files);
            // Deduplicate against the qualified hits using a HashSet for O(1) lookup.
            let seen: std::collections::HashSet<(String, u32, u32)> = locs.iter()
                .map(|l| (l.uri.to_string(), l.range.start.line, l.range.start.character))
                .collect();
            for (loc, content) in bare_hits {
                if let Some(loc) = filter((loc, content)) {
                    let key = (loc.uri.to_string(), loc.range.start.line, loc.range.start.character);
                    if !seen.contains(&key) {
                        locs.push(loc);
                    }
                }
            }
        }

        locs
    } else if let Some(dpkg) = declared_pkg {
        // ── Top-level symbol with known declared package ──────────────────────
        // Only search files that import `declared_pkg.Name` or `declared_pkg.*`
        // or are in the same package. This avoids the "13000 matches for Effect"
        // problem where every ViewModel has an inner class with the same name.
        let safe_pkg = regex_escape(dpkg);
        let import_pat = format!(
            r"import[^\n]*\b{safe_pkg}\b[^\n]*\b{safe_name}\b|import[^\n]*\b{safe_pkg}\b\.\*"
        );
        let pkg_pat = format!(r"^\s*package\s+{safe_pkg}\s*$");

        let mut candidate_files = rg_files_with_matches(&import_pat, &search_root);
        for f in rg_files_with_matches(&pkg_pat, &search_root) {
            if !candidate_files.contains(&f) { candidate_files.push(f); }
        }
        // Filter candidate files against ignore patterns before searching them.
        let candidate_files = matcher
            .map_or(candidate_files.clone(), |m| m.filter_file_strings(candidate_files));

        if candidate_files.is_empty() {
            return vec![];
        }
        rg_word_in_files(&safe_name, &candidate_files)
            .into_iter()
            .filter_map(filter)
            .collect()
    } else {
        // ── Fully unscoped: lowercase / unknown symbol ────────────────────────
        let mut cmd = Command::new("rg");
        cmd.args([
            "--no-heading", "--with-filename", "--line-number", "--column",
            "--word-regexp",
        ]);
        for ext in SOURCE_EXTENSIONS {
            cmd.args(["--glob", &format!("*.{ext}")]);
        }
        cmd.args(["-e", &safe_name]);
        cmd.arg(search_root.as_ref());

        let out = match cmd.output() {
            Ok(o) if !o.stdout.is_empty() => o,
            _ => return vec![],
        };

        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| parse_rg_line_with_content_rooted(l, &search_root))
            .filter_map(filter)
            .collect()
    };

    if let Some(m) = matcher { m.filter_locs(result) } else { result }
}

/// Quick heuristic rg-based implementor finder. Scans files that mention `name`
/// and returns locations where the line looks like a declaration/implementation
/// of that type (Kotlin/Java `class Foo : Interface`, `implements`, Swift
/// `class Foo: Protocol`, `struct Foo: Protocol`). This is a fallback when the
/// subtype index is empty during cold indexing.
///
/// Results in directories matched by `matcher` are filtered out.
pub fn rg_find_implementors(name: &str, root: Option<&Path>, matcher: Option<&IgnoreMatcher>) -> Vec<Location> {
    let safe = name.to_string();
    let root = match root {
        Some(r) => r,
        None => return vec![],
    };
    // Search for the name in source files.
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "--line-number", "--column", "-e", &safe]);
    for ext in SOURCE_EXTENSIONS { cmd.args(["--glob", &format!("*.{ext}")]); }
    cmd.arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    let locs: Vec<Location> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, root))
        .filter_map(|(loc, content)| {
            let line = content.trim();
            // Heuristics: declaration-like lines
            // Kotlin/Java: class Foo, interface Foo, enum class Foo, class Foo : Interface
            // Java implements: class Foo implements Interface
            // Swift: class Foo: Protocol, struct Foo: Protocol, extension Foo: Protocol
            let lower = line.to_lowercase();
            if lower.contains("class ") || lower.contains("struct ") || lower.contains("interface") || lower.contains("enum") || lower.contains("extension ") {
                // Check that the name appears as a word and near a declaration keyword
                if line.contains(name) {
                    return Some(loc);
                }
            }
            None
        })
        .collect();
    matcher.map_or(locs.clone(), |m| m.filter_locs(locs))
}

/// Parse one line of `rg --no-heading --with-filename --line-number --column`
/// output and return a [`Location`].
///
/// Expects the format `/abs/path/to/File.kt:line:col:content`.
/// Returns `None` if `file` is a relative path (rg sometimes emits relative
/// paths when invoked with a relative root; callers that need relative-path
/// support should use [`parse_rg_line_with_content_rooted`] instead).
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

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Escape a string for use as a regex literal (non-alphanumeric chars → `\c`).
pub(crate) fn regex_escape(s: &str) -> String {
    s.chars().flat_map(|c| {
        if c.is_alphanumeric() || c == '_' { vec![c] } else { vec!['\\', c] }
    }).collect()
}

/// Run rg with a regex pattern; return `(Location, line_content)` pairs.
fn rg_raw(pattern: &str, root: &Path) -> Vec<(Location, String)> {
    let mut cmd = Command::new("rg");
    cmd.args(["--no-heading", "--with-filename", "--line-number", "--column"]);
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", pattern]).arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, root))
        .collect()
}

/// Run `rg -l` to get the list of files matching a pattern.
fn rg_files_with_matches(pattern: &str, root: &Path) -> Vec<String> {
    let mut cmd = Command::new("rg");
    cmd.arg("-l");
    for ext in SOURCE_EXTENSIONS {
        cmd.args(["--glob", &format!("*.{ext}")]);
    }
    cmd.args(["-e", pattern]).arg(root);
    let out = match cmd.output() {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let p = std::path::Path::new(l);
            if p.is_absolute() {
                l.to_owned()
            } else {
                root.join(l).to_string_lossy().into_owned()
            }
        })
        .collect()
}

/// Run `rg --word-regexp NAME` restricted to specific files.
fn rg_word_in_files(safe_name: &str, files: &[String]) -> Vec<(Location, String)> {
    if files.is_empty() { return vec![]; }
    let out = match Command::new("rg")
        .args(["--no-heading", "--with-filename", "--line-number", "--column",
               "--word-regexp", "-e", safe_name, "--"])
        .args(files)
        .output()
    {
        Ok(o) if !o.stdout.is_empty() => o,
        _ => return vec![],
    };
    // Files passed to rg_word_in_files are already absolute (from rg_files_with_matches).
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| parse_rg_line_with_content_rooted(l, std::path::Path::new("/")))
        .collect()
}

fn parse_rg_line_with_content_rooted(line: &str, root: &Path) -> Option<(Location, String)> {
    let mut parts = line.splitn(4, ':');
    let file     = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col:      u32 = parts.next()?.trim().parse().ok()?;
    let content  = parts.next().unwrap_or("").to_string();

    let path = std::path::Path::new(file);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };

    // Only canonicalize when the path is not already absolute (e.g. relative workspace root).
    // Avoid the syscall-per-result cost on large workspaces where the root is always absolute.
    let abs_path = if abs_path.is_absolute() {
        abs_path
    } else {
        abs_path.canonicalize().unwrap_or(abs_path)
    };
    let uri = Url::from_file_path(&abs_path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some((Location { uri, range: Range::new(pos, pos) }, content))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "rg_tests.rs"]
mod tests;
