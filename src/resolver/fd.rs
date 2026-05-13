//! `fd`-based file discovery for on-demand symbol resolution.
//!
//! Used when a symbol is referenced via an import that was never indexed
//! (e.g. file opened before the workspace scan completed).

use std::path::Path;

use tower_lsp::lsp_types::{Location, Url};

use crate::parser::parse_by_extension;

/// Derive the Kotlin package from an import path by taking all dot-separated
/// segments that start with a lowercase letter (package convention).
///
/// `cz.moneta.app.AccountPickerContract.Event` → `"cz.moneta.app"`
pub(super) fn package_prefix(import_path: &str) -> String {
    use crate::StrExt;
    import_path
        .split('.')
        .take_while(|s| s.starts_with_lowercase())
        .collect::<Vec<_>>()
        .join(".")
}

/// Uppercase segment stems in priority order — outer class first.
///
/// `com.example.OuterClass.InnerClass` → `["OuterClass", "InnerClass"]`
/// `com.example.Foo`                   → `["Foo"]`
pub(super) fn import_file_stems(import_path: &str) -> Vec<String> {
    use crate::StrExt;
    let upper: Vec<&str> = import_path
        .split('.')
        .filter(|s| s.starts_with_uppercase())
        .collect();
    match upper.as_slice() {
        [] => vec![],
        [only] => vec![only.to_string()],
        [.., par, lst] => vec![par.to_string(), lst.to_string()],
    }
}

/// Find and synchronously parse the file most likely to contain `symbol_name`.
///
/// Search strategy (fastest-first):
///   1. fd `--full-path` regex derived from the import's package dir + filename —
///      extremely precise; handles multi-module projects where files live in
///      subdirs like `app/src/main/java/cz/moneta/…/EProductScreen.java`
///   2. Fallback: global fd by filename only (handles non-standard layouts)
pub(super) fn fd_find_and_parse(
    symbol_name: &str,
    full_import_path: &str,
    root: Option<&Path>,
    matcher: Option<&crate::rg::IgnoreMatcher>,
) -> Vec<Location> {
    let pkg = package_prefix(full_import_path);
    let expected_pkg = if pkg.is_empty() {
        None
    } else {
        Some(pkg.as_str())
    };
    let pkg_dir = pkg.replace('.', "/");

    let ext_alt = crate::rg::SOURCE_EXTENSIONS.join("|");
    for stem in import_file_stems(full_import_path) {
        // Strategy 1: precise full-path regex including the package directory.
        // e.g. ".*/cz/moneta/data/compat/enums/product/EProductScreen\.(kt|java|swift)$"
        if let Some(root) = root {
            let pat = if pkg_dir.is_empty() {
                format!(r"{stem}\.({ext_alt})$")
            } else {
                format!(r".*/{pkg_dir}/{stem}\.({ext_alt})$")
            };
            let locs = fd_search_by_full_path_pattern(&pat, symbol_name, expected_pkg, root);
            let locs = match matcher {
                Some(m) => m.filter_locs(locs),
                None => locs,
            };
            if !locs.is_empty() {
                return locs;
            }
        }

        // Strategy 2: global filename-only search (fallback for flat / non-standard layouts).
        for ext in crate::rg::SOURCE_EXTENSIONS {
            let locs = fd_search_file(&format!("{stem}.{ext}"), symbol_name, expected_pkg, root);
            let locs = match matcher {
                Some(m) => m.filter_locs(locs),
                None => locs,
            };
            if !locs.is_empty() {
                return locs;
            }
        }
    }
    vec![]
}

/// fd `--full-path <regex>` — searches `root` for files whose absolute path
/// matches `pattern`.  Parses each hit and returns locations for `symbol_name`.
fn fd_search_by_full_path_pattern(
    pattern: &str,
    symbol_name: &str,
    expected_pkg: Option<&str>,
    root: &Path,
) -> Vec<Location> {
    let Some(root_str) = root.to_str() else {
        return vec![];
    };
    let out = match std::process::Command::new("fd")
        .args([
            "--type",
            "f",
            "--absolute-path",
            "--full-path",
            pattern,
            root_str,
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    parse_fd_hits(&out.stdout, symbol_name, expected_pkg)
}

fn fd_search_file(
    file_name: &str,
    symbol_name: &str,
    expected_pkg: Option<&str>,
    root: Option<&Path>,
) -> Vec<Location> {
    let mut cmd = std::process::Command::new("fd");
    cmd.args([
        "--type",
        "f",
        "--absolute-path",
        "--max-results",
        "10",
        file_name,
    ]);
    if let Some(r) = root {
        cmd.arg(r);
    }

    let out = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    parse_fd_hits(&out.stdout, symbol_name, expected_pkg)
}

/// Parse a list of newline-separated absolute file paths from fd output,
/// parse each file with the appropriate parser, and return locations for
/// `symbol_name`.  When `expected_pkg` is given the package-exact match is
/// returned immediately; otherwise the first match wins.  A non-exact match
/// is kept as a fallback and returned only if no exact match is found.
fn parse_fd_hits(stdout: &[u8], symbol_name: &str, expected_pkg: Option<&str>) -> Vec<Location> {
    let mut fallback: Option<Location> = None;

    for path_str in String::from_utf8_lossy(stdout).lines() {
        let path_str = path_str.trim();
        if path_str.is_empty() {
            continue;
        }

        let path = std::path::Path::new(path_str);
        let Ok(uri) = Url::from_file_path(path) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };

        let file_data = parse_by_extension(path_str, &content);
        let Some(sym) = file_data.symbols.iter().find(|s| s.name == symbol_name) else {
            continue;
        };

        let loc = Location {
            uri,
            range: sym.selection_range,
        };

        if let Some(pkg) = expected_pkg {
            if file_data.package.as_deref() == Some(pkg) {
                return vec![loc];
            }
            if fallback.is_none() {
                fallback = Some(loc);
            }
        } else {
            return vec![loc];
        }
    }

    fallback.map(|l| vec![l]).unwrap_or_default()
}
