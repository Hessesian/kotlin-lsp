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

    let (service, socket) = LspService::new(backend::Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
