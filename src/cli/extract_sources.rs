//! CLI `extract-sources` subcommand.
//!
//! Finds `*-sources.jar` files in the Gradle cache, deduplicates by keeping
//! only the latest version of each artifact, and extracts `.kt`/`.java` sources
//! to an output directory for use with `sourcePaths` configuration.
//!
//! This is a Rust port of `contrib/extract-sources.py`.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) struct ExtractOptions {
    /// Override `$GRADLE_USER_HOME`. Defaults to `~/.gradle`.
    pub gradle_home: Option<PathBuf>,
    /// Extraction output root. Defaults to `~/.kotlin-lsp/sources`.
    pub output: Option<PathBuf>,
    /// Print what would be done without writing any files.
    pub dry_run: bool,
    /// Optional substring filters on artifact paths (e.g. `androidx.compose`).
    pub patterns: Vec<String>,
}

// ── version comparison ────────────────────────────────────────────────────────

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
enum VersionPart {
    Numeric(u64),
    Text(String),
}

fn version_key(version: &str) -> Vec<VersionPart> {
    version
        .split(['.', '-'])
        .map(|p| match p.parse::<u64>() {
            Ok(n) => VersionPart::Numeric(n),
            Err(_) => VersionPart::Text(p.to_owned()),
        })
        .collect()
}

// ── Gradle cache path parsing ─────────────────────────────────────────────────

struct GradleMeta {
    group: String,
    artifact: String,
    version: String,
}

/// Parse `(group, artifact, version)` from a Gradle module cache path.
///
/// Gradle cache layout:
/// `<home>/caches/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/<file>`
fn parse_jar_meta(jar: &Path) -> Option<GradleMeta> {
    let s = jar.to_string_lossy();
    let idx = s.find("files-2.1")? + "files-2.1".len();
    let rest = s[idx..].trim_start_matches(['/', '\\']);
    let parts: Vec<&str> = rest.splitn(5, ['/', '\\']).collect();
    if parts.len() < 3 {
        return None;
    }
    Some(GradleMeta {
        group: parts[0].to_owned(),
        artifact: parts[1].to_owned(),
        version: parts[2].to_owned(),
    })
}

fn artifact_dir_name(jar: &Path) -> String {
    if let Some(meta) = parse_jar_meta(jar) {
        return format!("{}.{}", meta.group, meta.artifact);
    }
    let name = jar
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    for suffix in &["-sources.jar", ".jar"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    name.to_owned()
}

// ── jar discovery & deduplication ────────────────────────────────────────────

fn find_source_jars(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            e.file_type().is_file()
                && name.ends_with("-sources.jar")
                && !name.ends_with("-samples-sources.jar")
        })
        .map(|e| e.into_path())
        .collect()
}

fn select_latest(jars: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut best: HashMap<(String, String), (Vec<VersionPart>, PathBuf)> = HashMap::new();
    let mut ungrouped: Vec<PathBuf> = Vec::new();

    for jar in jars {
        match parse_jar_meta(&jar) {
            None => ungrouped.push(jar),
            Some(meta) => {
                let key = (meta.group, meta.artifact);
                let vk = version_key(&meta.version);
                let entry = best.entry(key).or_insert_with(|| (vk.clone(), jar.clone()));
                if vk > entry.0 {
                    *entry = (vk, jar);
                }
            }
        }
    }

    let mut result: Vec<PathBuf> = best.into_values().map(|(_, p)| p).collect();
    result.extend(ungrouped);
    result
}

// ── extraction ────────────────────────────────────────────────────────────────

