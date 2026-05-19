#!/usr/bin/env python3
"""
kotlin-cli — thin CLI wrapper around kotlin-lsp.

Uses the LSP binary over stdio (JSON-RPC) so no separate server process needs
to be running.  If a cache exists the first query returns in ~100 ms.

Usage:
    kotlin-cli.py find-declaration  <Name>                       [--workspace DIR]
    kotlin-cli.py find-references   <Name>                       [--workspace DIR]
    kotlin-cli.py list-symbols      [<query>]                    [--workspace DIR]
    kotlin-cli.py hover             <file> <line> <col>          [--workspace DIR]
    kotlin-cli.py find-dead-code    [--kind class|fun|val|var]   [--workspace DIR]
    kotlin-cli.py find-implementors <Name>                       [--workspace DIR]
    kotlin-cli.py extract-interface <ClassName>                  [--workspace DIR]
    kotlin-cli.py rename            <OldName> <NewName> [--dry-run] [--workspace DIR]
    kotlin-cli.py list-templates                                 [--workspace DIR]
    kotlin-cli.py scaffold-feature  <FeatureName> --package PKG  [--workspace DIR]
    kotlin-cli.py generate-template <ClassName> --family NAME    [--workspace DIR]

Examples:
    python contrib/kotlin-cli.py find-declaration MainViewModel
    python contrib/kotlin-cli.py list-symbols "ChildDashboardViewModel"
    python contrib/kotlin-cli.py hover src/main/kotlin/App.kt 12 5
    python contrib/kotlin-cli.py find-dead-code --kind class --exclude-tests
    python contrib/kotlin-cli.py find-implementors IUserRepository
    python contrib/kotlin-cli.py extract-interface UserRepositoryImpl
    python contrib/kotlin-cli.py rename OldName NewName --dry-run
    python contrib/kotlin-cli.py list-templates
    python contrib/kotlin-cli.py scaffold-feature GoldConversion \\
        --package cz.moneta.smartbanka.feature \\
        --src-root feature/gold_conversion/src/main/kotlin --dry-run

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
import concurrent.futures
import json
import os
import pathlib
import queue
import re
import subprocess
import sys
import threading
import time
import xml.etree.ElementTree as ET


# ── LSP SymbolKind constants (LSP 3.x) ───────────────────────────────────────

_KIND_NAMES: dict[int, str] = {
    1: "file", 2: "module", 3: "namespace", 4: "package",
    5: "class", 6: "method", 7: "property", 8: "field",
    9: "constructor", 10: "enum", 11: "interface", 12: "function",
    13: "variable", 14: "constant", 15: "string", 16: "number",
    17: "boolean", 18: "array", 19: "object", 20: "key",
    21: "null", 22: "enum_member", 23: "struct", 24: "event",
    25: "operator", 26: "type_parameter",
}

# Kinds produced by kotlin-lsp for Kotlin symbols
_KOTLIN_CLASS_KINDS = {"class", "interface", "object", "struct"}
_KOTLIN_CALLABLE_KINDS = {"method", "function", "constructor"}
_KOTLIN_MEMBER_KINDS = {"method", "function", "constructor", "property", "field"}


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
        # Serialises id allocation + pending registration + stdin write so the
        # client is safe to use from multiple threads (e.g. find-dead-code).
        self._send_lock = threading.Lock()
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
                with self._send_lock:
                    q = self._pending.get(rid)
                if q is not None:
                    q.put(msg)

    def _send(self, msg: dict):
        self._proc.stdin.write(_encode(msg))
        self._proc.stdin.flush()

    def request(self, method: str, params: dict) -> dict:
        with self._send_lock:
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
            with self._send_lock:
                self._pending.pop(rid, None)

    def notify(self, method: str, params: dict):
        with self._send_lock:
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

    def wait_for_index(self, timeout: float = 30.0, poll_interval: float = 0.3) -> bool:
        """Poll workspace/symbol until the index returns at least one result.

        Returns True when ready, False if timeout expires.
        """
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                resp = self.request("workspace/symbol", {"query": ""})
                if resp.get("result"):
                    return True
            except TimeoutError:
                pass
            time.sleep(poll_interval)
        return False

    def shutdown(self):
        try:
            self.request("shutdown", {})
            self.notify("exit", {})
        except Exception:
            pass
        finally:
            self._proc.terminate()


# ── IDEA file-template helpers ────────────────────────────────────────────────

def _pascal_to_snake(name: str) -> str:
    """GoldConversion → gold_conversion"""
    s1 = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s1).lower()


def _expand_vars(text: str, variables: dict[str, str]) -> str:
    """Substitute ${VAR_NAME} placeholders using variables dict.

    Unknown placeholders are left as-is so the caller can see what's missing.
    """
    return re.sub(r"\$\{(\w+)\}", lambda m: variables.get(m.group(1), m.group(0)), text)


def _find_templates_dir(workspace: str) -> pathlib.Path | None:
    """Locate .idea/fileTemplates starting at workspace, searching up two levels."""
    for base in [pathlib.Path(workspace),
                 pathlib.Path(workspace).parent,
                 pathlib.Path(workspace).parent.parent]:
        d = base / ".idea" / "fileTemplates"
        if d.is_dir():
            return d
    return None


def _parse_template_settings(templates_dir: pathlib.Path) -> dict[str, str]:
    """Parse file.template.settings.xml → {template_filename: file-name_pattern}.

    The file-name pattern is the IDEA-style relative path (without .kt extension)
    that may contain ${VAR} placeholders, e.g.
    'contract/${feature_name}/${FEATURE_NAME}Contract'.
    """
    settings_file = templates_dir.parent / "file.template.settings.xml"
    if not settings_file.exists():
        return {}
    mapping: dict[str, str] = {}
    for tmpl in ET.parse(settings_file).getroot().iter("template"):
        name = tmpl.get("name", "")
        file_name = tmpl.get("file-name", "")
        if name and file_name:
            mapping[name] = file_name
    return mapping


def _load_template_family(
    templates_dir: pathlib.Path, base_name: str
) -> list[tuple[str, str]]:
    """Return [(template_filename, content)] for parent + all children."""
    parent = templates_dir / base_name
    if not parent.exists():
        return []
    family = [(base_name, parent.read_text(encoding="utf-8"))]
    for i in range(20):
        child = templates_dir / f"{base_name}.child.{i}.kt"
        if not child.exists():
            break
        family.append((child.name, child.read_text(encoding="utf-8")))
    return family


def _detect_src_root(workspace: str) -> pathlib.Path:
    """Heuristic: return the shallowest src/main/kotlin dir under workspace."""
    candidates = [
        c for c in pathlib.Path(workspace).rglob("src/main/kotlin")
        if "build" not in c.parts
    ]
    return sorted(candidates, key=lambda p: len(p.parts))[0] if candidates \
        else pathlib.Path(workspace)


_KNOWN_CLASS_SUFFIXES = [
    "Interactor", "Screen", "ViewModel", "Contract", "Mapper",
    "Repository", "Fragment", "Activity", "Reducer", "Effect",
    "Factory", "UseCase", "Handler", "Presenter", "Coordinator", "Navigator",
]

_ROLE_PACKAGES = {
    "contract", "interactor", "screen", "viewmodel", "mapper",
    "repository", "reducer", "factory", "handler", "usecase",
    "presenter", "coordinator", "navigator", "effect",
}

# Variables auto-provided by scaffold-feature (used to un-escape after dollar-sign pass)
_SCAFFOLD_VARS = {"FEATURE_NAME", "featureName", "feature_name", "PACKAGE_NAME"}


def _extract_feature_id(class_name: str) -> str:
    """GoldConversionInteractor → GoldConversion (strips first known suffix found)."""
    for suffix in _KNOWN_CLASS_SUFFIXES:
        if class_name.endswith(suffix) and len(class_name) > len(suffix):
            return class_name[:-len(suffix)]
    return class_name


def _file_suffix(stem: str, feature_id: str) -> str:
    """GoldConversionInteractor → Interactor (part after feature_id)."""
    if stem.startswith(feature_id):
        tail = stem[len(feature_id):]
        if tail:
            return tail
    for s in _KNOWN_CLASS_SUFFIXES:
        if stem.endswith(s):
            return s
    return stem


def _infer_base_package(pkg: str) -> str:
    """com.example.goldconversion.contract → com.example.goldconversion (strip role parts)."""
    parts = pkg.split(".")
    while parts and parts[-1].lower() in _ROLE_PACKAGES:
        parts.pop()
    return ".".join(parts)


def _parameterize(content: str, feature_pascal: str, base_package: str) -> str:
    """Replace feature-identifier variants with ${VAR} placeholders.

    Uses sentinels so we can safely escape Kotlin/Velocity dollar signs without
    accidentally clobbering the placeholders we just inserted.
    """
    feature_camel = feature_pascal[0].lower() + feature_pascal[1:]
    feature_snake = _pascal_to_snake(feature_pascal)

    result = content

    # 1. Package path (most specific — replace before individual word parts)
    if base_package:
        result = result.replace(base_package, "__PACKAGE_NAME__")

    # 2. Class-name variants → sentinels (Pascal first to avoid partial matches)
    result = result.replace(feature_pascal, "__FEATURE_NAME__")
    result = result.replace(feature_camel,  "__featureName__")
    result = result.replace(feature_snake,  "__feature_name__")

    # 3. Escape remaining ${...} (Kotlin string templates Velocity would interpret)
    result = re.sub(
        r'\$\{(\w+)\}',
        lambda m: f'${{{m.group(1)}}}' if m.group(1).startswith("__") else f'\\${{{m.group(1)}}}',
        result,
    )

    # 4. Escape bare $ident (Velocity reference syntax) — skip our sentinels
    result = re.sub(
        r'\$([A-Za-z_]\w*)',
        lambda m: m.group(0) if m.group(1).startswith("__") else f'\\${m.group(1)}',
        result,
    )

    # 5. Restore sentinels → ${VAR} placeholders
    result = result.replace("__PACKAGE_NAME__", "${PACKAGE_NAME}")
    result = result.replace("__FEATURE_NAME__", "${FEATURE_NAME}")
    result = result.replace("__featureName__",  "${featureName}")
    result = result.replace("__feature_name__", "${feature_name}")

    # 6. Un-escape any of our vars that step 3 may have caught before sentinels were set
    for var in _SCAFFOLD_VARS:
        result = result.replace(f"\\${{{var}}}", f"${{{var}}}")

    return result


def _upsert_template_settings(
    templates_dir: pathlib.Path,
    family_name: str,
    entries: list[dict],
) -> None:
    """Add or replace the family's entries in file.template.settings.xml."""
    settings_file = templates_dir.parent / "file.template.settings.xml"
    if settings_file.exists():
        try:
            tree = ET.parse(settings_file)
            root = tree.getroot()
        except ET.ParseError:
            root = ET.Element("component", name="ExportableFileTemplateSettings")
            tree = ET.ElementTree(root)
    else:
        root = ET.Element("component", name="ExportableFileTemplateSettings")
        tree = ET.ElementTree(root)

    # Remove stale entries for this family
    parent_name = f"{family_name}.kt"
    for tmpl in root.findall("template"):
        tname = tmpl.get("name", "")
        if tname == family_name or tname.startswith(parent_name):
            root.remove(tmpl)

    # Insert new entries
    for entry in entries:
        el = ET.SubElement(root, "template")
        el.set("name", entry["tpl_name"])
        el.set("file-name", entry["file_name_pattern"])
        el.set("reformat", "true")
        el.set("live-template-enabled", "false")

    try:
        ET.indent(tree, space="  ")
    except AttributeError:
        pass  # Python < 3.9

    settings_file.parent.mkdir(parents=True, exist_ok=True)
    tree.write(str(settings_file), encoding="unicode", xml_declaration=False)


