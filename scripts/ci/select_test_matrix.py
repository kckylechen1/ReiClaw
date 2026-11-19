#!/usr/bin/env python3
"""Select CI test matrix phases from a PR's changed workspace crates; fail closed."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys

from change_scope import (
    REGULAR_MODES,
    SHA,
    collect_diff_changes,
    git,
    is_safe_path,
    prose_path,
)


def load_all_matrix(value: str) -> list[dict]:
    text = Path(value[1:]).read_text(encoding="utf-8") if value.startswith("@") else value
    data = json.loads(text)
    if not isinstance(data, list) or not data:
        raise ValueError("all-matrix must be a non-empty JSON array")
    for entry in data:
        if (
            not isinstance(entry, dict)
            or not isinstance(entry.get("leg"), str)
            or not isinstance(entry.get("phase"), str)
        ):
            raise ValueError("all-matrix entries must be objects with string 'leg' and 'phase'")
    return data


def load_metadata(cwd: Path, metadata_file: Path | None) -> dict:
    if metadata_file is not None:
        return json.loads(metadata_file.read_text(encoding="utf-8"))
    proc = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(proc.stdout)


def relative_member_dir(manifest_path: str, workspace_root: str | None, cwd: Path) -> str:
    posix_manifest = PurePosixPath(manifest_path.replace("\\", "/"))
    if not posix_manifest.is_absolute():
        return posix_manifest.parent.as_posix()
    if workspace_root:
        ws_root = PurePosixPath(workspace_root.replace("\\", "/"))
        try:
            return posix_manifest.relative_to(ws_root).parent.as_posix()
        except ValueError:
            pass
    return Path(manifest_path).resolve().relative_to(cwd.resolve()).parent.as_posix()


def workspace_graph(metadata: dict, cwd: Path) -> tuple[dict[str, str], dict[str, set[str]]]:
    if not isinstance(metadata, dict):
        raise ValueError("metadata must be a JSON object")
    member_ids = metadata.get("workspace_members")
    packages = metadata.get("packages")
    if not isinstance(member_ids, list) or not member_ids or not isinstance(packages, list):
        raise ValueError("metadata missing workspace_members or packages")
    member_id_set = set(member_ids)
    workspace_pkgs = [pkg for pkg in packages if pkg.get("id") in member_id_set]
    if len(workspace_pkgs) != len(member_id_set):
        raise ValueError("metadata workspace_members missing from packages")
    workspace_root = metadata.get("workspace_root")
    member_dirs: dict[str, str] = {}
    workspace_names: set[str] = set()
    for pkg in workspace_pkgs:
        name = pkg.get("name")
        manifest_path = pkg.get("manifest_path")
        if not isinstance(name, str) or not isinstance(manifest_path, str):
            raise ValueError("workspace package missing name or manifest_path")
        workspace_names.add(name)
        rel_dir = relative_member_dir(manifest_path, workspace_root, cwd)
        if rel_dir != ".":
            if not is_safe_path(rel_dir):
                raise ValueError(f"unsafe member dir: {rel_dir}")
            member_dirs[name] = rel_dir

    reverse_deps: dict[str, set[str]] = {name: set() for name in workspace_names}
    for pkg in workspace_pkgs:
        pkg_name = pkg["name"]
        for dep in pkg.get("dependencies", []):
            dep_name = dep.get("name")
            if isinstance(dep_name, str) and dep_name in workspace_names:
                reverse_deps[dep_name].add(pkg_name)
    return member_dirs, reverse_deps


def select_matrix(
    event_name: str,
    base_sha: str,
    partition_path: Path,
    all_matrix: list[dict],
    cwd: Path,
    metadata_file: Path | None = None,
) -> list[dict]:
    try:
        if event_name != "pull_request":
            raise ValueError(f"non-PR event: {event_name}")
        if not isinstance(base_sha, str) or not SHA.fullmatch(base_sha) or base_sha == "0" * 40:
            raise ValueError("missing or invalid base SHA")
        resolved = git("rev-parse", "--verify", f"{base_sha}^{{commit}}", cwd=cwd).decode().strip()
        if resolved != base_sha:
            raise ValueError("base is not the specified commit")
        git("merge-base", "--is-ancestor", base_sha, "HEAD", cwd=cwd)
        changes = collect_diff_changes(base_sha, cwd)

        relevant = [
            path
            for path, old, new in changes
            if not (prose_path(path) and old in REGULAR_MODES and new in REGULAR_MODES)
        ]
        if not relevant:
            raise ValueError("no non-prose changes")

        partition = json.loads(partition_path.read_text(encoding="utf-8"))
        legs = partition.get("legs") if isinstance(partition, dict) else None
        if not isinstance(legs, dict) or not legs:
            raise ValueError("invalid test-partition.json legs")

        metadata = load_metadata(cwd, metadata_file)
        member_dirs, reverse_deps = workspace_graph(metadata, cwd)
        if not member_dirs:
            raise ValueError("no non-root workspace members found")

        sorted_members = sorted(member_dirs.items(), key=lambda item: len(item[1]), reverse=True)
        changed_members: set[str] = set()
        for path in relevant:
            match = next(
                ((name, mdir) for name, mdir in sorted_members if path.startswith(f"{mdir}/")),
                None,
            )
            if match is None:
                raise ValueError(f"non-member path changed: {path}")
            pkg_name, mdir = match
            rel_in_member = path[len(mdir) + 1 :]
            if rel_in_member in {"Cargo.toml", "build.rs"}:
                raise ValueError(f"build/manifest input changed: {path}")
            changed_members.add(pkg_name)
            # Root `tests/architecture` (app leg) cross-scans `bridges/**` and
            # all `.rs` files across workspace crates (`crates/**`, `bridges/**`).
            if path.startswith("bridges/") or path.endswith(".rs"):
                changed_members.add("reiclaw")

        affected = set(changed_members)
        queue = list(changed_members)
        while queue:
            current = queue.pop()
            for dependent in reverse_deps.get(current, ()):
                if dependent not in affected:
                    affected.add(dependent)
                    queue.append(dependent)

        selected: list[dict] = []
        for entry in all_matrix:
            leg_name = entry["leg"]
            leg_packages = legs.get(leg_name)
            if not isinstance(leg_packages, list):
                raise ValueError(f"unknown leg in matrix: {leg_name}")
            if any(pkg in affected for pkg in leg_packages):
                selected.append(entry)

        if not selected:
            raise ValueError("empty matrix selection")
        return selected
    except (KeyError, TypeError, ValueError, UnicodeError, OSError, subprocess.CalledProcessError) as error:
        print(f"Test matrix scoped selection fell back to full matrix: {error}", file=sys.stderr)
        return all_matrix


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--base-sha", default="")
    parser.add_argument("--partition", type=Path, default=Path("dev/ci/test-partition.json"))
    parser.add_argument("--all-matrix", required=True)
    parser.add_argument("--metadata-file", type=Path, default=None)
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args(argv)

    all_matrix = load_all_matrix(args.all_matrix)
    selected = select_matrix(
        args.event,
        args.base_sha,
        args.partition,
        all_matrix,
        Path.cwd(),
        metadata_file=args.metadata_file,
    )
    encoded = json.dumps(selected, separators=(",", ":"))
    if args.github_output and os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as target:
            target.write(f"matrix={encoded}\n")
    print(encoded)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