/// Extract `.kt`/`.java` files from a JAR into `dest`.
///
/// Returns the count of files extracted (or that would be, when `dry_run`).
fn extract_jar(jar: &Path, dest: &Path, dry_run: bool) -> Result<usize, String> {
    let file = std::fs::File::open(jar).map_err(|e| format!("{}: {e}", jar.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("{}: {e}", jar.display()))?;

    let mut count = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let entry_name = entry.name().to_owned();

        if entry_name.ends_with('/') {
            continue;
        }
        if !entry_name.ends_with(".kt") && !entry_name.ends_with(".java") {
            continue;
        }
        // Zip-slip protection: reject paths with traversal or absolute roots.
        if entry_name.contains("..") || entry_name.starts_with('/') || entry_name.starts_with('\\')
        {
            eprintln!("  WARNING: skipping unsafe path: {entry_name}");
            continue;
        }

        if dry_run {
            count += 1;
            continue;
        }

        let target = dest.join(&entry_name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        std::fs::write(&target, &buf).map_err(|e| e.to_string())?;
        count += 1;
    }
    Ok(count)
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn default_gradle_home() -> PathBuf {
    std::env::var("GRADLE_USER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            crate::util::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".gradle")
        })
}

// ── entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run_extract_sources(opts: ExtractOptions) {
    let search_root = opts
        .gradle_home
        .unwrap_or_else(default_gradle_home)
        .join("caches")
        .join("modules-2")
        .join("files-2.1");

    if !search_root.exists() {
        eprintln!("ERROR: Gradle cache not found at {}", search_root.display());
        eprintln!("Use --gradle-home to specify your Gradle home.");
        std::process::exit(1);
    }

    let output_root = opts.output.unwrap_or_else(|| {
        crate::util::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kotlin-lsp")
            .join("sources")
    });

    println!("Searching: {}", search_root.display());
    println!("Output:    {}", output_root.display());
    if !opts.patterns.is_empty() {
        println!("Patterns:  {}", opts.patterns.join(", "));
    }
    if opts.dry_run {
        println!("Dry run — no files will be written.");
    }
    println!();

    let mut all_jars = find_source_jars(&search_root);
    println!("Found {} *-sources.jar file(s) total.", all_jars.len());

    if !opts.patterns.is_empty() {
        all_jars.retain(|jar| {
            let s = jar.to_string_lossy();
            opts.patterns.iter().any(|p| s.contains(p.as_str()))
        });
        println!(
            "After filtering: {} jar(s) match pattern(s).",
            all_jars.len()
        );
    }

    let selected = select_latest(all_jars);
    println!(
        "After dedup (latest version per artifact): {} jar(s).\n",
        selected.len()
    );

    if selected.is_empty() {
        println!("Nothing to extract.");
        return;
    }

    let mut sorted = selected;
    sorted.sort();

    let mut total_files = 0usize;
    let mut extracted_dirs: Vec<PathBuf> = Vec::new();

    for jar in &sorted {
        let dir_name = artifact_dir_name(jar);
        let dest = output_root.join(&dir_name);
        let version_str = parse_jar_meta(jar)
            .map(|m| format!("  v{}", m.version))
            .unwrap_or_default();

        println!("  {dir_name}{version_str}");
        println!("    src: {}", jar.display());
        println!("    dst: {}", dest.display());

        match extract_jar(jar, &dest, opts.dry_run) {
            Ok(count) => {
                let verb = if opts.dry_run {
                    "would extract"
                } else {
                    "extracted"
                };
                println!("    {verb} {count} file(s)");
                total_files += count;
                if !extracted_dirs.contains(&dest) {
                    extracted_dirs.push(dest);
                }
            }
            Err(e) => eprintln!("  WARNING: {e}"),
        }
        println!();
    }

    let verb = if opts.dry_run {
        "Would extract"
    } else {
        "Extracted"
    };
    println!(
        "{verb} {total_files} file(s) across {} artifact(s).",
        extracted_dirs.len()
    );

    if !opts.dry_run && !extracted_dirs.is_empty() {
        println!();
        println!("Add to your LSP config (sourcePaths accepts a parent dir):");
        println!();
        println!("  sourcePaths = [\"{}\"]", output_root.display());
        println!();
        println!("Or list individual artifact dirs:");
        println!();
        extracted_dirs.sort();
        for d in &extracted_dirs {
            println!("    \"{}\",", d.display());
        }
    }
}