# ── IDEA template commands ────────────────────────────────────────────────────

def cmd_list_templates(workspace: str, as_json: bool) -> None:
    """List IDEA file templates available in the project's .idea/fileTemplates."""
    templates_dir = _find_templates_dir(workspace)
    if not templates_dir:
        print(f"No .idea/fileTemplates found under '{workspace}'.", file=sys.stderr)
        sys.exit(1)

    settings = _parse_template_settings(templates_dir)
    parents = sorted(
        [f for f in templates_dir.iterdir() if f.is_file() and ".child." not in f.name],
        key=lambda f: f.name,
    )
    if not parents:
        print("No templates found.", file=sys.stderr)
        sys.exit(1)

    result = []
    for p in parents:
        children = sorted(templates_dir.glob(f"{p.name}.child.*"), key=lambda f: f.name)
        all_content = p.read_text(encoding="utf-8")
        for c in children:
            all_content += c.read_text(encoding="utf-8")

        variables = sorted(set(re.findall(r"\$\{(\w+)\}", all_content)))
        generates = []
        for tpl_name in [p.name] + [c.name for c in children]:
            pat = settings.get(tpl_name)
            if pat:
                generates.append(pat + ".kt")

        result.append({
            "name": p.stem,
            "variables": variables,
            "generates": generates,
        })

    if as_json:
        print(json.dumps(result, indent=2))
        return

    for entry in result:
        print(f"\n{entry['name']}")
        print(f"  variables : {', '.join(entry['variables'])}")
        print("  generates :")
        for g in entry["generates"]:
            print(f"    {g}")


