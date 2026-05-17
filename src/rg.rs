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
pub(crate) struct IgnoreMatcher {
    /// Original patterns as provided by the client (passed to `fd --exclude` as-is).
    pub patterns: Vec<String>,
    /// Arc-wrapped so the compiled set can be shared into `filter_entry` closures.
    glob_set: Arc<globset::GlobSet>,
    /// Workspace root this matcher was built for — used to relativize absolute paths.
    root: std::path::PathBuf,
}

impl IgnoreMatcher {
    /// Build an `IgnoreMatcher` from raw client patterns against `root`.
    pub(crate) fn new(patterns: Vec<String>, root: &Path) -> Self {
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
                    format!("**/{}/", normalized), // trailing / for dir match
                    format!("**/{normalized}/**"),
                ]
            } else {
                vec![normalized]
            };

            for glob_pat in glob_pats {
                match globset::Glob::new(&glob_pat) {
                    Ok(g) => {
                        builder.add(g);
                    }
                    Err(e) => {
                        log::warn!("ignorePatterns: invalid pattern {:?}: {}", pat, e);
                    }
                }
            }
        }
        let glob_set = builder.build().unwrap_or_else(|e| {
            log::warn!("ignorePatterns: failed to build glob set: {}", e);
            globset::GlobSetBuilder::new().build().unwrap()
        });
        Self {
            patterns,
            glob_set: Arc::new(glob_set),
            root: root.to_path_buf(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Returns `true` if `rel_path` (relative to workspace root) should be excluded.
    pub(crate) fn matches(&self, rel_path: &Path) -> bool {
        self.glob_set.is_match(rel_path)
    }

    /// Clone the Arc-wrapped glob set for use in `filter_entry` closures.
    pub(crate) fn glob_set(&self) -> Arc<globset::GlobSet> {
        Arc::clone(&self.glob_set)
    }

    /// Remove locations whose file path is inside an ignored directory.
    /// Paths are relativized against the workspace root this matcher was built for.
    pub(crate) fn filter_locs(&self, locs: Vec<Location>) -> Vec<Location> {
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
    pub(crate) fn filter_file_strings(&self, files: Vec<String>) -> Vec<String> {
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
    pub(crate) fn filter_paths(&self, paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
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
pub(crate) const SOURCE_EXTENSIONS: &[&str] = &["kt", "java", "swift"];

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
    let safe: String = name
        .chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || c == '_' {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect();
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
    open_file: Option<&Path>,
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
/// When `source_paths` is non-empty, rg searches only those directories instead
/// of `root`. `root` is still used as the base for resolving relative entries in
/// `source_paths` and as a fallback if every configured path is missing on disk.
///
/// Results in directories matched by `matcher` are filtered out.
pub(crate) fn rg_find_definition(
    name: &str,
    root: Option<&Path>,
    source_paths: &[String],
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let pattern = build_rg_pattern(name);

    // Use the provided root, or fall back to CWD (which editors like Helix
    // set to the workspace root when spawning the LSP server).
    let fallback_root: std::borrow::Cow<Path> = match root {
        Some(r) => std::borrow::Cow::Borrowed(r),
        None => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
    };

    let locs = RgSearch::scoped(source_paths, fallback_root.as_ref())
        .with_pattern(pattern)
        .locations();

    if let Some(m) = matcher {
        m.filter_locs(locs)
    } else {
        locs
    }
}

/// Request parameters for a ripgrep reference search.
pub(crate) struct RgSearchRequest<'a> {
    name: &'a str,
    parent_class: Option<&'a str>,
    declared_pkg: Option<&'a str>,
    /// Outer-outer class for a lowercase method declared inside a doubly-nested
    /// class (e.g. `create` inside `Factory` inside `RegularReducer`).
    ///
    /// When set, file discovery searches for files that mention this class (via
    /// `\bOwnerClass\b`) rather than using import or package patterns.  This
    /// ensures callers that reference the outer class via a variable name
    /// (`factory.create()`) are found, while sibling factories in the same
    /// package are excluded because they do not reference the outer class.
    owner_class: Option<&'a str>,
    search_root: std::borrow::Cow<'a, Path>,
    /// Source-root directories from workspace config; when non-empty, rg is
    /// scoped to these directories instead of the full workspace root.
    source_paths: &'a [String],
    include_decl: bool,
    from_uri: &'a Url,
    decl_files: &'a [String],
}

enum RgTarget<'a> {
    Root(&'a Path),
    /// Workspace source-root directories (paths under the workspace root).
    /// When set, rg searches only these directories instead of the full workspace root.
    /// Relative paths are resolved against `parse_root` at command-build time.
    SourcePaths(&'a [String]),
    Files(&'a [String]),
}

struct RgSearch<'a> {
    parse_root: &'a Path,
    target: RgTarget<'a>,
    patterns: Vec<String>,
    word_regexp: bool,
    list_files: bool,
}

impl<'a> RgSearch<'a> {
    fn rooted(root: &'a Path) -> Self {
        Self {
            parse_root: root,
            target: RgTarget::Root(root),
            patterns: Vec::new(),
            word_regexp: false,
            list_files: false,
        }
    }

    /// Search only within `source_paths` directories (when configured via `sourceRoots`).
    /// Falls back to `fallback_root` when `source_paths` is empty.
    fn scoped(source_paths: &'a [String], fallback_root: &'a Path) -> Self {
        if source_paths.is_empty() {
            return Self::rooted(fallback_root);
        }
        Self {
            parse_root: fallback_root,
            target: RgTarget::SourcePaths(source_paths),
            patterns: Vec::new(),
            word_regexp: false,
            list_files: false,
        }
    }

    fn files(files: &'a [String]) -> Self {
        Self {
            parse_root: Path::new("/"),
            target: RgTarget::Files(files),
            patterns: Vec::new(),
            word_regexp: false,
            list_files: false,
        }
    }

    fn with_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.patterns.push(pattern.into());
        self
    }

    fn with_patterns<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.patterns.extend(patterns.into_iter().map(Into::into));
        self
    }

    fn word_regexp(mut self) -> Self {
        self.word_regexp = true;
        self
    }

    fn list_files(mut self) -> Self {
        self.list_files = true;
        self
    }

    fn build_command(&self) -> Command {
        let mut command = Command::new("rg");
        if self.list_files {
            command.arg("-l");
        } else {
            command.args([
                "--no-heading",
                "--with-filename",
                "--line-number",
                "--column",
            ]);
        }
        if self.word_regexp {
            command.arg("--word-regexp");
        }
        for ext in SOURCE_EXTENSIONS {
            command.args(["--glob", &format!("*.{ext}")]);
        }
        for pattern in &self.patterns {
            command.args(["-e", pattern]);
        }
        match &self.target {
            RgTarget::Root(root) => {
                command.arg(root);
            }
            RgTarget::SourcePaths(paths) => {
                let mut any_added = false;
                for p in paths.iter() {
                    let path = Path::new(p);
                    let abs = if path.is_absolute() {
                        path.to_path_buf()
                    } else {
                        self.parse_root.join(path)
                    };
                    if abs.is_dir() {
                        command.arg(&abs);
                        any_added = true;
                    }
                }
                // If all configured source paths are missing, fall back to workspace root
                // so rg doesn't silently return zero results.
                if !any_added {
                    command.arg(self.parse_root);
                }
            }
            RgTarget::Files(files) => {
                command.arg("--");
                command.args(*files);
            }
        }
        command
    }

    fn output(&self) -> Option<std::process::Output> {
        let mut command = self.build_command();
        match command.output() {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => Some(output),
            _ => None,
        }
    }

    fn locations_with_content(&self) -> Vec<(Location, String)> {
        let Some(output) = self.output() else {
            return vec![];
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| parse_rg_line_with_content_rooted(line, self.parse_root))
            .collect()
    }

    fn locations(&self) -> Vec<Location> {
        self.locations_with_content()
            .into_iter()
            .map(|(location, _)| location)
            .collect()
    }

    fn files_with_matches(&self) -> Vec<String> {
        let Some(output) = self.output() else {
            return vec![];
        };
        let root = match &self.target {
            RgTarget::Root(root) => *root,
            RgTarget::Files(_) => return vec![],
            RgTarget::SourcePaths(_) => {
                // Apply the same relative-path normalization as the Root branch so that
                // a source root passed as relative (or rg run from a different cwd) doesn't
                // produce relative filenames that later fail URI construction.
                let parse_root = self.parse_root;
                return String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|line| {
                        let path = Path::new(line);
                        if path.is_absolute() {
                            line.to_owned()
                        } else {
                            parse_root.join(line).to_string_lossy().into_owned()
                        }
                    })
                    .collect();
            }
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                let path = Path::new(line);
                if path.is_absolute() {
                    line.to_owned()
                } else {
                    root.join(line).to_string_lossy().into_owned()
                }
            })
            .collect()
    }
}

impl<'a> RgSearchRequest<'a> {
    pub(crate) fn new(
        name: &'a str,
        parent_class: Option<&'a str>,
        declared_pkg: Option<&'a str>,
        root: Option<&'a Path>,
        include_decl: bool,
        from_uri: &'a Url,
        decl_files: &'a [String],
    ) -> Self {
        let search_root = match root {
            Some(root) => std::borrow::Cow::Borrowed(root),
            None => std::borrow::Cow::Owned(std::env::current_dir().unwrap_or_default()),
        };
        Self {
            name,
            parent_class,
            declared_pkg,
            owner_class: None,
            search_root,
            source_paths: &[],
            include_decl,
            from_uri,
            decl_files,
        }
    }

    pub(crate) fn with_source_paths(mut self, source_paths: &'a [String]) -> Self {
        self.source_paths = source_paths;
        self
    }

    pub(crate) fn with_owner_class(mut self, owner_class: &'a str) -> Self {
        self.owner_class = Some(owner_class);
        self
    }
}

const REFERENCE_DECLARATION_KEYWORDS: &[&str] = &[
    "class ",
    "interface ",
    "object ",
    "fun ",
    "val ",
    "var ",
    "typealias ",
    "enum class ",
    "enum ",
    "struct ",
    "protocol ",
    "func ",
    "let ",
    "extension ",
];

fn build_rg_patterns(request: &RgSearchRequest<'_>) -> Vec<String> {
    let safe_name = regex_escape(request.name);
    if let Some(parent_class) = request.parent_class {
        let safe_parent = regex_escape(parent_class);
        let qualified_pattern = format!(r"\b{}\.\b{}\b", safe_parent, safe_name);
        let direct_import_pattern =
            format!(r"import[^\n]*\b{}\.(?:{}\b|\*)", safe_parent, safe_name);
        vec![qualified_pattern, direct_import_pattern, safe_name]
    } else if let Some(declared_pkg) = request.declared_pkg {
        let safe_package = regex_escape(declared_pkg);
        let import_pattern = format!(
            r"import[^\n]*\b{safe_package}\b[^\n]*\b{safe_name}\b|import[^\n]*\b{safe_package}\b\.\*"
        );
        let package_pattern = format!(r"^\s*package\s+{safe_package}\s*$");
        vec![import_pattern, package_pattern, safe_name]
    } else {
        vec![safe_name]
    }
}

fn should_skip_reference(loc: &Location, content: &str, request: &RgSearchRequest<'_>) -> bool {
    let trimmed = content.trim_start();
    if trimmed.starts_with("import ") || trimmed.starts_with("package ") {
        return true;
    }
    if !request.include_decl {
        let is_declaration = REFERENCE_DECLARATION_KEYWORDS
            .iter()
            .any(|keyword| content.contains(keyword))
            && loc.uri.as_str() == request.from_uri.as_str();
        if is_declaration {
            return true;
        }
    }
    false
}

fn run_rg_search(request: &RgSearchRequest<'_>, patterns: &[String]) -> Vec<Location> {
    let mut search = RgSearch::scoped(request.source_paths, request.search_root.as_ref())
        .with_patterns(patterns.iter().cloned());
    if request.parent_class.is_none() && request.declared_pkg.is_none() {
        search = search.word_regexp();
    }
    search
        .locations_with_content()
        .into_iter()
        .filter_map(|(loc, content)| {
            (!should_skip_reference(&loc, &content, request)).then_some(loc)
        })
        .collect()
}

fn filter_candidate_files(
    candidate_files: Vec<String>,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<String> {
    match matcher {
        Some(matcher) => matcher.filter_file_strings(candidate_files),
        None => candidate_files,
    }
}

fn extend_unique_files(files: &mut Vec<String>, new_files: Vec<String>) {
    for file in new_files {
        if !files.contains(&file) {
            files.push(file);
        }
    }
}

fn merge_decl_files(candidate_files: &mut Vec<String>, decl_files: &[String]) {
    let mut existing: std::collections::HashSet<String> = candidate_files.iter().cloned().collect();
    for decl_file in decl_files {
        if existing.insert(decl_file.clone()) {
            candidate_files.push(decl_file.clone());
        }
    }
}

/// When `source_paths` is non-empty, filter `decl_files` to only those within the
/// configured source roots so declaration files outside the scope don't bypass scoping.
fn scope_decl_files<'a>(
    decl_files: &'a [String],
    source_paths: &'a [String],
) -> std::borrow::Cow<'a, [String]> {
    if source_paths.is_empty() {
        return std::borrow::Cow::Borrowed(decl_files);
    }
    // Use Path::starts_with (component-based) rather than str::starts_with to avoid
    // sibling-path false positives: "/src/main/kotlin2" must not match "/src/main/kotlin".
    let source_paths_buf: Vec<&Path> = source_paths.iter().map(|s| Path::new(s.as_str())).collect();
    let filtered: Vec<String> = decl_files
        .iter()
        .filter(|f| {
            let fp = Path::new(f.as_str());
            source_paths_buf.iter().any(|sp| fp.starts_with(sp))
        })
        .cloned()
        .collect();
    std::borrow::Cow::Owned(filtered)
}

/// Returns `true` when `content` contains `.<name>` preceded by a qualifier word
/// that is NOT `expected_parent`.  A bare `name` (no qualifier) is always allowed.
///
/// This prevents sibling qualified names from bleeding into results: when searching
/// for `ReducerA.Factory`, a line containing `ReducerC.Factory` must be excluded even
/// though it also contains the bare word `Factory`.
pub(crate) fn has_wrong_qualifier(content: &str, name: &str, expected_parent: &str) -> bool {
    let dotname = format!(".{name}");
    let mut offset = 0;
    while let Some(rel) = content[offset..].find(&dotname) {
        let match_start = offset + rel;
        let name_end = match_start + dotname.len();
        // Confirm word boundary after the name (not a prefix of a longer identifier).
        let after_ch = content[name_end..].chars().next();
        if after_ch.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            offset = name_end;
            continue;
        }
        // Extract the full qualifier chain immediately before the dot
        // (e.g. "Outer.Inner" for "Outer.Inner.Factory", "ReducerA" for "ReducerA.Factory").
        // We walk back over [A-Za-z0-9_.] so multi-segment chains are captured whole.
        let qualifier: String = content[..match_start]
            .chars()
            .rev()
            .take_while(|&c| c.is_alphanumeric() || c == '_' || c == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let qualifier = qualifier.trim_start_matches('.');
        if !qualifier.is_empty() && qualifier != expected_parent {
            return true;
        }
        offset = name_end;
    }
    false
}

fn append_unique_reference_hits(
    locations: &mut Vec<Location>,
    hits: Vec<(Location, String)>,
    request: &RgSearchRequest<'_>,
) {
    let mut seen: std::collections::HashSet<(String, u32, u32)> = locations
        .iter()
        .map(|location| {
            (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
            )
        })
        .collect();

    for (location, content) in hits {
        if should_skip_reference(&location, &content, request) {
            continue;
        }
        if let Some(parent) = request.parent_class {
            if has_wrong_qualifier(&content, request.name, parent) {
                continue;
            }
        }

        let key = (
            location.uri.to_string(),
            location.range.start.line,
            location.range.start.character,
        );
        if seen.insert(key) {
            locations.push(location);
        }
    }
}

fn parent_scoped_reference_locations(
    request: &RgSearchRequest<'_>,
    patterns: &[String],
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let mut locations = run_rg_search(request, &patterns[..1]);
    let mut candidate_files = filter_candidate_files(
        rg_files_with_matches_scoped(
            &patterns[1],
            request.source_paths,
            request.search_root.as_ref(),
        ),
        matcher,
    );
    merge_decl_files(
        &mut candidate_files,
        &scope_decl_files(request.decl_files, request.source_paths),
    );
    if !candidate_files.is_empty() {
        let bare_hits = rg_word_in_files(&patterns[2], &candidate_files);
        append_unique_reference_hits(&mut locations, bare_hits, request);
    }
    locations
}

fn package_scoped_reference_locations(
    request: &RgSearchRequest<'_>,
    patterns: &[String],
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let mut candidate_files = rg_files_with_matches_scoped(
        &patterns[0],
        request.source_paths,
        request.search_root.as_ref(),
    );
    extend_unique_files(
        &mut candidate_files,
        rg_files_with_matches_scoped(
            &patterns[1],
            request.source_paths,
            request.search_root.as_ref(),
        ),
    );
    let candidate_files = filter_candidate_files(candidate_files, matcher);
    if candidate_files.is_empty() {
        return vec![];
    }
    rg_word_in_files(&patterns[2], &candidate_files)
        .into_iter()
        .filter_map(|(location, content)| {
            (!should_skip_reference(&location, &content, request)).then_some(location)
        })
        .collect()
}

/// Find references to a lowercase method declared inside a doubly-nested class
/// (e.g. `create` inside `Factory` inside `RegularReducer`).
///
/// Callers use variable-name syntax (`factory.create()`) rather than qualified
/// syntax (`RegularReducer.create()`), so standard parent-class scoping misses
/// them.  Instead we find candidate files by searching for any mention of the
/// outer class, then do a bare-word search for the method name within those
/// files.  Sibling factories in the same package are naturally excluded because
/// they do not reference the outer class.
fn owner_scoped_reference_locations(
    request: &RgSearchRequest<'_>,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let owner_class = request.owner_class.expect("owner_class must be set");
    let safe_owner = regex_escape(owner_class);
    let safe_name = regex_escape(request.name);

    // Find files that mention the outer class (as a type, import, or constructor param).
    let owner_pattern = format!(r"\b{safe_owner}\b");
    let candidate_files = filter_candidate_files(
        rg_files_with_matches_scoped(
            &owner_pattern,
            request.source_paths,
            request.search_root.as_ref(),
        ),
        matcher,
    );

    if candidate_files.is_empty() {
        return vec![];
    }

    // Bare search for the method name in candidate files; qualifier filter is
    // intentionally skipped since callers use variable names, not class names.
    rg_word_in_files(&safe_name, &candidate_files)
        .into_iter()
        .filter_map(|(loc, content)| {
            (!should_skip_reference(&loc, &content, request)).then_some(loc)
        })
        .collect()
}

/// Run `rg` to find all *usages* of `name` in the project.
///
/// Uses `--word-regexp` so only whole-word matches are returned.
/// If `include_decl` is false, lines in `from_uri` that contain declaration
/// keywords (e.g. `fun`, `val`, `class`) alongside `name` are filtered out.
/// Other lines from `from_uri` are still included (e.g. call sites in the
/// same file).
///
/// Results in directories matched by `matcher` are filtered out.
pub(crate) fn rg_find_references(
    request: &RgSearchRequest<'_>,
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let result = if request.owner_class.is_some() {
        owner_scoped_reference_locations(request, matcher)
    } else {
        let patterns = build_rg_patterns(request);
        if request.parent_class.is_some() {
            parent_scoped_reference_locations(request, &patterns, matcher)
        } else if request.declared_pkg.is_some() {
            package_scoped_reference_locations(request, &patterns, matcher)
        } else {
            run_rg_search(request, &patterns)
        }
    };

    if let Some(matcher) = matcher {
        matcher.filter_locs(result)
    } else {
        result
    }
}

/// Quick heuristic rg-based implementor finder. Scans files that mention `name`
/// and returns locations where the line looks like a declaration/implementation
/// of that type (Kotlin/Java `class Foo : Interface`, `implements`, Swift
/// `class Foo: Protocol`, `struct Foo: Protocol`). This is a fallback when the
/// subtype index is empty during cold indexing.
///
/// Results in directories matched by `matcher` are filtered out.
pub(crate) fn rg_find_implementors(
    name: &str,
    root: Option<&Path>,
    source_paths: &[String],
    matcher: Option<&IgnoreMatcher>,
) -> Vec<Location> {
    let safe = name.to_string();
    let root = match root {
        Some(r) => r,
        None => return vec![],
    };
    // Search for the name in source files.
    let locs: Vec<Location> = RgSearch::scoped(source_paths, root)
        .with_pattern(safe)
        .locations_with_content()
        .into_iter()
        .filter_map(|(loc, content)| {
            let line = content.trim();
            // Heuristics: declaration-like lines
            // Kotlin/Java: class Foo, interface Foo, enum class Foo, class Foo : Interface
            // Java implements: class Foo implements Interface
            // Swift: class Foo: Protocol, struct Foo: Protocol, extension Foo: Protocol
            let lower = line.to_lowercase();
            if lower.contains("class ")
                || lower.contains("struct ")
                || lower.contains("interface")
                || lower.contains("enum")
                || lower.contains("extension ")
            {
                // Check that the name appears as a word and near a declaration keyword
                if line.contains(name) {
                    return Some(loc);
                }
            }
            None
        })
        .collect();
    match matcher {
        Some(m) => m.filter_locs(locs),
        None => locs,
    }
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
    let file = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col: u32 = parts.next()?.trim().parse().ok()?;

    let path = std::path::Path::new(file);
    // Silently skip if rg somehow gave us a relative path.
    if !path.is_absolute() {
        return None;
    }

    let uri = Url::from_file_path(path).ok()?;
    let pos = Position::new(line_num.saturating_sub(1), col.saturating_sub(1));
    Some(Location {
        uri,
        range: Range::new(pos, pos),
    })
}

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Escape a string for use as a regex literal (non-alphanumeric chars → `\c`).
pub(crate) fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_alphanumeric() || c == '_' {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect()
}

fn rg_files_with_matches_scoped(
    pattern: &str,
    source_paths: &[String],
    root: &Path,
) -> Vec<String> {
    RgSearch::scoped(source_paths, root)
        .list_files()
        .with_pattern(pattern.to_owned())
        .files_with_matches()
}

/// Run `rg --word-regexp NAME` restricted to specific files.
fn rg_word_in_files(safe_name: &str, files: &[String]) -> Vec<(Location, String)> {
    if files.is_empty() {
        return vec![];
    }
    RgSearch::files(files)
        .word_regexp()
        .with_pattern(safe_name.to_owned())
        .locations_with_content()
}

/// Plain word-boundary search for all occurrences of `name` under `root`.
///
/// Used by the CLI `refs --fast` subcommand.  Less precise than
/// `rg_find_references` (no package/class context) but zero-cost to run —
/// no index required.
///
/// When `source_paths` is non-empty, the search is scoped to those directories
/// instead of `root`, mirroring the scoping behaviour of other rg search functions.
pub(crate) fn rg_word_search(name: &str, root: &Path, source_paths: &[String]) -> Vec<Location> {
    RgSearch::scoped(source_paths, root)
        .word_regexp()
        .with_pattern(regex_escape(name))
        .locations()
}

fn parse_rg_line_with_content_rooted(line: &str, root: &Path) -> Option<(Location, String)> {
    let mut parts = line.splitn(4, ':');
    let file = parts.next()?;
    let line_num: u32 = parts.next()?.trim().parse().ok()?;
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let content = parts.next().unwrap_or("").to_string();

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
    Some((
        Location {
            uri,
            range: Range::new(pos, pos),
        },
        content,
    ))
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "rg_tests.rs"]
mod tests;
