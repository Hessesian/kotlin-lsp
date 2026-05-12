//! CLI argument parsing via lexopt.

use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum Subcommand {
    Find {
        name: String,
    },
    Refs {
        name: String,
    },
    Hover {
        file: PathBuf,
        line: u32,
        col: u32,
    },
    /// Show completion candidates at a file position (debug).
    Complete {
        file: PathBuf,
        line: u32,
        /// 1-based UTF-16 column. `None` when resolved from `--dot` or `--eol`.
        col: Option<u32>,
        /// Resolve column to just after the last `.` on the line.
        dot: bool,
        /// Resolve column to end of trimmed content on the line (bare-word prefix).
        eol: bool,
        /// Skip loading `~/.kotlin-lsp/sources` (extracted stdlib/libraries).
        /// Returns only workspace symbols. Much faster (~2s vs ~10s).
        no_stdlib: bool,
    },
    Index,
    /// Dump semantic tokens for a file (debug).
    Tokens {
        file: PathBuf,
        /// Use CST classification only; skip cross-file index resolution (default).
        cst_only: bool,
        /// Opt-in to Phase 2 cross-file resolution (loads full index).
        resolve: bool,
        /// Show per-phase token breakdown before dedup.
        phases: bool,
        /// Also print the tree-sitter parse tree after tokens.
        show_tree: bool,
    },
    /// Dump the tree-sitter parse tree for a file (debug).
    Tree {
        file: PathBuf,
    },
    /// List resolved source roots for the workspace.
    Sources,
    /// Extract Gradle *-sources.jar files to a sourcePaths-ready directory.
    ExtractSources {
        gradle_home: Option<PathBuf>,
        output: Option<PathBuf>,
        dry_run: bool,
        patterns: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Load cache when available; fall back to rg/fd otherwise.
    Auto,
    /// Always use rg/fd; never load index.
    Fast,
    /// Require a warm cache; exit with error if missing.
    Smart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFmt {
    Text,
    Json,
}

#[derive(Debug)]
pub(crate) struct CliArgs {
    pub subcommand: Subcommand,
    pub mode: Mode,
    pub fmt: OutputFmt,
    pub root: Option<PathBuf>,
    pub verbose: bool,
}

impl CliArgs {
    pub(crate) fn parse() -> Result<Option<Self>, String> {
        let mut args = lexopt::Parser::from_env();
        let Some(first) = parse_first_argument(&mut args)? else {
            return Ok(None);
        };
        let Some(subcommand) = parse_subcommand_name(first)? else {
            return Ok(None);
        };
        let parsed = parse_cli_flags(&mut args)?;
        let mode = parsed.mode;
        let fmt = parsed.fmt;
        let root = parsed.root.clone();
        let verbose = parsed.verbose;
        let subcommand = build_subcommand(&subcommand, parsed)?;
        Ok(Some(Self {
            subcommand,
            mode,
            fmt,
            root,
            verbose,
        }))
    }
}

struct ParsedCliFlags {
    mode: Mode,
    fmt: OutputFmt,
    root: Option<PathBuf>,
    positionals: Vec<String>,
    cst_only: bool,
    resolve: bool,
    phases: bool,
    show_tree: bool,
    verbose: bool,
    gradle_home: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    dry_run: bool,
    dot: bool,
    eol: bool,
    no_stdlib: bool,
}

fn parse_first_argument(args: &mut lexopt::Parser) -> Result<Option<std::ffi::OsString>, String> {
    match args.next().map_err(|e| e.to_string())? {
        None => Ok(None),
        Some(lexopt::Arg::Value(value)) => Ok(Some(value)),
        Some(lexopt::Arg::Short('h') | lexopt::Arg::Long("help")) => {
            print_help();
            std::process::exit(0);
        }
        Some(lexopt::Arg::Short('V') | lexopt::Arg::Long("version")) => {
            print_version();
            std::process::exit(0);
        }
        Some(lexopt::Arg::Long(flag)) if is_subcommand(flag) => Err(format!(
            "'{flag}' is a subcommand, not a flag — use `kotlin-lsp {flag}` (without --)"
        )),
        Some(lexopt::Arg::Short(_) | lexopt::Arg::Long(_)) => Ok(None),
    }
}

fn parse_subcommand_name(first: std::ffi::OsString) -> Result<Option<String>, String> {
    let subcommand = first.to_string_lossy().into_owned();
    if is_subcommand(&subcommand) {
        Ok(Some(subcommand))
    } else {
        Ok(None)
    }
}

fn parse_cli_flags(args: &mut lexopt::Parser) -> Result<ParsedCliFlags, String> {
    let mut parsed = ParsedCliFlags {
        mode: Mode::Auto,
        fmt: OutputFmt::Text,
        root: None,
        positionals: Vec::new(),
        cst_only: false,
        resolve: false,
        phases: false,
        show_tree: false,
        verbose: false,
        gradle_home: None,
        output_dir: None,
        dry_run: false,
        dot: false,
        eol: false,
        no_stdlib: false,
    };

    loop {
        match args.next().map_err(|e| e.to_string())? {
            None => return Ok(parsed),
            Some(lexopt::Arg::Long("fast")) => parsed.mode = Mode::Fast,
            Some(lexopt::Arg::Long("smart")) => parsed.mode = Mode::Smart,
            Some(lexopt::Arg::Long("json")) => parsed.fmt = OutputFmt::Json,
            Some(lexopt::Arg::Long("cst-only")) => parsed.cst_only = true,
            Some(lexopt::Arg::Long("resolve")) => parsed.resolve = true,
            Some(lexopt::Arg::Long("phases")) => parsed.phases = true,
            Some(lexopt::Arg::Long("tree")) => parsed.show_tree = true,
            Some(lexopt::Arg::Short('v') | lexopt::Arg::Long("verbose")) => parsed.verbose = true,
            Some(lexopt::Arg::Long("root")) => {
                let value = args.value().map_err(|e| e.to_string())?;
                parsed.root = Some(PathBuf::from(value.to_string_lossy().as_ref()));
            }
            Some(lexopt::Arg::Long("gradle-home")) => {
                let value = args.value().map_err(|e| e.to_string())?;
                parsed.gradle_home = Some(PathBuf::from(value.to_string_lossy().as_ref()));
            }
            Some(lexopt::Arg::Long("output")) => {
                let value = args.value().map_err(|e| e.to_string())?;
                parsed.output_dir = Some(PathBuf::from(value.to_string_lossy().as_ref()));
            }
            Some(lexopt::Arg::Long("dry-run")) => parsed.dry_run = true,
            Some(lexopt::Arg::Short('d') | lexopt::Arg::Long("dot")) => parsed.dot = true,
            Some(lexopt::Arg::Short('e') | lexopt::Arg::Long("eol")) => parsed.eol = true,
            Some(lexopt::Arg::Long("no-stdlib")) => parsed.no_stdlib = true,
            Some(lexopt::Arg::Short('h') | lexopt::Arg::Long("help")) => {
                print_help();
                std::process::exit(0);
            }
            Some(lexopt::Arg::Short('V') | lexopt::Arg::Long("version")) => {
                print_version();
                std::process::exit(0);
            }
            Some(lexopt::Arg::Value(value)) => parsed
                .positionals
                .push(value.to_string_lossy().into_owned()),
            Some(lexopt::Arg::Short(flag)) => return Err(format!("Unknown short flag: -{flag}")),
            Some(lexopt::Arg::Long(flag)) => return Err(format!("Unknown flag: --{flag}")),
        }
    }
}

fn build_subcommand(subcommand: &str, parsed: ParsedCliFlags) -> Result<Subcommand, String> {
    let ParsedCliFlags {
        positionals,
        cst_only,
        resolve,
        phases,
        show_tree,
        gradle_home,
        output_dir,
        dry_run,
        dot,
        eol,
        no_stdlib,
        ..
    } = parsed;
    match subcommand {
        "find" => Ok(Subcommand::Find {
            name: first_positional(positionals, "find requires a NAME argument")?,
        }),
        "refs" => Ok(Subcommand::Refs {
            name: first_positional(positionals, "refs requires a NAME argument")?,
        }),
        "hover" => build_hover_subcommand(positionals),
        "complete" => build_complete_subcommand(positionals, dot, eol, no_stdlib),
        "index" => Ok(Subcommand::Index),
        "tokens" => Ok(Subcommand::Tokens {
            file: PathBuf::from(first_positional(
                positionals,
                "tokens requires a FILE argument",
            )?),
            cst_only,
            resolve,
            phases,
            show_tree,
        }),
        "tree" => Ok(Subcommand::Tree {
            file: PathBuf::from(first_positional(
                positionals,
                "tree requires a FILE argument",
            )?),
        }),
        "sources" => Ok(Subcommand::Sources),
        "extract-sources" => Ok(Subcommand::ExtractSources {
            gradle_home,
            output: output_dir,
            dry_run,
            patterns: positionals,
        }),
        _ => unreachable!(),
    }
}

fn build_hover_subcommand(positionals: Vec<String>) -> Result<Subcommand, String> {
    let (file, line, col) = parse_file_line_col(positionals, "hover")?;
    Ok(Subcommand::Hover { file, line, col })
}

fn build_complete_subcommand(
    positionals: Vec<String>,
    dot: bool,
    eol: bool,
    no_stdlib: bool,
) -> Result<Subcommand, String> {
    let mut iter = positionals.into_iter();
    let file = PathBuf::from(iter.next().ok_or("complete requires a FILE argument")?);
    let line = iter
        .next()
        .ok_or("complete requires a LINE argument")?
        .parse::<u32>()
        .map_err(|_| "LINE must be a positive integer".to_string())?;
    if line == 0 {
        return Err("LINE must be >= 1 (positions are 1-based)".to_string());
    }
    if dot && eol {
        return Err("--dot and --eol are mutually exclusive".to_string());
    }
    // col is optional when --dot or --eol is given
    let col = match iter.next() {
        Some(s) => {
            let c = s
                .parse::<u32>()
                .map_err(|_| "COL must be a positive integer".to_string())?;
            if c == 0 {
                return Err("COL must be >= 1 (positions are 1-based)".to_string());
            }
            Some(c)
        }
        None => {
            if !dot && !eol {
                return Err("complete requires a COL argument (or use --dot / --eol)".to_string());
            }
            None
        }
    };
    Ok(Subcommand::Complete {
        file,
        line,
        col,
        dot,
        eol,
        no_stdlib,
    })
}

fn parse_file_line_col(
    positionals: Vec<String>,
    name: &'static str,
) -> Result<(PathBuf, u32, u32), String> {
    let mut iter = positionals.into_iter();
    let file = PathBuf::from(
        iter.next()
            .ok_or_else(|| format!("{name} requires FILE LINE COL arguments"))?,
    );
    let line = iter
        .next()
        .ok_or_else(|| format!("{name} requires LINE argument"))?
        .parse::<u32>()
        .map_err(|_| "LINE must be a positive integer".to_string())?;
    if line == 0 {
        return Err("LINE must be >= 1 (positions are 1-based)".to_string());
    }
    let col = iter
        .next()
        .ok_or_else(|| format!("{name} requires COL argument"))?
        .parse::<u32>()
        .map_err(|_| "COL must be a positive integer".to_string())?;
    if col == 0 {
        return Err("COL must be >= 1 (positions are 1-based)".to_string());
    }
    Ok((file, line, col))
}

fn first_positional(
    positionals: Vec<String>,
    missing_message: &'static str,
) -> Result<String, String> {
    positionals
        .into_iter()
        .next()
        .ok_or_else(|| missing_message.to_string())
}

fn is_subcommand(value: &str) -> bool {
    matches!(
        value,
        "find"
            | "refs"
            | "hover"
            | "complete"
            | "index"
            | "tokens"
            | "tree"
            | "sources"
            | "extract-sources"
    )
}

fn print_version() {
    println!("kotlin-lsp {}", env!("CARGO_PKG_VERSION"));
}

fn print_help() {
    println!(
        "kotlin-lsp {} — Kotlin/Java symbol navigation

USAGE:
    kotlin-lsp <SUBCOMMAND> [OPTIONS] [ARGS]
    kotlin-lsp                            # start LSP server (stdio)

SUBCOMMANDS:
    find    <name>              Find declarations of a symbol
    refs    <name>              Find all references to a symbol
    hover   <file> <line> <col> Show type/doc info at a position
    complete <file> <line> [col] Show completion candidates at a position
    index                       Build and cache the workspace index
    sources                     List resolved source roots
    extract-sources [PATTERN…]  Extract Gradle *-sources.jar to sourcePaths dir
    tokens  <file>              Dump semantic tokens (debug)
    tree    <file>              Dump tree-sitter parse tree (debug)

OPTIONS:
    --fast              Use rg/fd only; never load index (default when no cache)
    --smart             Require index; build it if missing
    --json              Output results as JSON array
    --root <dir>        Workspace root (default: nearest .git dir or cwd)
    --resolve           (tokens) Load index for Phase 2 cross-file resolution
    --cst-only          (tokens) Force CST-only mode (default, kept for clarity)
    --phases            (tokens) Show per-phase token breakdown with dedup markers
    --tree              (tokens) Also print the parse tree after tokens
    --gradle-home <dir> (extract-sources) Gradle home (default: $GRADLE_USER_HOME or ~/.gradle)
    --output <dir>      (extract-sources) Output root (default: ~/.kotlin-lsp/sources)
    --dry-run           (extract-sources) Print what would be extracted; write nothing
    -d, --dot           (complete) Resolve col to just after the last '.' on the line
    -e, --eol           (complete) Resolve col to end of trimmed content on the line
    --no-stdlib         (complete) Skip ~/.kotlin-lsp/sources; workspace symbols only (~2s)
    -v, --verbose       Show progress messages (indexing, cache status)
    -h, --help          Print this help
    -V, --version       Print version

EXAMPLES:
    kotlin-lsp find MyViewModel
    kotlin-lsp refs --fast MyViewModel --root ./android
    kotlin-lsp hover src/Foo.kt 42 10 --json
    kotlin-lsp complete src/Foo.kt 42 10
    kotlin-lsp complete src/Foo.kt 42 10 --json
    kotlin-lsp complete src/Foo.kt 42 --dot --json
    kotlin-lsp complete src/Foo.kt 42 --eol --json
    kotlin-lsp complete src/Foo.kt 42 --dot --no-stdlib --json
    kotlin-lsp index --root ./android
    kotlin-lsp sources --root ./android
    kotlin-lsp sources --json
    kotlin-lsp extract-sources
    kotlin-lsp extract-sources androidx.compose org.jetbrains.kotlin
    kotlin-lsp extract-sources --dry-run
    kotlin-lsp extract-sources --output ~/my-sources androidx.compose
    kotlin-lsp tokens src/Foo.kt
    kotlin-lsp tokens --resolve src/Foo.kt
    kotlin-lsp tokens src/Foo.kt --tree
    kotlin-lsp tree src/Foo.kt",
        env!("CARGO_PKG_VERSION")
    );
}
