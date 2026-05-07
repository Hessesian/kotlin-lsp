//! CLI argument parsing via lexopt.

use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum Subcommand {
    Find { name: String },
    Refs { name: String },
    Hover { file: PathBuf, line: u32, col: u32 },
    Index,
    /// Dump semantic tokens for a file (debug).
    Tokens {
        file: PathBuf,
        /// Use CST classification only; skip cross-file index resolution.
        cst_only: bool,
        /// Also print the tree-sitter parse tree after tokens.
        show_tree: bool,
    },
    /// Dump the tree-sitter parse tree for a file (debug).
    Tree { file: PathBuf },
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
}

impl CliArgs {
    pub(crate) fn parse() -> Result<Option<Self>, String> {
        let mut args = lexopt::Parser::from_env();

        // Peek at first positional to decide if this is a CLI subcommand
        // or an LSP invocation.  LSP mode has no positional args (just flags
        // like --port or --index-only).
        let first = match args.next().map_err(|e| e.to_string())? {
            None => return Ok(None), // no args → LSP stdio mode
            Some(lexopt::Arg::Value(v)) => v,
            Some(lexopt::Arg::Short('h') | lexopt::Arg::Long("help")) => {
                print_help();
                std::process::exit(0);
            }
            Some(lexopt::Arg::Short('V') | lexopt::Arg::Long("version")) => {
                print_version();
                std::process::exit(0);
            }
            Some(lexopt::Arg::Long(flag))
                if matches!(flag, "find" | "refs" | "hover" | "index" | "tokens" | "tree") =>
            {
                return Err(format!(
                    "'{flag}' is a subcommand, not a flag — use `kotlin-lsp {flag}` (without --)"
                ));
            }
            Some(lexopt::Arg::Short(_) | lexopt::Arg::Long(_)) => {
                // Other flag before subcommand → LSP mode
                return Ok(None);
            }        };

        let subcmd_str = first.to_string_lossy();
        let subcommand = match subcmd_str.as_ref() {
            "find" | "refs" | "hover" | "index" | "tokens" | "tree" => subcmd_str.into_owned(),
            _ => return Ok(None), // unknown first positional → LSP mode
        };

        let mut mode = Mode::Auto;
        let mut fmt = OutputFmt::Text;
        let mut root: Option<PathBuf> = None;
        let mut positionals: Vec<String> = Vec::new();
        let mut cst_only = false;
        let mut show_tree = false;

        loop {
            match args.next().map_err(|e| e.to_string())? {
                None => break,
                Some(lexopt::Arg::Long("fast")) => mode = Mode::Fast,
                Some(lexopt::Arg::Long("smart")) => mode = Mode::Smart,
                Some(lexopt::Arg::Long("json")) => fmt = OutputFmt::Json,
                Some(lexopt::Arg::Long("cst-only")) => cst_only = true,
                Some(lexopt::Arg::Long("tree")) => show_tree = true,
                Some(lexopt::Arg::Long("root")) => {
                    let val = args.value().map_err(|e| e.to_string())?;
                    root = Some(PathBuf::from(val.to_string_lossy().as_ref()));
                }
                Some(lexopt::Arg::Short('h') | lexopt::Arg::Long("help")) => {
                    print_help();
                    std::process::exit(0);
                }
                Some(lexopt::Arg::Short('V') | lexopt::Arg::Long("version")) => {
                    print_version();
                    std::process::exit(0);
                }
                Some(lexopt::Arg::Value(v)) => positionals.push(v.to_string_lossy().into_owned()),
                Some(lexopt::Arg::Short(c)) => {
                    return Err(format!("Unknown short flag: -{c}"));
                }
                Some(lexopt::Arg::Long(l)) => {
                    return Err(format!("Unknown flag: --{l}"));
                }
            }
        }

        let sub = match subcommand.as_str() {
            "find" => {
                let name = positionals
                    .into_iter()
                    .next()
                    .ok_or("find requires a NAME argument")?;
                Subcommand::Find { name }
            }
            "refs" => {
                let name = positionals
                    .into_iter()
                    .next()
                    .ok_or("refs requires a NAME argument")?;
                Subcommand::Refs { name }
            }
            "hover" => {
                let mut it = positionals.into_iter();
                let file = PathBuf::from(
                    it.next().ok_or("hover requires FILE LINE COL arguments")?,
                );
                let line: u32 = it
                    .next()
                    .ok_or("hover requires LINE argument")?
                    .parse()
                    .map_err(|_| "LINE must be a positive integer")?;
                let col: u32 = it
                    .next()
                    .ok_or("hover requires COL argument")?
                    .parse()
                    .map_err(|_| "COL must be a positive integer")?;
                Subcommand::Hover { file, line, col }
            }
            "index" => Subcommand::Index,
            "tokens" => {
                let file = PathBuf::from(
                    positionals
                        .into_iter()
                        .next()
                        .ok_or("tokens requires a FILE argument")?,
                );
                Subcommand::Tokens { file, cst_only, show_tree }
            }
            "tree" => {
                let file = PathBuf::from(
                    positionals
                        .into_iter()
                        .next()
                        .ok_or("tree requires a FILE argument")?,
                );
                Subcommand::Tree { file }
            }
            _ => unreachable!(),
        };

        Ok(Some(Self {
            subcommand: sub,
            mode,
            fmt,
            root,
        }))
    }
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
    find   <name>              Find declarations of a symbol
    refs   <name>              Find all references to a symbol
    hover  <file> <line> <col> Show type/doc info at a position
    index                      Build and cache the workspace index
    tokens <file>              Dump semantic tokens (debug)
    tree   <file>              Dump tree-sitter parse tree (debug)

OPTIONS:
    --fast          Use rg/fd only; never load index (default when no cache)
    --smart         Require index; build it if missing
    --json          Output results as JSON array
    --root <dir>    Workspace root (default: nearest .git dir or cwd)
    --cst-only      (tokens) Skip index; CST classification only
    --tree          (tokens) Also print the parse tree after tokens
    -h, --help      Print this help
    -V, --version   Print version

EXAMPLES:
    kotlin-lsp find MyViewModel
    kotlin-lsp refs --fast MyViewModel --root ./android
    kotlin-lsp hover src/Foo.kt 42 10 --json
    kotlin-lsp index --root ./android
    kotlin-lsp tokens --cst-only src/Foo.kt
    kotlin-lsp tokens src/Foo.kt --tree
    kotlin-lsp tree src/Foo.kt",
        env!("CARGO_PKG_VERSION")
    );
}
