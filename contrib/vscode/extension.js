const { LanguageClient, TransportKind } = require("vscode-languageclient/node");
const vscode = require("vscode");

let client;

function activate(context) {
  const command = vscode.workspace.getConfiguration("kotlinLsp").get("path", "kotlin-lsp");

  const serverOptions = {
    command,
    transport: TransportKind.stdio,
  };

  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "kotlin" },
      { scheme: "file", language: "java" },
    ],
  };

  client = new LanguageClient("kotlin-lsp", "Kotlin LSP", serverOptions, clientOptions);
  client.start();
}

function deactivate() {
  return client?.stop();
}

module.exports = { activate, deactivate };
