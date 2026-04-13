mod backend;
mod inlay_hints;
mod indexer;
mod parser;
mod queries;
mod resolver;
mod stdlib;
mod stdlib_tail;
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
                let root = pb.canonicalize().unwrap_or(pb);
                println!("Indexing workspace: {}", root.display());
                let root_clone = root.clone();
                let idx_clone = std::sync::Arc::clone(&idx);
                // Already inside #[tokio::main] runtime — await directly.
                idx.index_workspace_full(&root_clone, None).await;
                
                // index_workspace_full returns immediately (non-blocking parse).
                // The indexing_in_progress flag clears when index_workspace_impl returns,
                // but background parse tasks continue running. Poll definitions count instead.
                println!("Waiting for background parse tasks to complete...");
                let mut last_count = 0;
                let mut stable_iterations = 0;
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                    let count = idx_clone.definitions.len();
                    if count == last_count {
                        stable_iterations += 1;
                        if stable_iterations >= 30 {
                            // Symbol count stable for 30 seconds, assume done
                            break;
                        }
                    } else {
                        stable_iterations = 0;
                        if count % 100 < last_count % 100 || count - last_count >= 100 {
                            println!("Indexing... ({} symbols so far)", count);
                        }
                    }
                    last_count = count;
                }
                
                // Give background tasks time to finalize
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                
                // Save cache now that parsing is complete
                idx_clone.save_cache_to_disk();
                
                println!("Indexing complete: {} symbols", idx_clone.definitions.len());
                std::process::exit(0);
            }
        }
    }

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
