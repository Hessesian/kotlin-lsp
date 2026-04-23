mod backend;
mod inlay_hints;
mod indexer;
mod parser;
mod queries;
mod resolver;
mod rg;
mod stdlib;
mod stdlib_tail;
mod task_runner;
mod types;

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

    let stdin  = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // Support a one-shot indexer mode: `--index-only <path>`
    let mut args = std::env::args().skip(1);
    if let Some(flag) = args.next() {
        if flag == "--index-only" {
            if let Some(path) = args.next() {
                let pb = std::path::PathBuf::from(path);
                if !pb.is_dir() {
                    eprintln!("Path is not a directory: {}", pb.display());
                    std::process::exit(1);
                }
                let idx = std::sync::Arc::new(indexer::Indexer::new());
                let root = pb.canonicalize().unwrap_or(pb);
                println!("Indexing workspace: {}", root.display());
                
                // index_workspace_full now blocks until ALL parsing completes
                // and returns explicit results — no poll loop needed!
                std::sync::Arc::clone(&idx).index_workspace_full(&root, None).await;
                
                println!("Indexing complete: {} files, {} symbols",
                    idx.files.len(), idx.definitions.len());
                std::process::exit(0);
            }
        }
    }

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