def cmd_scaffold_feature(
    workspace: str,
    feature_name: str,
    template_base: str,
    package_name: str,
    extra_vars: dict[str, str],
    src_root: str | None,
    dry_run: bool,
    as_json: bool,
) -> None:
    """Expand an IDEA file template family and write the resulting Kotlin files.

    File paths are derived from the file-name patterns in
    .idea/file.template.settings.xml combined with --package (PACKAGE_NAME).

    With --json the command outputs [{path, content}] so an AI agent can inspect
    and modify the scaffold before writing.  Without --json files are written
    immediately (skipping any that already exist).
    """
    templates_dir = _find_templates_dir(workspace)
    if not templates_dir:
        print(f"No .idea/fileTemplates found under '{workspace}'.", file=sys.stderr)
        sys.exit(1)

    settings = _parse_template_settings(templates_dir)

    # Resolve template file name (accept stem with or without .kt)
    parent_file = next(
        (f for f in templates_dir.iterdir()
         if f.is_file() and ".child." not in f.name
         and (f.name == template_base or f.stem == template_base)),
        None,
    )
    if parent_file is None:
        available = ", ".join(
            f.stem for f in templates_dir.iterdir() if ".child." not in f.name
        )
        print(f"Template '{template_base}' not found. Available: {available}",
              file=sys.stderr)
        sys.exit(1)

    family = _load_template_family(templates_dir, parent_file.name)

    # Build variable map; FEATURE_NAME, feature_name, featureName are auto-derived
    variables: dict[str, str] = {
        "FEATURE_NAME": feature_name,
        "feature_name": _pascal_to_snake(feature_name),
        "featureName": feature_name[0].lower() + feature_name[1:],
        "PACKAGE_NAME": package_name,
    }
    variables.update(extra_vars)

    # Remaining unknown placeholders in the template content
    all_raw = "".join(c for _, c in family)
    unknown = set(re.findall(r"\$\{(\w+)\}", all_raw)) - set(variables)
    if unknown:
        print(f"WARNING: unresolved variables: {', '.join(sorted(unknown))}. "
              f"Pass them with --var KEY=VALUE.", file=sys.stderr)

    pkg_path = package_name.replace(".", "/")
    base_src = pathlib.Path(src_root) if src_root else _detect_src_root(workspace)

    results: list[dict[str, str]] = []
    for tpl_name, raw_content in family:
        content = _expand_vars(raw_content, variables)

        file_name_pat = settings.get(tpl_name)
        if file_name_pat:
            rel = _expand_vars(file_name_pat, variables) + ".kt"
        else:
            # Fallback: derive from package + primary type declaration
            pkg_m = re.search(r"^\s*package\s+([\w.]+)", content, re.MULTILINE)
            cls_m = re.search(
                r"(?m)^\s*(?:(?:private|internal|public|protected|abstract"
                r"|open|data|sealed)\s+)*(?:class|interface|object)\s+(\w+)",
                content,
            )
            if pkg_m and cls_m:
                sub_pkg = pkg_m.group(1).removeprefix(package_name).lstrip(".")
                rel = sub_pkg.replace(".", "/") + f"/{cls_m.group(1)}.kt"
            else:
                rel = f"unknown_{tpl_name}"

        out_path = base_src / pkg_path / rel
        results.append({"path": str(out_path), "content": content})

    if as_json:
        print(json.dumps(results, indent=2))
        return

    for r in results:
        path = pathlib.Path(r["path"])
        if dry_run:
            print(f"  would create: {r['path']}")
        elif path.exists():
            print(f"  SKIP (exists): {r['path']}", file=sys.stderr)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(r["content"], encoding="utf-8")
            print(f"  created: {r['path']}")


