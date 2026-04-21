#!/usr/bin/env python3
"""
extract-sources.py — unpack Gradle *-sources.jar files for kotlin-lsp

Finds *-sources.jar files under a Gradle cache directory, deduplicates by
keeping only the latest version of each artifact, and extracts .kt/.java
sources to an output directory suitable for use with kotlin-lsp sourcePaths.

Usage:
    python3 extract-sources.py [OPTIONS] [PATTERN ...]

    PATTERN  Optional substring filters on group/artifact ID
             e.g.  androidx.compose  org.jetbrains.kotlinx

Options:
    --gradle-home DIR   Gradle home to search  (default: ~/.gradle)
    --output DIR        Extraction output root  (default: ~/.kotlin-lsp/sources)
    --dry-run           Print what would be extracted without doing it
    -h, --help          Show this help

Example:
    python3 extract-sources.py androidx.compose org.jetbrains.kotlin
    # Then add to your LSP config:
    # sourcePaths = ["~/.kotlin-lsp/sources"]

Output layout:
    ~/.kotlin-lsp/sources/
        androidx.compose.runtime/
            commonMain/
                androidx/compose/runtime/Composable.kt
                ...
        androidx.compose.foundation/
            ...
"""

import argparse
import os
import re
import sys
import zipfile
from pathlib import Path
from typing import Optional


def gradle_home() -> Path:
    env = os.environ.get("GRADLE_USER_HOME")
    if env:
        return Path(env)
    return Path.home() / ".gradle"


def find_source_jars(search_root: Path) -> list[Path]:
    """Recursively find *-sources.jar files under search_root, excluding samples."""
    jars = []
    for p in search_root.rglob("*-sources.jar"):
        if p.is_file() and not p.name.endswith("-samples-sources.jar"):
            jars.append(p)
    return jars


# Gradle module cache layout:
#   <gradle-home>/caches/modules-2/files-2.1/<group>/<artifact>/<version>/<hash>/<artifact>-<version>-sources.jar
_GRADLE_PATH_RE = re.compile(
    r"[/\\]files-2\.1[/\\]"
    r"(?P<group>[^/\\]+)[/\\]"
    r"(?P<artifact>[^/\\]+)[/\\]"
    r"(?P<version>[^/\\]+)[/\\]"
)


def parse_jar_meta(jar: Path) -> Optional[tuple[str, str, str]]:
    """Return (group, artifact, version) parsed from the Gradle cache path, or None."""
    m = _GRADLE_PATH_RE.search(str(jar))
    if m:
        return m.group("group"), m.group("artifact"), m.group("version")
    return None


def version_key(version: str) -> tuple:
    """Comparable key for version strings: split on . and - , cast digits to int."""
    parts = re.split(r"[.\-]", version)
    result = []
    for p in parts:
        if p.isdigit():
            result.append((0, int(p)))
        else:
            result.append((1, p))
    return tuple(result)


def select_latest(jars: list[Path]) -> list[Path]:
    """For each (group, artifact), keep only the JAR with the highest version."""
    # group -> artifact -> version -> jar path
    best: dict[tuple[str, str], tuple[str, Path]] = {}

    ungrouped = []
    for jar in jars:
        meta = parse_jar_meta(jar)
        if meta is None:
            ungrouped.append(jar)
            continue
        group, artifact, version = meta
        key = (group, artifact)
        if key not in best or version_key(version) > version_key(best[key][0]):
            best[key] = (version, jar)

    selected = [v for _, v in best.values()]
    selected.extend(ungrouped)
    return selected


def artifact_dir_name(jar: Path) -> str:
    """Return a short directory name for a JAR's extracted sources."""
    meta = parse_jar_meta(jar)
    if meta:
        group, artifact, _ = meta
        return f"{group}.{artifact}"
    # Fallback: strip -sources.jar suffix
    name = jar.name
    for suffix in ("-sources.jar", ".jar"):
        if name.endswith(suffix):
            name = name[: -len(suffix)]
            break
    return name


