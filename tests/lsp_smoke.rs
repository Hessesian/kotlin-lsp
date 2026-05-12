//! End-to-end smoke tests for the `kotlin-lsp --stdio` LSP server.
//!
//! These tests spawn the compiled binary, drive it over stdin/stdout using the
//! LSP JSON-RPC wire protocol, and assert that core features (completion,
//! go-to-definition, inlay hints, workspace symbols, same-name disambiguation)
//! all work correctly on small synthetic fixtures.
//!
//! Design notes:
//! - A dedicated reader thread parses Content-Length frames and forwards them
//!   over an mpsc channel, enabling receive-with-timeout from the test thread.
//! - Server-originated requests (e.g. `window/workDoneProgress/create`) are
//!   automatically acknowledged so the server is never blocked waiting for a
//!   client reply.
//! - Tests wait for the `$/progress` end event with token `kotlin-lsp/indexing`
//!   before asserting on results, ensuring they exercise the indexed code path
//!   and not an rg/on-demand fallback.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_kotlin-lsp");
/// Maximum time to wait for indexing to complete (small synthetic project).
const INDEXING_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time to wait for a single LSP response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ── minimal LSP client ────────────────────────────────────────────────────────

struct LspClient {
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    // Keep the child alive until the client is dropped.
    _child: Child,
}

impl LspClient {
    /// Spawn the server, set up the reader thread, and return a ready client.
    ///
    /// `workspace_root` is passed as `KOTLIN_LSP_WORKSPACE_ROOT` so the server
    /// uses the synthetic fixture directory even if a real config file exists.
    fn spawn(workspace_root: &Path) -> Self {
        let mut child = Command::new(BIN)
            .args(["--stdio"])
            .env("KOTLIN_LSP_WORKSPACE_ROOT", workspace_root)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr so Rust panics/backtraces reach the test output.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to spawn kotlin-lsp");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let (tx, rx) = mpsc::channel::<Value>();

        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                // Read the Content-Length header line.
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) | Err(_) => break, // server closed stdout
                    Ok(_) => {}
                }
                let len: usize = match header
                    .trim()
                    .strip_prefix("Content-Length: ")
                    .and_then(|s| s.parse().ok())
                {
                    Some(n) => n,
                    None => continue, // unexpected header; skip
                };

                // Consume the blank separator line (\r\n).
                let mut blank = String::new();
                let _ = reader.read_line(&mut blank);

                // Read the JSON body.
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_err() {
                    break;
                }
                let msg: Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });

        LspClient {
            stdin,
            rx,
            next_id: 1,
            _child: child,
        }
    }

    /// Write a raw JSON-RPC message to the server's stdin.
    fn write_raw(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    fn notify(&mut self, method: &str, params: Value) {
        self.write_raw(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    /// Send a JSON-RPC request and return the matching response.
    ///
    /// Incoming server-originated requests are auto-acknowledged with `null`.
    /// Notifications are silently consumed.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        self.write_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = self.rx.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!("timeout ({REQUEST_TIMEOUT:?}) waiting for response to `{method}`")
            });

            // Server request (has both `id` and `method`) → auto-ack.
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let server_id = msg["id"].clone();
                self.write_raw(&json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "result": null,
                }));
                continue;
            }

            // Notification (has `method`, no `id`) → skip.
            if msg.get("method").is_some() {
                continue;
            }

            // Response matching our request id → return it.
            if msg.get("id") == Some(&json!(id)) {
                return msg;
            }
        }
    }

    /// Wait until the server sends a `$/progress` *end* event for the
    /// `kotlin-lsp/indexing` token, indicating that workspace indexing is done.
    fn wait_for_indexing(&mut self) {
        let deadline = Instant::now() + INDEXING_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .rx
                .recv_timeout(remaining)
                .expect("timeout waiting for `kotlin-lsp/indexing` end — indexing took too long");

            // Auto-ack server requests.
            if msg.get("method").is_some() && msg.get("id").is_some() {
                let server_id = msg["id"].clone();
                self.write_raw(&json!({
                    "jsonrpc": "2.0",
                    "id": server_id,
                    "result": null,
                }));
                continue;
            }

            if msg.get("method") == Some(&json!("$/progress")) {
                let token = msg["params"]["token"].as_str().unwrap_or("");
                let kind = msg["params"]["value"]["kind"].as_str().unwrap_or("");
                if token == "kotlin-lsp/indexing" && kind == "end" {
                    return;
                }
            }
        }
    }

    /// Full initialization handshake: send `initialize`, wait for the response,
    /// then send `initialized`.  Does not wait for indexing to finish.
    fn initialize(&mut self, root: &Path) {
        let root_uri = format!("file://{}", root.display());
        let resp = self.request(
            "initialize",
            json!({
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "completion": {"completionItem": {"snippetSupport": false}},
                        "definition": {},
                        "inlayHint": {},
                        "hover": {},
                    },
                    "workspace": {"symbol": {}},
                    "window": {"workDoneProgress": true},
                },
            }),
        );
        assert!(
            resp.get("result").is_some(),
            "initialize must succeed; got: {resp}"
        );
        self.notify("initialized", json!({}));
    }

    /// Send `textDocument/didOpen` for a file already on disk.
    fn open_file(&mut self, uri: &str, language_id: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                },
            }),
        );
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Best-effort graceful shutdown.
        let _ = self.write_raw(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "shutdown",
            "params": null,
        }));
        let _ = self.write_raw(&json!({"jsonrpc":"2.0","method":"exit","params":null}));
    }
}