# ── Helpers ───────────────────────────────────────────────────────────────────


def cmd_generate_template(
    client: LspClient,
    source_class: str,
    family_name: str,
    workspace: str,
    dry_run: bool,
    as_json: bool,
) -> None:
    """Generate IDEA file templates from an existing class and its feature siblings.

    Finds all workspace symbols that share the feature identifier prefix (e.g.
    all GoldConversion* classes), parameterizes their source files by replacing
    every case-variant of the feature identifier with ${FEATURE_NAME} /
    ${featureName} / ${feature_name} / ${PACKAGE_NAME}, and writes the results
    as a reusable IDEA file template family.

    The generated templates are immediately usable with scaffold-feature:
        scaffold-feature NewFeature --template 'My Family' --package com.example
    """
    # 1. Resolve the source class to a file
    sym = _resolve_unique_symbol(client, source_class, kind_filter=_KOTLIN_CLASS_KINDS)
    source_file = pathlib.Path(sym["location"]["uri"].removeprefix("file://"))

    # 2. Derive feature identifier (strip known role suffix: GoldConversionInteractor → GoldConversion)
    feature_id = _extract_feature_id(source_class)
    print(f"Feature identifier: {feature_id!r}", file=sys.stderr)

    # 3. Discover all files that belong to this feature via workspaceSymbol
    #    (reliable across subdirectories — contract/, screen/, viewmodel/, etc.)
    resp = client.request("workspace/symbol", {"query": feature_id})
    all_syms = resp.get("result") or []
    feature_files: dict[str, str] = {}  # filepath → package
    for s in all_syms:
        if not s["name"].startswith(feature_id):
            continue
        if _KIND_NAMES.get(s.get("kind", 0)) not in _KOTLIN_CLASS_KINDS:
            continue
        fpath = s["location"]["uri"].removeprefix("file://")
        if fpath not in feature_files:
            # Read the package declaration from the file
            try:
                for line in pathlib.Path(fpath).read_text(encoding="utf-8").splitlines()[:8]:
                    if line.startswith("package "):
                        feature_files[fpath] = line.split()[1]
                        break
                else:
                    feature_files[fpath] = ""
            except OSError:
                feature_files[fpath] = ""

    if not feature_files:
        # Fallback: just the source file
        feature_files[str(source_file)] = ""

    # Sort: source class file first, rest alphabetically by basename
    sorted_files = sorted(
        feature_files.items(),
        key=lambda kv: (0 if kv[0] == str(source_file) else 1, pathlib.Path(kv[0]).name),
    )

    # 4. Infer base package from source file (strips role sub-packages)
    src_package = feature_files.get(str(source_file), "")
    base_package = _infer_base_package(src_package) if src_package else ""

    print(f"Base package:       {base_package!r}", file=sys.stderr)
    print(f"Files ({len(sorted_files)}):", file=sys.stderr)
    for fp, _ in sorted_files:
        print(f"  {fp}", file=sys.stderr)

    # 5. Parameterize each file and derive file_name_pattern
    entries = []
    for filepath, pkg in sorted_files:
        try:
            content = pathlib.Path(filepath).read_text(encoding="utf-8")
        except OSError as e:
            print(f"Warning: could not read {filepath}: {e}", file=sys.stderr)
            continue

        parameterized = _parameterize(content, feature_id, base_package)
        file_suffix = _file_suffix(pathlib.Path(filepath).stem, feature_id)

        # Derive file_name_pattern relative to the base-package dir
        # e.g. com.example.goldconversion.contract / GoldConversionContract
        #   → contract/${FEATURE_NAME}Contract
        file_name_pattern = ""
        if pkg and base_package and pkg.startswith(base_package):
            sub_pkg = pkg.removeprefix(base_package).lstrip(".")
            stem_param = _parameterize(pathlib.Path(filepath).stem, feature_id, "")
            if sub_pkg:
                file_name_pattern = sub_pkg.replace(".", "/") + "/" + stem_param
            else:
                file_name_pattern = stem_param

        entries.append({
            "filepath":          filepath,
            "file_suffix":       file_suffix,
            "file_name_pattern": file_name_pattern,
            "content":           parameterized,
        })

    if not entries:
        print("No files to templatize.", file=sys.stderr)
        sys.exit(1)

    if as_json:
        print(json.dumps(
            [{"suffix": e["file_suffix"], "source": e["filepath"],
              "file_name": e["file_name_pattern"]} for e in entries],
            indent=2,
        ))
        return

    if dry_run:
        print()
        for e in entries:
            print(f"── {pathlib.Path(e['filepath']).name}  [{e['file_suffix']}]")
            print(f"   file-name: {e['file_name_pattern'] or '(auto-derived by scaffold-feature)'}")
            # Show changed lines
            orig = pathlib.Path(e["filepath"]).read_text().splitlines()
            new  = e["content"].splitlines()
            shown = 0
            for i, (o, n) in enumerate(zip(orig, new)):
                if o != n:
                    print(f"   L{i+1:3d}  - {o.strip()[:80]}")
                    print(f"        + {n.strip()[:80]}")
                    shown += 1
                    if shown >= 6:
                        remaining = sum(1 for a, b in zip(orig[i+1:], new[i+1:]) if a != b)
                        if remaining:
                            print(f"        … ({remaining} more changes)")
                        break
            print()
        return

    # 6. Write template files
    templates_dir = _find_templates_dir(workspace)
    if templates_dir is None:
        templates_dir = pathlib.Path(workspace) / ".idea" / "fileTemplates"
    templates_dir.mkdir(parents=True, exist_ok=True)

    parent_filename = f"{family_name}.kt"
    settings_entries = []
    for i, entry in enumerate(entries):
        tpl_name = parent_filename if i == 0 else f"{parent_filename}.child.{i - 1}.kt"
        out_path = templates_dir / tpl_name
        out_path.write_text(entry["content"], encoding="utf-8")
        print(f"✓ {tpl_name}  [{entry['file_suffix']}]")
        if entry["file_name_pattern"]:
            settings_entries.append({
                "tpl_name":         tpl_name,
                "file_name_pattern": entry["file_name_pattern"],
            })

    if settings_entries:
        _upsert_template_settings(templates_dir, family_name, settings_entries)
        print(f"\nUpdated file.template.settings.xml ({len(settings_entries)} entries)")

    print(f"\nGenerated {len(entries)} template(s) for family '{family_name}'")
    print(f"Use: scaffold-feature NewFeature --template '{family_name}' --package <base-pkg>")



