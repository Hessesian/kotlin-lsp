#!/usr/bin/env python3
"""
kotlin-cli — thin CLI wrapper around kotlin-lsp.

Uses the LSP binary over stdio (JSON-RPC) so no separate server process needs
to be running.  If a cache exists the first query returns in ~100 ms.

Usage:
    kotlin-cli.py find-declaration <Name>           [--workspace DIR]
    kotlin-cli.py find-references  <Name>           [--workspace DIR]
    kotlin-cli.py list-symbols     [<query>]        [--workspace DIR]
    kotlin-cli.py hover            <file> <line> <col> [--workspace DIR]

Examples:
    python contrib/kotlin-cli.py find-declaration MainViewModel
    python contrib/kotlin-cli.py list-symbols "ChildDashboardViewModel"
    python contrib/kotlin-cli.py hover src/main/kotlin/App.kt 12 5
    python contrib/kotlin-cli.py find-declaration UserRepository \\
        --workspace /home/user/myproject/android

Options:
    --workspace DIR   Root of the Kotlin/Android project.
                      Defaults to the current working directory.
    --binary PATH     Path to the kotlin-lsp binary (default: kotlin-lsp).
    --timeout SECS    Seconds to wait for each response (default: 30).
    --json            Output raw JSON instead of human-readable text.

TCP mode (for Sora Editor / remote clients):
    kotlin-lsp --port 9257
    # then connect Sora Editor's editor-lsp to host:9257
    # or tunnel over USB with: adb forward tcp:9257 tcp:9257
"""

import argparse
import json
import os
import pathlib
import queue
import subprocess
import sys
import threading
import time


# ── JSON-RPC framing ──────────────────────────────────────────────────────────

def _encode(obj: dict) -> bytes:
    body = json.dumps(obj, separators=(",", ":")).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode() + body


def _reader_thread(stdout, q: queue.Queue):
    """Background thread: parse Content-Length framed messages and enqueue them."""
    try:
        while True:
            # Read headers
            headers = {}
            while True:
                line = stdout.readline()
                if not line:
                    return
                line = line.decode().strip()
                if not line:
                    break
                key, _, val = line.partition(":")
                headers[key.strip().lower()] = val.strip()

            length = int(headers.get("content-length", 0))
            if length == 0:
                continue
            body = stdout.read(length)
            try:
                q.put(json.loads(body))
            except json.JSONDecodeError:
                pass
    except Exception:
        pass


# ── LSP session ───────────────────────────────────────────────────────────────

class LspClient:
    def __init__(self, binary: str, workspace: str, timeout: float):
        self.workspace = os.path.abspath(workspace)
        self.timeout = timeout
        self._id = 0
        self._q: queue.Queue = queue.Queue()
        self._pending: dict[int, queue.Queue] = {}
        self._proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        t = threading.Thread(target=_reader_thread,
                             args=(self._proc.stdout, self._q), daemon=True)
        t.start()
        threading.Thread(target=self._dispatcher, daemon=True).start()

    def _dispatcher(self):
        while True:
            try:
                msg = self._q.get(timeout=1)
            except queue.Empty:
                continue
            rid = msg.get("id")
            if rid is not None:
                q = self._pending.get(rid)
                if q is not None:
                    q.put(msg)

    def _send(self, msg: dict):
        self._proc.stdin.write(_encode(msg))
        self._proc.stdin.flush()

    def request(self, method: str, params: dict) -> dict:
        self._id += 1
        rid = self._id
        q: queue.Queue = queue.Queue()
        self._pending[rid] = q
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        try:
            return q.get(timeout=self.timeout)
        except queue.Empty:
            raise TimeoutError(f"No response for {method!r} within {self.timeout}s")
        finally:
            self._pending.pop(rid, None)

    def notify(self, method: str, params: dict):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def initialize(self):
        root_uri = pathlib.Path(self.workspace).as_uri()
        self.request("initialize", {
            "processId": os.getpid(),
            "rootUri": root_uri,
            "workspaceFolders": [{"uri": root_uri, "name": os.path.basename(self.workspace)}],
            "capabilities": {},
        })
        self.notify("initialized", {})

    def shutdown(self):
        try:
            self.request("shutdown", {})
            self.notify("exit", {})
        except Exception:
            pass
        finally:
            self._proc.terminate()


# ── Commands ──────────────────────────────────────────────────────────────────

def _loc(loc: dict) -> str:
    uri = loc["uri"].removeprefix("file://")
    r = loc["range"]["start"]
    return f"{uri}:{r['line'] + 1}:{r['character'] + 1}"


