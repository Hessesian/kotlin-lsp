mod backend;
mod inlay_hints;
mod indexer;
mod parser;
mod queries;
mod resolver;
mod stdlib;
mod types;

use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
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
                // Build a headless client stub: use tower_lsp::Client::new is internal,
                // so call indexer.index_workspace_full with a dummy client via channel.
                let idx = std::sync::Arc::new(indexer::Indexer::new());
                // Create a real tokio runtime handle and a simple Client via LspService is heavy.
                // Instead, call the blocking save_cache_to_disk after indexing to persist.
                let rt = tokio::runtime::Runtime::new().unwrap();
                let root = pb.canonicalize().unwrap_or(pb);
                println!("Indexing workspace: {}", root.display());
                rt.block_on(async move {
                    // Use a dummy tower_lsp::Client from LspService by spinning up a minimal service
                    // but simpler: call index_workspace_full with a fake Client created from a channel.
                    // For minimal effect, create a real LspService but don't attach stdio: use a pipe pair.
                    let (service, socket) = tower_lsp::LspService::new(backend::Backend::new);
                    // Spawn server in background to obtain a Client
                    let (stdin_r, stdin_w) = tokio::io::duplex(64);
                    let (stdout_r, stdout_w) = tokio::io::duplex(64);
                    let server = Server::new(stdin_r, stdout_w, socket);
                    let svc_handle = tokio::spawn(async move { server.serve(service).await });
                    // Create a real client from the service handle
                    // (LspService::new returned service factory; grabbing client is non-trivial).
                    // Fallback: create a Client via tower_lsp::Client::new won't compile (private).
                    // So call index_workspace directly and then save cache.
                    idx.index_workspace_full(&root, None).await;
                    // Persist cache (already done by indexer) and shutdown helper
                    svc_handle.abort();
                });
                println!("Indexing complete: {}", root.display());
                std::process::exit(0);
            }
        }
    }

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