def _loc(loc: dict) -> str:
    uri = loc["uri"].removeprefix("file://")
    r = loc["range"]["start"]
    return f"{uri}:{r['line'] + 1}:{r['character'] + 1}"


def _loc_from_sym(sym: dict) -> str:
    """Format location from a DocumentSymbol (has range, not location.range)."""
    uri = sym["_uri"].removeprefix("file://")
    r = sym["selectionRange"]["start"]
    return f"{uri}:{r['line'] + 1}:{r['character'] + 1}"


def _range_contains(outer: dict, inner: dict) -> bool:
    """Return True if outer LSP range fully contains inner."""
    os_, oe = outer["start"], outer["end"]
    is_, ie = inner["start"], inner["end"]
    start_ok = (os_["line"] < is_["line"] or
                (os_["line"] == is_["line"] and os_["character"] <= is_["character"]))
    end_ok = (oe["line"] > ie["line"] or
              (oe["line"] == ie["line"] and oe["character"] >= ie["character"]))
    return start_ok and end_ok


def _walk_kotlin_files(workspace: str, exclude_tests: bool) -> list[str]:
    """Enumerate .kt and .java files in workspace, skipping build dirs."""
    _SKIP_DIRS = {"build", ".gradle", ".idea", ".git", "target", "node_modules"}
    _TEST_DIRS = {"test", "androidTest", "testFixtures", "commonTest", "jvmTest"}
    files = []
    for root, dirs, filenames in os.walk(workspace):
        dirs[:] = [d for d in dirs
                   if d not in _SKIP_DIRS
                   and not d.startswith(".")
                   and (not exclude_tests or d not in _TEST_DIRS)]
        for f in filenames:
            if f.endswith(".kt") or f.endswith(".java"):
                files.append(os.path.join(root, f))
    return files


def _document_symbols(client: LspClient, filepath: str) -> list[dict]:
    """Return DocumentSymbol list for a file, with _uri injected for tracking."""
    uri = pathlib.Path(filepath).as_uri()
    resp = client.request("textDocument/documentSymbol", {"textDocument": {"uri": uri}})
    result = resp.get("result") or []
    for sym in result:
        sym["_uri"] = uri
    return result


def _resolve_unique_symbol(client: LspClient, name: str,
                            kind_filter: set[str] | None = None) -> dict:
    """Find exactly one workspace symbol matching name (and optional kind).

    Prints candidates and exits if 0 or >1 match is found.
    """
    resp = client.request("workspace/symbol", {"query": name})
    symbols = resp.get("result") or []
    matches = [s for s in symbols if s["name"] == name]
    if kind_filter:
        matches = [s for s in matches
                   if _KIND_NAMES.get(s.get("kind", 0)) in kind_filter]
    if not matches:
        print(f"No declaration found for '{name}'.", file=sys.stderr)
        sys.exit(1)
    if len(matches) > 1:
        print(f"Ambiguous: {len(matches)} symbols named '{name}'. "
              f"Specify one with --file or use a more qualified name.", file=sys.stderr)
        for s in matches:
            kind = _KIND_NAMES.get(s.get("kind", 0), "?")
            container = f" [{s['containerName']}]" if s.get("containerName") else ""
            print(f"  {_loc(s['location'])}  {kind}  {s['name']}{container}",
                  file=sys.stderr)
        sys.exit(1)
    return matches[0]