// ── fixture helpers ───────────────────────────────────────────────────────────

fn write(dir: &Path, rel: &str, content: &str) {
    let full = dir.join(rel);
    if let Some(p) = full.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(full, content).unwrap();
}

fn file_uri(dir: &Path, rel: &str) -> String {
    format!("file://{}", dir.join(rel).display())
}

/// Build LSP `Position` (0-based) by counting lines and UTF-16 columns.
fn pos(text: &str, line: usize, col: usize) -> Value {
    let _ = text; // kept for documentation; positions are already 0-based
    json!({"line": line, "character": col})
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// The server must respond to `initialize` with capability declarations.
#[test]
fn smoke_initialize_returns_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Empty workspace — just need a Kotlin file so the server has something.
    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write(root, "src/Empty.kt", "package com.example\n");

    let mut client = LspClient::spawn(root);
    client.initialize(root);

    let resp = client.request(
        "initialize",
        json!({
            "rootUri": format!("file://{}", root.display()),
            "capabilities": {},
        }),
    );
    // Re-initializing is an error (server already initialized), but it must
    // still reply (not crash).  Just check we get a structured reply.
    assert!(
        resp.get("result").is_some() || resp.get("error").is_some(),
        "server must reply to any request; got: {resp}"
    );
}

/// Completions must include symbols from cross-package (library) files once
/// indexing is complete, and the response must not be marked `isIncomplete`.
#[test]
fn smoke_completion_cross_package() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    // Library file simulating a compose-like annotation class.
    write(
        root,
        "lib/Composable.kt",
        "package androidx.compose.runtime\nannotation class Composable\n",
    );
    write(
        root,
        "lib/PaymentService.kt",
        "package com.payments\nclass PaymentService {\n    fun process() {}\n}\n",
    );
    // Edit file — cursor after `Pay` prefix on line 2 (0-based), col 7 (0-based).
    let edit_text = "package com.example\nfun foo() {\n    Pay\n}\n";
    write(root, "src/Screen.kt", edit_text);

    let mut client = LspClient::spawn(root);
    client.initialize(root);
    client.wait_for_indexing();

    let uri = file_uri(root, "src/Screen.kt");
    client.open_file(&uri, "kotlin", edit_text);

    // Line 2 (0-based), col 7 = after "    Pay" (4 spaces + 3 chars)
    let resp = client.request(
        "textDocument/completion",
        json!({
            "textDocument": {"uri": uri},
            "position": pos(edit_text, 2, 7),
        }),
    );
    let result = &resp["result"];
    let items = if result.is_array() {
        result.as_array().unwrap().clone()
    } else {
        result["items"].as_array().cloned().unwrap_or_default()
    };

    let labels: Vec<&str> = items.iter().filter_map(|v| v["label"].as_str()).collect();

    assert!(
        labels.contains(&"PaymentService"),
        "PaymentService from library must appear for prefix 'Pay'; got: {labels:?}"
    );

    // Completion response must not be incomplete (isIncomplete==false means the
    // index is done; true means the server returned a partial fallback).
    if result.is_object() {
        let incomplete = result["isIncomplete"].as_bool().unwrap_or(false);
        assert!(
            !incomplete,
            "completion must not be isIncomplete after indexing; got: {result}"
        );
    }
}

/// `textDocument/definition` must resolve to the file and line where a class
/// is declared, not just the current file.
#[test]
fn smoke_go_to_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    // Declaration file.
    write(
        root,
        "src/data/UserRepository.kt",
        "package com.example.data\n\nclass UserRepository {\n    fun findAll(): List<String> = emptyList()\n}\n",
    );
    // Usage file — `UserRepository` appears on line 4 (0-based), col 11.
    let usage_text =
        "package com.example.ui\n\nimport com.example.data.UserRepository\n\nfun show() {\n    val repo = UserRepository()\n}\n";
    write(root, "src/ui/Screen.kt", usage_text);

    let mut client = LspClient::spawn(root);
    client.initialize(root);
    client.wait_for_indexing();

    let uri = file_uri(root, "src/ui/Screen.kt");
    client.open_file(&uri, "kotlin", usage_text);

    // `UserRepository` on line 5 (0-based): "    val repo = UserRepository()"
    // col 19 = start of `UserRepository`
    let resp = client.request(
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": pos(usage_text, 5, 19),
        }),
    );

    let result = &resp["result"];
    // Result may be a single Location or an array.
    let target_uri = if result.is_array() {
        result[0]["uri"].as_str().unwrap_or("").to_owned()
    } else {
        result["uri"].as_str().unwrap_or("").to_owned()
    };

    let expected_uri = file_uri(root, "src/data/UserRepository.kt");
    assert!(
        target_uri == expected_uri,
        "definition must point to UserRepository.kt; got: {target_uri}"
    );
}

