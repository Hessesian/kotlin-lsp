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
    /// List auto-discovered source roots for the workspace.
    Sources,
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
        let subcommand = build_subcommand(
            &subcommand,
            parsed.positionals,
            parsed.cst_only,
            parsed.resolve,
            parsed.phases,
            parsed.show_tree,
        )?;
        Ok(Some(Self {
            subcommand,
            mode: parsed.mode,
            fmt: parsed.fmt,
            root: parsed.root,
            verbose: parsed.verbose,
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

fn build_subcommand(
    subcommand: &str,
    positionals: Vec<String>,
    cst_only: bool,
    resolve: bool,
    phases: bool,
    show_tree: bool,
) -> Result<Subcommand, String> {
    match subcommand {
        "find" => Ok(Subcommand::Find {
            name: first_positional(positionals, "find requires a NAME argument")?,
        }),
        "refs" => Ok(Subcommand::Refs {
            name: first_positional(positionals, "refs requires a NAME argument")?,
        }),
        "hover" => build_hover_subcommand(positionals),
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
        _ => unreachable!(),
    }
}

fn build_hover_subcommand(positionals: Vec<String>) -> Result<Subcommand, String> {
    let mut positionals = positionals.into_iter();
    let file = PathBuf::from(
        positionals
            .next()
            .ok_or("hover requires FILE LINE COL arguments")?,
    );
    let line = parse_position_arg(
        positionals.next(),
        "hover requires LINE argument",
        "LINE must be a positive integer",
    )?;
    let col = parse_position_arg(
        positionals.next(),
        "hover requires COL argument",
        "COL must be a positive integer",
    )?;
    Ok(Subcommand::Hover { file, line, col })
}

fn parse_position_arg(
    value: Option<String>,
    missing_message: &'static str,
    invalid_message: &'static str,
) -> Result<u32, String> {
    value
        .ok_or(missing_message)?
        .parse()
        .map_err(|_| invalid_message.to_string())
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
        "find" | "refs" | "hover" | "index" | "tokens" | "tree" | "sources"
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
    index                       Build and cache the workspace index
    sources                     List auto-discovered source roots
    tokens  <file>              Dump semantic tokens (debug)
    tree    <file>              Dump tree-sitter parse tree (debug)

OPTIONS:
    --fast          Use rg/fd only; never load index (default when no cache)
    --smart         Require index; build it if missing
    --json          Output results as JSON array
    --root <dir>    Workspace root (default: nearest .git dir or cwd)
    --resolve       (tokens) Load index for Phase 2 cross-file resolution
    --cst-only      (tokens) Force CST-only mode (default, kept for clarity)
    --phases        (tokens) Show per-phase token breakdown with dedup markers
    --tree          (tokens) Also print the parse tree after tokens
    -v, --verbose   Show progress messages (indexing, cache status)
    -h, --help      Print this help
    -V, --version   Print version

EXAMPLES:
    kotlin-lsp find MyViewModel
    kotlin-lsp refs --fast MyViewModel --root ./android
    kotlin-lsp hover src/Foo.kt 42 10 --json
    kotlin-lsp index --root ./android
    kotlin-lsp sources --root ./android
    kotlin-lsp sources --json
    kotlin-lsp tokens src/Foo.kt
    kotlin-lsp tokens --resolve src/Foo.kt
    kotlin-lsp tokens src/Foo.kt --tree
    kotlin-lsp tree src/Foo.kt",
        env!("CARGO_PKG_VERSION")
    );
}