# ── Commands ──────────────────────────────────────────────────────────────────

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
        kind = _KIND_NAMES.get(s.get("kind", 0), "symbol")
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


def cmd_find_dead_code(client: LspClient, workspace: str,
                       kinds: list[str], exclude_tests: bool,
                       limit: int | None, as_json: bool):
    """Find symbols with zero external references.

    NOTE: results are candidates — reflection, XML manifests, DI wiring, and
    framework entry-points may appear unreferenced. Review before deleting.
    """
    files = _walk_kotlin_files(workspace, exclude_tests)
    print(f"  scanning {len(files)} file(s)…", file=sys.stderr)

    all_symbols: list[dict] = []
    for filepath in files:
        syms = _document_symbols(client, filepath)
        if kinds:
            syms = [s for s in syms if _KIND_NAMES.get(s.get("kind", 0)) in set(kinds)]
        all_symbols.extend(syms)

    if limit:
        all_symbols = all_symbols[:limit]

    total = len(all_symbols)
    print(f"  checking {total} symbol(s) for references…", file=sys.stderr)

    dead: list[dict] = []
    completed = 0

    def check_sym(sym: dict) -> dict | None:
        resp = client.request("textDocument/references", {
            "textDocument": {"uri": sym["_uri"]},
            "position": sym["selectionRange"]["start"],
            "context": {"includeDeclaration": False},
        })
        refs = resp.get("result") or []
        return sym if not refs else None

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        futures = {pool.submit(check_sym, sym): sym for sym in all_symbols}
        for future in concurrent.futures.as_completed(futures):
            completed += 1
            if completed % 20 == 0 or completed == total:
                print(f"\r  {completed}/{total}…", end="", flush=True, file=sys.stderr)
            result = future.result()
            if result is not None:
                dead.append(result)

    print(f"\r  {total} checked — {len(dead)} unreferenced.      ", file=sys.stderr)

    if as_json:
        # Strip internal _uri key before serialising
        print(json.dumps([{k: v for k, v in s.items() if k != "_uri"} for s in dead],
                         indent=2))
        return
    for sym in dead:
        kind = _KIND_NAMES.get(sym.get("kind", 0), "?")
        print(f"{_loc_from_sym(sym)}  {kind}  {sym['name']}")


def cmd_find_implementors(client: LspClient, name: str, as_json: bool):
    """List all classes/objects that implement an interface or extend a class."""
    sym = _resolve_unique_symbol(client, name, kind_filter=_KOTLIN_CLASS_KINDS)
    loc = sym["location"]
    resp = client.request("textDocument/implementation", {
        "textDocument": {"uri": loc["uri"]},
        "position": loc["range"]["start"],
    })
    result = resp.get("result") or []
    if isinstance(result, dict):
        result = [result]
    if as_json:
        print(json.dumps(result, indent=2))
        return
    if not result:
        print("No implementations found.", file=sys.stderr)
        sys.exit(1)
    for r in result:
        # Location vs LocationLink
        if "targetUri" in r:
            uri = r["targetUri"].removeprefix("file://")
            s = r["targetSelectionRange"]["start"]
            print(f"{uri}:{s['line']+1}:{s['character']+1}")
        else:
            print(_loc(r))


