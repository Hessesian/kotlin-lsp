mod backend;
mod inlay_hints;
mod indexer;
mod lines_ext;
mod parser;
mod queries;
mod resolver;
mod rg;
mod stdlib;
mod stdlib_tail;
mod task_runner;
mod types;

pub(crate) use lines_ext::LinesExt;

use tower_lsp::{LspService, Server};

fn main() {
    // Build custom tokio runtime with larger blocking pool
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(512)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main());
}

async fn async_main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr) // keep stdout clean for LSP JSON-RPC
        .init();

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
        std::sync::Arc::clone(&idx).index_workspace_full(&root, None).await;
        println!("Indexing complete: {} files, {} symbols",
            idx.files.len(), idx.definitions.len());
        std::process::exit(0);
    }

    // --port <N>  — serve a single LSP client over TCP (useful for Android / Sora Editor)
    if args.peek().map(|s| s == "--port").unwrap_or(false) {
        args.next();
        let port: u16 = args.next()
            .unwrap_or_else(|| { eprintln!("Usage: kotlin-lsp --port <port>"); std::process::exit(1); })
            .parse()
            .unwrap_or_else(|_| { eprintln!("Invalid port number"); std::process::exit(1); });

        let addr = format!("127.0.0.1:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
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
            let (service, socket) = LspService::new(backend::Backend::new);
            Server::new(reader, writer, socket).serve(service).await;
            eprintln!("Client disconnected, waiting for next connection…");
        }
    }

    // Default: stdio transport
    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