def extract_jar(jar: Path, dest: Path, dry_run: bool) -> int:
    """
    Extract .kt and .java files from jar into dest.
    Returns the number of files extracted (or that would be extracted).
    """
    if not zipfile.is_zipfile(jar):
        print(f"  WARNING: not a valid zip file, skipping: {jar}", file=sys.stderr)
        return 0

    count = 0
    with zipfile.ZipFile(jar, "r") as zf:
        for entry in zf.infolist():
            name = entry.filename
            # Skip directories
            if name.endswith("/"):
                continue
            # Only source files
            if not (name.endswith(".kt") or name.endswith(".java")):
                continue
            # Zip-slip protection: ensure extracted path stays inside dest
            target = (dest / name).resolve()
            if not str(target).startswith(str(dest.resolve())):
                print(f"  WARNING: skipping unsafe path: {name}", file=sys.stderr)
                continue
            if dry_run:
                count += 1
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(zf.read(entry))
            count += 1
    return count


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Extract Gradle *-sources.jar files for kotlin-lsp sourcePaths.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage:")[0].strip(),
    )
    parser.add_argument(
        "--gradle-home",
        metavar="DIR",
        type=Path,
        default=None,
        help="Gradle home directory (default: $GRADLE_USER_HOME or ~/.gradle)",
    )
    parser.add_argument(
        "--output",
        metavar="DIR",
        type=Path,
        default=Path.home() / ".kotlin-lsp" / "sources",
        help="Output root directory (default: ~/.kotlin-lsp/sources)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print what would be extracted without doing anything",
    )
    parser.add_argument(
        "patterns",
        nargs="*",
        metavar="PATTERN",
        help="Substring filters on group or artifact (e.g. androidx.compose)",
    )
    args = parser.parse_args()

    search_root = (args.gradle_home or gradle_home()) / "caches" / "modules-2" / "files-2.1"
    if not search_root.exists():
        print(f"ERROR: Gradle cache not found at {search_root}", file=sys.stderr)
        print("Use --gradle-home to specify your Gradle home.", file=sys.stderr)
        sys.exit(1)

    output_root: Path = args.output
    patterns: list[str] = args.patterns
    dry_run: bool = args.dry_run

    print(f"Searching: {search_root}")
    print(f"Output:    {output_root}")
    if patterns:
        print(f"Patterns:  {', '.join(patterns)}")
    if dry_run:
        print("Dry run — no files will be written.")
    print()

    all_jars = find_source_jars(search_root)
    print(f"Found {len(all_jars)} *-sources.jar file(s) total.")

    if patterns:
        def matches(jar: Path) -> bool:
            s = str(jar)
            return any(p in s for p in patterns)

        all_jars = [j for j in all_jars if matches(j)]
        print(f"After filtering: {len(all_jars)} jar(s) match pattern(s).")

    selected = select_latest(all_jars)
    print(f"After dedup (latest version per artifact): {len(selected)} jar(s).\n")

    if not selected:
        print("Nothing to extract.")
        return

    total_files = 0
    extracted_dirs: list[Path] = []

    for jar in sorted(selected):
        dir_name = artifact_dir_name(jar)
        dest = output_root / dir_name
        meta = parse_jar_meta(jar)
        version_str = f"  v{meta[2]}" if meta else ""
        print(f"  {dir_name}{version_str}")
        print(f"    src: {jar}")
        print(f"    dst: {dest}")

        count = extract_jar(jar, dest, dry_run=dry_run)
        print(f"    {'would extract' if dry_run else 'extracted'} {count} file(s)")
        total_files += count
        if dest not in extracted_dirs:
            extracted_dirs.append(dest)
        print()

    print(f"{'Would extract' if dry_run else 'Extracted'} {total_files} file(s) across {len(extracted_dirs)} artifact(s).")

    if not dry_run and extracted_dirs:
        print()
        print("Add to your LSP config (sourcePaths accepts a parent dir):")
        print()
        print(f'  sourcePaths = ["{output_root}"]')
        print()
        print("Or list individual artifact dirs:")
        print()
        for d in sorted(extracted_dirs):
            print(f'    "{d}",')


if __name__ == "__main__":
    main()