def cmd_extract_interface(client: LspClient, class_name: str, as_json: bool):
    """Generate a Kotlin interface skeleton from a class's public members.

    Signatures come from DocumentSymbol.detail (the truncated declaration stored
    in the index).  Abstract/interface method bodies are excluded automatically.
    Output is printed to stdout — pipe to a .kt file or paste into your editor.
    """
    # 1. Locate the class file
    sym = _resolve_unique_symbol(client, class_name, kind_filter=_KOTLIN_CLASS_KINDS)
    file_uri = sym["location"]["uri"]
    filepath = file_uri.removeprefix("file://")

    # 2. Get all symbols in that file
    all_syms = _document_symbols(client, filepath)

    # 3. Find the class symbol by name to get its range
    class_sym = next(
        (s for s in all_syms
         if s["name"] == class_name
         and _KIND_NAMES.get(s.get("kind", 0)) in _KOTLIN_CLASS_KINDS),
        None,
    )
    if class_sym is None:
        print(f"Could not locate '{class_name}' in documentSymbol results.",
              file=sys.stderr)
        sys.exit(1)

    class_range = class_sym["range"]

    # 4. Collect members: first record all function/method ranges, then
    #    include properties only if they lie outside any function body.
    #    Also skip visibly-private/internal symbols by detail prefix.
    function_ranges: list[dict] = []
    nested_class_ranges: list[dict] = []
    candidate_members: list[dict] = []

    for s in all_syms:
        if s is class_sym:
            continue
        kind = _KIND_NAMES.get(s.get("kind", 0))
        sym_range = s["range"]
        if not _range_contains(class_range, sym_range):
            continue
        if kind in _KOTLIN_CLASS_KINDS:
            nested_class_ranges.append(sym_range)
            continue
        if kind in ("method", "function"):
            function_ranges.append(sym_range)
            candidate_members.append(s)
        elif kind in _KOTLIN_MEMBER_KINDS:
            candidate_members.append(s)

    members = []
    for s in candidate_members:
        kind = _KIND_NAMES.get(s.get("kind", 0))
        sym_range = s["range"]
        # Skip symbols inside a nested class
        if any(_range_contains(nr, sym_range) for nr in nested_class_ranges):
            continue
        # Skip local variables/properties that live inside a function body
        if kind not in ("method", "function") and any(
            _range_contains(fr, sym_range) for fr in function_ranges
        ):
            continue
        # Skip private/internal members (they don't belong in a public interface)
        detail: str = s.get("detail") or ""
        if detail.startswith(("private ", "internal ", "protected ")):
            continue
        members.append(s)

    if not members:
        print(f"No members found inside '{class_name}'.", file=sys.stderr)
        sys.exit(1)

    if as_json:
        print(json.dumps(
            [{"name": m["name"],
              "kind": _KIND_NAMES.get(m.get("kind", 0), "?"),
              "detail": m.get("detail", "")}
             for m in members],
            indent=2,
        ))
        return

    iface_name = f"I{class_name}"
    lines = [f"interface {iface_name} {{"]
    for m in members:
        detail: str = m.get("detail") or ""
        if detail:
            # detail is a truncated declaration like "fun foo(x: Int): String"
            # Strip any trailing body or brace
            decl = detail.split("{")[0].rstrip(" =").strip()
            lines.append(f"    {decl}")
        else:
            # Fallback: emit a comment so nothing is silently lost
            kind = _KIND_NAMES.get(m.get("kind", 0), "?")
            lines.append(f"    // {kind} {m['name']}")
    lines.append("}")
    print("\n".join(lines))


def _apply_text_edits(path: str, edits: list[dict]) -> None:
    """Apply LSP TextEdits to a file.

    Edits are applied in reverse order (bottom-to-top) to preserve line/column
    positions of earlier edits.  LSP positions are UTF-16 code units; for the
    common case of ASCII-only identifiers this coincides with Python str indices.
    Files with non-BMP characters may see incorrect offsets.
    """
    with open(path, encoding="utf-8") as fh:
        lines = fh.readlines()

    sorted_edits = sorted(
        edits,
        key=lambda e: (e["range"]["start"]["line"], e["range"]["start"]["character"]),
        reverse=True,
    )
    for edit in sorted_edits:
        sl = edit["range"]["start"]["line"]
        sc = edit["range"]["start"]["character"]
        el = edit["range"]["end"]["line"]
        ec = edit["range"]["end"]["character"]
        before = lines[sl][:sc]
        after = lines[el][ec:]
        lines[sl : el + 1] = [before + edit["newText"] + after]

    with open(path, "w", encoding="utf-8") as fh:
        fh.writelines(lines)


def _apply_workspace_edit(edit: dict, dry_run: bool) -> int:
    """Apply (or preview) a WorkspaceEdit. Returns number of files affected."""
    doc_changes = edit.get("documentChanges") or []
    plain_changes: dict[str, list] = edit.get("changes") or {}

    if doc_changes:
        for dc in doc_changes:
            path = dc["textDocument"]["uri"].removeprefix("file://")
            file_edits = dc.get("edits", [])
            if dry_run:
                print(f"  {path}: {len(file_edits)} change(s)")
                for e in file_edits:
                    r = e["range"]
                    print(f"    [{r['start']['line']+1}:{r['start']['character']+1}]"
                          f" → {e['newText']!r}")
            else:
                _apply_text_edits(path, file_edits)
        return len(doc_changes)

    for uri_str, file_edits in plain_changes.items():
        path = uri_str.removeprefix("file://")
        if dry_run:
            print(f"  {path}: {len(file_edits)} change(s)")
            for e in file_edits:
                r = e["range"]
                print(f"    [{r['start']['line']+1}:{r['start']['character']+1}]"
                      f" → {e['newText']!r}")
        else:
            _apply_text_edits(path, file_edits)
    return len(plain_changes)