def cmd_find_declaration(client: LspClient, name: str, as_json: bool):
    resp = client.request("workspace/symbol", {"query": name})
    symbols = resp.get("result") or []
    matches = [s for s in symbols if s["name"] == name]
    if as_json:
        print(json.dumps(matches, indent=2))
        return
    if not matches:
        print(f"No declaration found for '{name}'", file=sys.stderr)
        sys.exit(1)
    for s in matches:
        kind_map = {5: "class", 6: "method", 13: "enum", 9: "constructor",
                    12: "function", 8: "field", 14: "string"}
        kind = kind_map.get(s.get("kind", 0), "symbol")
        container = f" [{s['containerName']}]" if s.get("containerName") else ""
        print(f"{_loc(s['location'])}  {kind}  {s['name']}{container}")


def cmd_list_symbols(client: LspClient, query: str, as_json: bool):
    resp = client.request("workspace/symbol", {"query": query})
    symbols = resp.get("result") or []
    if as_json:
        print(json.dumps(symbols, indent=2))
        return
    for s in symbols:
        container = f"{s['containerName']}." if s.get("containerName") else ""
        print(f"{_loc(s['location'])}  {container}{s['name']}")


def cmd_find_references(client: LspClient, name: str, as_json: bool):
    # First locate the declaration to get a position to pivot from
    resp = client.request("workspace/symbol", {"query": name})
    symbols = resp.get("result") or []
    decl = next((s for s in symbols if s["name"] == name), None)
    if not decl:
        print(f"No declaration found for '{name}'", file=sys.stderr)
        sys.exit(1)

    loc = decl["location"]
    resp2 = client.request("textDocument/references", {
        "textDocument": {"uri": loc["uri"]},
        "position": loc["range"]["start"],
        "context": {"includeDeclaration": True},
    })
    refs = resp2.get("result") or []
    if as_json:
        print(json.dumps(refs, indent=2))
        return
    if not refs:
        print("No references found.", file=sys.stderr)
        sys.exit(1)
    for r in refs:
        print(_loc(r))


def cmd_hover(client: LspClient, file: str, line: int, col: int, as_json: bool):
    uri = "file://" + os.path.abspath(file)
    resp = client.request("textDocument/hover", {
        "textDocument": {"uri": uri},
        "position": {"line": line - 1, "character": col - 1},
    })
    result = resp.get("result")
    if as_json:
        print(json.dumps(result, indent=2))
        return
    if not result:
        print("No hover info.", file=sys.stderr)
        sys.exit(1)
    contents = result.get("contents", {})
    if isinstance(contents, dict):
        print(contents.get("value", ""))
    elif isinstance(contents, list):
        for c in contents:
            print(c.get("value", c) if isinstance(c, dict) else c)
    else:
        print(contents)


# ── Entry point ───────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(
        description="CLI wrapper around kotlin-lsp",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--workspace", default=os.getcwd(),
                        help="Kotlin/Android project root (default: cwd)")
    parser.add_argument("--binary", default="kotlin-lsp",
                        help="Path to kotlin-lsp binary (default: kotlin-lsp)")
    parser.add_argument("--timeout", type=float, default=30,
                        help="Seconds to wait for each LSP response (default: 30)")
    parser.add_argument("--json", action="store_true",
                        help="Output raw JSON")

    sub = parser.add_subparsers(dest="cmd", required=True)

    p_decl = sub.add_parser("find-declaration", help="Find where a symbol is declared")
    p_decl.add_argument("name")

    p_refs = sub.add_parser("find-references", help="Find all usages of a symbol")
    p_refs.add_argument("name")

    p_sym = sub.add_parser("list-symbols", help="List/search symbols in workspace")
    p_sym.add_argument("query", nargs="?", default="",
                       help="Symbol name filter (empty = all)")

    p_hover = sub.add_parser("hover", help="Get type info at a file position")
    p_hover.add_argument("file")
    p_hover.add_argument("line", type=int)
    p_hover.add_argument("col", type=int)

    args = parser.parse_args()

    client = LspClient(args.binary, args.workspace, args.timeout)
    client.initialize()

    # Give the server a moment to load from cache (usually instant)
    time.sleep(0.2)

    try:
        if args.cmd == "find-declaration":
            cmd_find_declaration(client, args.name, args.json)
        elif args.cmd == "find-references":
            cmd_find_references(client, args.name, args.json)
        elif args.cmd == "list-symbols":
            cmd_list_symbols(client, args.query, args.json)
        elif args.cmd == "hover":
            cmd_hover(client, args.file, args.line, args.col, args.json)
    finally:
        client.shutdown()


if __name__ == "__main__":
    main()