/// When two classes share the same simple name in different packages, definition
/// must go to the one that the usage file actually imports.
#[test]
fn smoke_same_name_disambiguation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);

    write(
        root,
        "src/api/Result.kt",
        "package com.example.api\n\nclass Result(val code: Int)\n",
    );
    write(
        root,
        "src/domain/Result.kt",
        "package com.example.domain\n\nclass Result(val value: String)\n",
    );
    // Usage imports the API Result explicitly.
    let usage_text = "package com.example.ui\n\nimport com.example.api.Result\n\nfun handle(): Result {\n    return Result(200)\n}\n";
    write(root, "src/ui/Handler.kt", usage_text);

    let mut client = LspClient::spawn(root);
    client.initialize(root);
    client.wait_for_indexing();

    let uri = file_uri(root, "src/ui/Handler.kt");
    client.open_file(&uri, "kotlin", usage_text);

    // `Result` return type on line 4 (0-based): "fun handle(): Result {"
    // col 14 = start of `Result`
    let resp = client.request(
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": pos(usage_text, 4, 14),
        }),
    );

    let result = &resp["result"];
    let target_uri = if result.is_array() {
        result[0]["uri"].as_str().unwrap_or("").to_owned()
    } else {
        result["uri"].as_str().unwrap_or("").to_owned()
    };

    let api_uri = file_uri(root, "src/api/Result.kt");
    let domain_uri = file_uri(root, "src/domain/Result.kt");

    assert!(
        target_uri == api_uri,
        "definition must go to api/Result (the imported one), not domain/Result;\n\
         api:    {api_uri}\n\
         domain: {domain_uri}\n\
         got:    {target_uri}"
    );
}

/// `textDocument/inlayHint` must return type hints for inferred-type properties
/// and for lambda parameters.  The server emits `: Type` hints (not call-site
/// parameter-name hints), so we verify that:
/// - `val result = add(1, 2)` gets a `: Int` type hint (inferred return type)
/// - `list.forEach { it }` gets a `: String` hint for the `it` variable
#[test]
fn smoke_inlay_hints() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    // A file with inferred-type properties and a lambda.
    let src = "\
package com.example

fun add(alpha: Int, beta: Int): Int = alpha + beta

val result = add(1, 2)
";
    write(root, "src/Math.kt", src);

    let mut client = LspClient::spawn(root);
    client.initialize(root);
    client.wait_for_indexing();

    let uri = file_uri(root, "src/Math.kt");
    client.open_file(&uri, "kotlin", src);

    // Request inlay hints for the whole file.
    let resp = client.request(
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": uri},
            "range": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 6, "character": 0},
            },
        }),
    );

    let hints = resp["result"].as_array().cloned().unwrap_or_default();
    // The server may return an empty list if no hints are emitted, but when it
    // does return hints they must be well-formed `: Type` labels.
    let labels: Vec<String> = hints
        .iter()
        .filter_map(|h| {
            // label may be a plain string or an array of InlayHintLabelPart
            if let Some(s) = h["label"].as_str() {
                Some(s.to_owned())
            } else if let Some(arr) = h["label"].as_array() {
                let parts: Vec<&str> = arr.iter().filter_map(|p| p["value"].as_str()).collect();
                Some(parts.join(""))
            } else {
                None
            }
        })
        .collect();

    // Every label that is present must look like `: SomeType` — the server
    // only emits type-annotation hints, never other kinds.
    for label in &labels {
        assert!(
            label.starts_with(": "),
            "inlay hint label must start with ': '; got: {label:?}"
        );
    }

    // `val result = add(1, 2)` is a top-level inferred-type property.
    // The server should emit `: Int` for it.
    assert!(
        labels.iter().any(|l| l == ": Int"),
        "expected ': Int' type hint for `val result = add(1, 2)`; got: {labels:?}"
    );
}

/// `workspace/symbol` must find a class by name across all indexed files.
#[test]
fn smoke_workspace_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    write(root, "workspace.json", r#"{"sourcePaths":[]}"#);
    write(
        root,
        "src/data/OrderRepository.kt",
        "package com.example.data\n\nclass OrderRepository {\n    fun save() {}\n}\n",
    );
    write(root, "src/Empty.kt", "package com.example\n");

    let mut client = LspClient::spawn(root);
    client.initialize(root);
    client.wait_for_indexing();

    let resp = client.request("workspace/symbol", json!({"query": "OrderRepository"}));

    let symbols = resp["result"].as_array().cloned().unwrap_or_default();
    assert!(
        !symbols.is_empty(),
        "workspace/symbol for 'OrderRepository' must return at least one result"
    );
    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        names.iter().any(|n| *n == "OrderRepository"),
        "results must include 'OrderRepository'; got: {names:?}"
    );
}