def cmd_rename(client: LspClient, old_name: str, new_name: str,
               dry_run: bool, as_json: bool):
    """Rename a symbol across the entire workspace using LSP textDocument/rename."""
    sym = _resolve_unique_symbol(client, old_name)
    loc = sym["location"]

    prep = client.request("textDocument/prepareRename", {
        "textDocument": {"uri": loc["uri"]},
        "position": loc["range"]["start"],
    })
    if prep.get("error") or not prep.get("result"):
        msg = (prep.get("error") or {}).get("message", "server rejected rename")
        print(f"Cannot rename '{old_name}': {msg}", file=sys.stderr)
        sys.exit(1)

    resp = client.request("textDocument/rename", {
        "textDocument": {"uri": loc["uri"]},
        "position": loc["range"]["start"],
        "newName": new_name,
    })
    if resp.get("error"):
        print(f"Rename failed: {resp['error'].get('message', 'unknown')}", file=sys.stderr)
        sys.exit(1)

    result = resp.get("result")
    if as_json:
        print(json.dumps(result, indent=2))
        return
    if not result:
        print("No changes produced.", file=sys.stderr)
        sys.exit(1)

    if dry_run:
        print(f"Dry run: '{old_name}' → '{new_name}'")
        n = _apply_workspace_edit(result, dry_run=True)
        print(f"  {n} file(s) would be modified.")
    else:
        n = _apply_workspace_edit(result, dry_run=False)
        print(f"Renamed '{old_name}' → '{new_name}' across {n} file(s).")


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

    p_dead = sub.add_parser("find-dead-code",
                             help="Find symbols with zero external references")
    p_dead.add_argument(
        "--kind", nargs="+",
        metavar="KIND",
        choices=list(_KIND_NAMES.values()),
        default=["class", "interface", "function"],
        help="Symbol kinds to check (default: class interface function)",
    )
    p_dead.add_argument("--exclude-tests", action="store_true",
                        help="Skip test source directories")
    p_dead.add_argument("--limit", type=int, default=None,
                        help="Max number of symbols to check (for quick sampling)")

    p_impl = sub.add_parser("find-implementors",
                             help="List all classes that implement an interface")
    p_impl.add_argument("name")

    p_iface = sub.add_parser("extract-interface",
                              help="Generate an interface skeleton from a class")
    p_iface.add_argument("class_name", metavar="ClassName")

    p_ren = sub.add_parser("rename", help="Rename a symbol across the workspace")
    p_ren.add_argument("old_name", metavar="OldName")
    p_ren.add_argument("new_name", metavar="NewName")
    p_ren.add_argument("--dry-run", action="store_true",
                       help="Print changes without writing files")

    sub.add_parser("list-templates",
                   help="List IDEA file templates available in the project")

    p_scaffold = sub.add_parser("scaffold-feature",
                                help="Generate files from an IDEA file template")
    p_scaffold.add_argument("feature_name", metavar="FeatureName",
                            help="PascalCase feature name (e.g. GoldConversion)")
    p_scaffold.add_argument("--template", default="New Contract",
                            help="Template base name (default: 'New Contract')")
    p_scaffold.add_argument("--package", dest="package", required=True,
                            metavar="PKG",
                            help="Base package name assigned to PACKAGE_NAME")
    p_scaffold.add_argument("--var", nargs="+", metavar="KEY=VALUE",
                            dest="extra_vars", default=[],
                            help="Additional template variables (KEY=VALUE)")
    p_scaffold.add_argument("--src-root", metavar="DIR",
                            help="Module src/main/kotlin root (auto-detected if omitted)")
    p_scaffold.add_argument("--dry-run", action="store_true",
                            help="Print paths without writing files")

    p_gen = sub.add_parser("generate-template",
                           help="Generate reusable IDEA file templates from an existing class")
    p_gen.add_argument("source_class", metavar="ClassName",
                       help="Existing class to use as pattern source (e.g. GoldConversionInteractor)")
    p_gen.add_argument("--family", default=None, metavar="NAME",
                       help="Template family name (default: derived from source class suffix)")
    p_gen.add_argument("--dry-run", action="store_true",
                       help="Preview parameterization without writing files")

    args = parser.parse_args()

    # Template commands don't need an LSP server — run them immediately and exit.
    if args.cmd == "list-templates":
        cmd_list_templates(args.workspace, args.json)
        return
    if args.cmd == "scaffold-feature":
        extra: dict[str, str] = {}
        for kv in (args.extra_vars or []):
            if "=" in kv:
                k, _, v = kv.partition("=")
                extra[k.strip()] = v.strip()
            else:
                print(f"Ignoring malformed --var entry (expected KEY=VALUE): {kv!r}",
                      file=sys.stderr)
        cmd_scaffold_feature(
            workspace=args.workspace,
            feature_name=args.feature_name,
            template_base=args.template,
            package_name=args.package,
            extra_vars=extra,
            src_root=args.src_root,
            dry_run=args.dry_run,
            as_json=args.json,
        )
        return


    client = LspClient(args.binary, args.workspace, args.timeout)
    client.initialize()

    # Wait until the index reports at least one symbol (cache load or fresh scan)
    if not client.wait_for_index(timeout=args.timeout):
        print("WARNING: index not ready after timeout — results may be empty.",
              file=sys.stderr)

    try:
        if args.cmd == "find-declaration":
            cmd_find_declaration(client, args.name, args.json)
        elif args.cmd == "find-references":
            cmd_find_references(client, args.name, args.json)
        elif args.cmd == "list-symbols":
            cmd_list_symbols(client, args.query, args.json)
        elif args.cmd == "hover":
            cmd_hover(client, args.file, args.line, args.col, args.json)
        elif args.cmd == "find-dead-code":
            cmd_find_dead_code(client, args.workspace, args.kind,
                               args.exclude_tests, args.limit, args.json)
        elif args.cmd == "find-implementors":
            cmd_find_implementors(client, args.name, args.json)
        elif args.cmd == "extract-interface":
            cmd_extract_interface(client, args.class_name, args.json)
        elif args.cmd == "rename":
            cmd_rename(client, args.old_name, args.new_name,
                       args.dry_run, args.json)
        elif args.cmd == "generate-template":
            family = args.family or _file_suffix(args.source_class,
                                                  _extract_feature_id(args.source_class)) or args.source_class
            cmd_generate_template(
                client,
                source_class=args.source_class,
                family_name=family,
                workspace=args.workspace,
                dry_run=args.dry_run,
                as_json=args.json,
            )
    finally:
        client.shutdown()


if __name__ == "__main__":
    main()
