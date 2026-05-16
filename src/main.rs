#![warn(unreachable_pub)]

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
mod backend;
mod cli;
mod features;
mod indexer;
mod inlay_hints;
mod language;
mod lines_ext;
mod parser;
mod queries;
mod resolver;
mod rg;
mod semantic_tokens;
mod stdlib;
mod stdlib_tail;
mod str_ext;
mod task_runner;
mod types;
mod util;
mod workspace;
mod workspace_json;

pub(crate) use lines_ext::LinesExt;
pub(crate) use str_ext::StrExt;
pub(crate) use types::Language;

use std::sync::Arc;

use tokio::sync::mpsc;
use tower_lsp::{LspService, Server};

fn main() {
    install_panic_hook();

    // Build custom tokio runtime — scale workers to available cores.
    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_count)
        .max_blocking_threads(512)
        .enable_all()
        .build()
        .unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async_main())
    }));

    match result {
        Ok(()) => {
            // Let runtime drop naturally so in-flight tasks (e.g. cache writes) can finish.
            drop(runtime);
        }
        Err(_) => {
            // Panic hook already printed the crash report to stderr.
            // Exit 101 (Rust's default panic exit) signals to editors that
            // the server crashed and should be restarted.
            std::process::exit(101);
        }
    }
}

// Thread-local flag: when true, the panic hook suppresses the crash report
// because the panic will be caught by `panic_safe`.
std::thread_local! {
    pub(crate) static PANIC_CAUGHT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        // If this panic is being caught by panic_safe, just log briefly.
        if PANIC_CAUGHT.with(|c| c.get()) {
            let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                *s
            } else {
                "panic"
            };
            let location = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "unknown".to_owned());
            eprintln!("[kotlin-lsp] caught panic in handler: {payload} at {location}");
            return;
        }

        // Fatal panic — full crash report.
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_owned()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_owned());

        let backtrace = std::backtrace::Backtrace::force_capture();

        eprintln!("\n╔══════════════════════════════════════════╗");
        eprintln!("║  kotlin-lsp CRASH REPORT                 ║");
        eprintln!("╠══════════════════════════════════════════╣");
        eprintln!("║ panic: {payload}");
        eprintln!("║ location: {location}");
        eprintln!("╠══════════════════════════════════════════╣");
        eprintln!("║ backtrace:");
        for line in backtrace.to_string().lines().take(30) {
            eprintln!("║   {line}");
        }
        eprintln!("╚══════════════════════════════════════════╝");
        eprintln!("The server will exit. Your editor should restart it automatically.");
    }));
}

fn make_backend(client: tower_lsp::Client) -> backend::Backend {
    let indexer = Arc::new(indexer::Indexer::new());
    let (event_tx, event_rx) = mpsc::channel(64);
    let actor = workspace::Actor::new(
        Arc::clone(&indexer),
        Arc::new(backend::LspProgressReporter(client.clone())),
        event_rx,
        Some(client.clone()),
    );
    tokio::spawn(actor.run());
    backend::Backend::new(client, indexer, event_tx)
}

async fn async_main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr) // keep stdout clean for LSP JSON-RPC
        .init();

    // CLI subcommands: find, refs, hover, index
    match cli::CliArgs::parse() {
        Ok(Some(args)) => {
            cli::run(args).await;
            return;
        }
        Ok(None) => {} // LSP mode
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("Usage: kotlin-lsp [find|refs|hover|index] [--fast|--smart] [--json] [--root <dir>]");
            std::process::exit(1);
        }
    }

    let mut args = std::env::args().skip(1).peekable();

    // --index-only <path>  — build cache and exit
    if args.peek().map(|s| s == "--index-only").unwrap_or(false) {
        args.next();
        let path = args.next().unwrap_or_else(|| {
            eprintln!("Usage: kotlin-lsp --index-only <path>");
            std::process::exit(1);
        });
        let pb = std::path::PathBuf::from(path);
        if !pb.is_dir() {
            eprintln!("Path is not a directory: {}", pb.display());
            std::process::exit(1);
        }
        let idx = std::sync::Arc::new(indexer::Indexer::new());
        let root = pb.canonicalize().unwrap_or(pb);
        println!("Indexing workspace: {}", root.display());
        std::sync::Arc::clone(&idx)
            .index_workspace_full(&root, std::sync::Arc::new(indexer::NoopReporter))
            .await;
        println!(
            "Indexing complete: {} files, {} symbols",
            idx.files.len(),
            idx.definitions.len()
        );
        std::process::exit(0);
    }

    // --port <N>  — serve a single LSP client over TCP (useful for Android / Sora Editor)
    if args.peek().map(|s| s == "--port").unwrap_or(false) {
        args.next();
        let port: u16 = args
            .next()
            .unwrap_or_else(|| {
                eprintln!("Usage: kotlin-lsp --port <port>");
                std::process::exit(1);
            })
            .parse()
            .unwrap_or_else(|_| {
                eprintln!("Invalid port number");
                std::process::exit(1);
            });

        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Failed to bind {addr}: {e}");
                std::process::exit(1);
            });
        eprintln!("kotlin-lsp listening on {addr} (TCP, loopback only)");

        // Serve one client at a time; restart the loop for subsequent connections.
        loop {
            let (stream, peer) = listener.accept().await.unwrap_or_else(|e| {
                eprintln!("Accept error: {e}");
                std::process::exit(1);
            });
            eprintln!("Client connected: {peer}");
            let (reader, writer) = tokio::io::split(stream);
            let (service, socket) = LspService::new(make_backend);
            Server::new(reader, writer, socket).serve(service).await;
            eprintln!("Client disconnected, waiting for next connection…");
        }
    }

    // Default: stdio transport
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(make_backend);
    Server::new(stdin, stdout, socket).serve(service).await;
}
