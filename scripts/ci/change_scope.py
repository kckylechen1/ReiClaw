#!/usr/bin/env python3
"""Classify the checked-out merge result; uncertain diffs keep every check."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys


SHA = re.compile(r"[0-9a-f]{40}\Z")
REGULAR_MODES = {"000000", "100644"}
REGULAR_FILE_MODES = {"000000", "100644", "100755"}
PROSE_FILES = {
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    ".github/pull_request_template.md",
}
INSTALLER_PATH = re.compile(
    r"(^|/)Cargo\.toml$|^Cargo\.lock$|^xtask/|^dev/ci/|^scripts/ci/install|^install\.sh$|^\.cargo/config\.toml$|^\.github/workflows/ci\.yml$"
)
WEB_PATH = re.compile(
    r"^package\.json$|^package-lock\.json$|^npm-shrinkwrap\.json$|^pnpm-lock\.yaml$|^yarn\.lock$|^\.npmrc$|^web/|^\.nvmrc$|^crates/reiclaw-gateway/|^\.github/workflows/ci\.yml$"
)
DOC_BUILD_FILES = {
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    ".github/workflows/ci.yml",
    "docs/book/book.toml",
    "docs/book/src/SUMMARY.md",
    "locales.toml",
    "src/main.rs",
    "web/src/contexts/themes.json",
}
DOC_BUILD_PREFIXES = (
    "xtask/",
    "docs/book/theme/",
    "docs/book/po/",
    "crates/reiclaw-schema-markdown/",
    "crates/reiclaw-config/",
    "scripts/ci/docs",
    "scripts/ci/test_docs_",
    "scripts/ci/collect_changed_links.py",
)


def git(*args: str, cwd: Path) -> bytes:
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True
    ).stdout


def event_base(event_name: str, event: dict) -> str:
    if event_name == "pull_request":
        return event["pull_request"]["base"]["sha"]
    if event_name == "push":
        return event["before"]
    if event_name == "merge_group":
        return event["merge_group"]["base_sha"]
    raise ValueError(f"unsupported event: {event_name}")


def is_safe_path(path: str) -> bool:
    if not path or "\n" in path or "\r" in path or "\0" in path:
        return False
    parsed = PurePosixPath(path)
    return (
        not parsed.is_absolute()
        and ".." not in parsed.parts
        and path == parsed.as_posix()
    )


def prose_path(path: str) -> bool:
    # Runtime Markdown (personality templates, prompts and fixtures), nested
    # crate AGENTS.md files, agent policy, build/config files, and every
    # unknown path retain the full lane.
    if not is_safe_path(path):
        return False
    parsed = PurePosixPath(path)
    return path in PROSE_FILES or (
        path.startswith("docs/book/src/") and parsed.suffix in {".md", ".mdx"}
    )


def build_path(path: str) -> bool:
    return (
        path in DOC_BUILD_FILES
        or path.startswith(DOC_BUILD_PREFIXES)
        or (path.startswith("docs/book/") and not prose_path(path))
    )


def contains_preprocessor_input(path: str, base: str, cwd: Path) -> bool:
    # The source gate does not execute mdBook's custom preprocessors. Inspect
    # both versions so adding, editing or deleting a directive reaches the real
    # renderer; never implement a second, partial peer-groups DSL here.
    if not path.startswith("docs/book/src/") or not prose_path(path):
        return False
    for ref in (base, "HEAD"):
        present = subprocess.run(
            ["git", "cat-file", "-e", f"{ref}:{path}"], cwd=cwd, capture_output=True
        )
        if present.returncode != 0:
            continue  # Additions/deletions legitimately exist at only one end.
        if re.search(rb"\{\{\s*#", git("show", f"{ref}:{path}", cwd=cwd)):
            return True
    return False


def parse_change_records(raw: bytes) -> list[tuple[str, str, str, str]]:
    """Use NUL-delimited raw diffs so renames and unusual names cannot hide code."""
    fields = raw.split(b"\0")
    if fields.pop() != b"" or len(fields) % 2:
        raise ValueError("malformed git diff")
    changes = []
    for offset in range(0, len(fields), 2):
        header = fields[offset].decode("ascii").split()
        if len(header) != 5 or not header[0].startswith(":") or header[4] not in {"A", "M", "D", "T"}:
            raise ValueError("unexpected raw diff record")
        old_mode, new_mode, status = header[0][1:], header[1], header[4]
        path = fields[offset + 1].decode("utf-8", errors="strict")
        if not path:
            raise ValueError("empty changed path")
        changes.append((path, old_mode, new_mode, status))
    return changes


def parse_changes(raw: bytes) -> list[tuple[str, str, str]]:
    return [(path, old, new) for path, old, new, _ in parse_change_records(raw)]


def collect_diff_changes(base: str, cwd: Path) -> list[tuple[str, str, str]]:
    """Collect verified regular-file changes; fail closed on renames or D+A pairs."""
    raw = git("diff", "--no-ext-diff", "--raw", "-z", "-M", base, "HEAD", "--", cwd=cwd)
    records = parse_change_records(raw)
    if not records:
        raise ValueError("empty comparison; retaining full validation")
    statuses = {status for _, _, _, status in records}
    if "D" in statuses and "A" in statuses:
        raise ValueError("diff contains both deletion and addition; treating as potential rename")
    changes: list[tuple[str, str, str]] = []
    for path, old, new, _ in records:
        if (
            not is_safe_path(path)
            or old not in REGULAR_FILE_MODES
            or new not in REGULAR_FILE_MODES
        ):
            raise ValueError("unsafe path or non-regular mode; retaining full validation")
        changes.append((path, old, new))
    return changes


def classify(event_name: str, event: dict, cwd: Path) -> dict[str, str]:
    # The conservative default is also used for force pushes, missing event
    # payloads and incomplete clones. Never turn a diff error into docs-only.
    result = {
        "full": "true",
        "docs_build": "true",
        "base_sha": "",
        "installer": "true",
        "web": "true",
    }
    try:
        base = event_base(event_name, event)
        if not isinstance(base, str) or not SHA.fullmatch(base) or base == "0" * 40:
            raise ValueError("missing or invalid base SHA")
        resolved = git("rev-parse", "--verify", f"{base}^{{commit}}", cwd=cwd).decode().strip()
        if resolved != base:
            raise ValueError("base is not the specified commit")
        git("merge-base", "--is-ancestor", base, "HEAD", cwd=cwd)
        changes = collect_diff_changes(base, cwd)
        full = any(
            not prose_path(path) or old not in REGULAR_MODES or new not in REGULAR_MODES
            for path, old, new in changes
        )
        docs_build = any(
            build_path(path) or contains_preprocessor_input(path, base, cwd)
            for path, _, _ in changes
        )
        if event_name == "pull_request":
            installer = any(INSTALLER_PATH.search(path) for path, _, _ in changes)
            web = any(WEB_PATH.search(path) for path, _, _ in changes)
        else:
            installer = True
            web = True
        result.update(
            full=str(full).lower(),
            docs_build=str(docs_build).lower(),
            base_sha=base,
            installer=str(installer).lower(),
            web=str(web).lower(),
        )
        print(
            f"Changed paths: {len(changes)}; full={result['full']}; "
            f"docs_build={result['docs_build']}; installer={result['installer']}; "
            f"web={result['web']}"
        )
    except (KeyError, TypeError, ValueError, UnicodeError, subprocess.CalledProcessError) as error:
        print(f"Scope could not be established; running full validation: {error}", file=sys.stderr)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    args = parser.parse_args()
    try:
        event = json.loads(Path(args.event_path).read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"Cannot read event payload; retaining full validation: {error}", file=sys.stderr)
        event = {}
    result = classify(args.event, event, Path.cwd())
    output = "".join(f"{key}={value}\n" for key, value in result.items())
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as target:
            target.write(output)
    print(output, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
