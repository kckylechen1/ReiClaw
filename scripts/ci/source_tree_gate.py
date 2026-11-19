#!/usr/bin/env python3
"""Mechanical checks on the shape of the Rust source tree.

Each check turns a rule that review used to enforce by hand into a failure
whose message names the fix (AGENTS.md rule 10):

* orphans: every ``.rs`` file under a package's ``src/``, ``tests/``,
  ``benches/`` or ``examples/`` (and its ``build.rs``) must be reachable from
  a Cargo target root through ``mod`` declarations, ``#[path]`` attributes or
  ``include!``. A cfg-gated module counts as reachable, so feature-gated code
  is never reported.
* silencing: no module-wide ``allow`` of ``dead_code`` or ``unused*`` lints,
  either as an inner ``#![allow(..)]`` or on a ``mod`` item (AGENTS.md rule 7:
  remove unused code rather than silencing it).
* crates: the workspace members match ``APPROVED_CRATES`` (ADR-026 §5 rule 1)
  and every package inherits ``[workspace.lints]``.

Usage: ``python3 scripts/ci/source_tree_gate.py [--root DIR]``.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

# ADR-026 §5 rule 1: a crate needs two distinct consumers or a hard dependency
# boundary, and the budget is 18 crates or fewer. Adding a member means naming
# its boundary in the PR; folding one (ADR-026 S6) removes its entry here.
APPROVED_CRATES = frozenset(
    {
        "bridges/acp",
        "bridges/telegram",
        "crates/reiclaw-api",
        "crates/reiclaw-config",
        "crates/reiclaw-cost",
        "crates/reiclaw-eval",
        "crates/reiclaw-gateway",
        "crates/reiclaw-gateway-client",
        "crates/reiclaw-http",
        "crates/reiclaw-infra",
        "crates/reiclaw-log",
        "crates/reiclaw-macros",
        "crates/reiclaw-memory",
        "crates/reiclaw-platform",
        "crates/reiclaw-policy",
        "crates/reiclaw-provider-dispatch",
        "crates/reiclaw-providers",
        "crates/reiclaw-runtime",
        "crates/reiclaw-schema-markdown",
        "crates/reiclaw-spawn",
        "crates/reiclaw-tool-call-parser",
        "crates/reiclaw-tools",
        "tools/fill-translations",
        "xtask",
    }
)

# Files allowed to silence dead or unused code module-wide, with the condition
# that retires each entry. An entry whose file no longer silences anything
# fails the gate, so the list only shrinks.
SILENCING_EXCEPTIONS = {
    "src/main.rs": "the binary re-declares the library's modules; "
    "remove once main.rs consumes the reiclaw library crate",
    "tests/support/mod.rs": "shared helpers compiled into several test "
    "binaries, each of which uses a subset",
}

SOURCE_DIRS = ("src", "tests", "benches", "examples")
ROOT_GLOBS = (
    "src/lib.rs",
    "src/main.rs",
    "src/bin/*.rs",
    "src/bin/*/main.rs",
    "tests/*.rs",
    "tests/*/main.rs",
    "benches/*.rs",
    "benches/*/main.rs",
    "examples/*.rs",
    "examples/*/main.rs",
)

ATTR = r"#\[[^\]]*\]"
MOD_ITEM = re.compile(
    rf"((?:{ATTR}\s*)*)(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*([;{{])"
)
ATTR_RE = re.compile(ATTR)
PATH_VALUE = re.compile(r'path\s*=\s*"([^"]+)"')
INCLUDE = re.compile(r'\binclude!\s*\(\s*"([^"]+)"\s*\)')
INNER_ALLOW = re.compile(r"#!\[\s*(?:cfg_attr\s*\((?:[^()]|\([^()]*\))*?,\s*)?allow\s*\(([^)]*)\)")
OUTER_ALLOW = re.compile(r"allow\s*\(([^)]*)\)")
SILENCED_LINT = re.compile(r"^(?:dead_code|unused(?:_\w+)?)$")
RAW_STRING = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'")


@dataclass(frozen=True)
class Violation:
    check: str
    path: str
    message: str


def _neutralize(literal: str) -> str:
    # Keep string contents for `#[path]` and `include!`, but stop braces and
    # semicolons inside literals from looking like code.
    return literal.replace("{", " ").replace("}", " ").replace(";", " ")


def strip_source(text: str) -> str:
    """Drop comments and neutralize literals so braces and `mod` items are code."""
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
            continue
        if text.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if text.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif text.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
            continue
        boundary = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        raw = RAW_STRING.match(text, i) if boundary else None
        if raw:
            close = '"' + raw.group(1)
            end = text.find(close, raw.end())
            end = n if end < 0 else end + len(close)
            out.append(_neutralize(text[i:end]))
            i = end
            continue
        if text[i] == '"' or (boundary and text[i] in "bc" and text.startswith('"', i + 1)):
            j = i + (1 if text[i] == '"' else 2)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(_neutralize(text[i : j + 1]))
            i = j + 1
            continue
        if text[i] == "'":
            char = CHAR_LITERAL.match(text, i)
            if char:
                out.append("' '")
                i = char.end()
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


class Workspace:
    def __init__(self, root: Path):
        self.root = root.resolve()
        manifest = tomllib.loads((self.root / "Cargo.toml").read_text())
        self.members = [str(m) for m in manifest.get("workspace", {}).get("members", [])]
        self.package_dirs = [self.root] + [self.root / m for m in self.members]
        self._stripped: dict[Path, str] = {}

    def stripped(self, path: Path) -> str:
        if path not in self._stripped:
            self._stripped[path] = strip_source(path.read_text(errors="replace"))
        return self._stripped[path]

    def rel(self, path: Path) -> str:
        return path.relative_to(self.root).as_posix()

    # ── orphans ──────────────────────────────────────────────────────────

    def target_roots(self, package: Path) -> set[Path]:
        manifest = tomllib.loads((package / "Cargo.toml").read_text())
        roots = {p for pattern in ROOT_GLOBS for p in package.glob(pattern)}
        lib = manifest.get("lib", {})
        if "path" in lib:
            roots.add(package / lib["path"])
        for kind in ("bin", "test", "bench", "example"):
            for target in manifest.get(kind, []):
                if "path" in target:
                    roots.add(package / target["path"])
        build = manifest.get("package", {}).get("build")
        if isinstance(build, str):
            roots.add(package / build)
        elif build is not False and (package / "build.rs").is_file():
            roots.add(package / "build.rs")
        return {r.resolve() for r in roots if r.is_file()}

    def candidates(self, package: Path) -> set[Path]:
        package = package.resolve()
        # Skip files that belong to a package nested inside this one.
        nested = {d.resolve() for d in self.package_dirs if package in d.resolve().parents}
        found: set[Path] = set()
        for sub in SOURCE_DIRS:
            for path in (package / sub).rglob("*.rs"):
                path = path.resolve()
                if any(parent in nested for parent in path.parents):
                    continue
                found.add(path)
        if (package / "build.rs").is_file():
            found.add((package / "build.rs").resolve())
        return found

    def _visit(self, path: Path, reached: set[Path], mod_rs: bool) -> None:
        path = path.resolve()
        if path in reached or not path.is_file():
            return
        reached.add(path)
        text = self.stripped(path)
        here = path.parent
        # Child modules of a mod-rs file (crate root or mod.rs) live beside it;
        # those of any other file live in a directory named after it.
        base = here if mod_rs else here / path.stem

        events: list[tuple[int, int, re.Match[str]]] = []
        events += [(m.start(), 0, m) for m in MOD_ITEM.finditer(text)]
        events += [(m.start(), 0, m) for m in INCLUDE.finditer(text)]
        events += [(m.start(), 1, m) for m in re.finditer(r"[{}]", text)]
        events.sort(key=lambda e: (e[0], e[1]))

        depth = 0
        inline: list[tuple[int, str]] = []
        pending: tuple[int, str] | None = None
        for _, kind, match in events:
            if kind == 1:
                if match.group(0) == "{":
                    depth += 1
                    if pending and pending[0] == match.start():
                        inline.append((depth, pending[1]))
                        pending = None
                else:
                    if inline and inline[-1][0] == depth:
                        inline.pop()
                    depth -= 1
                continue
            if match.re is INCLUDE:
                self._visit(here / match.group(1), reached, mod_rs=True)
                continue
            attrs, name, terminator = match.group(1), match.group(2), match.group(3)
            if terminator == "{":
                pending = (match.end() - 1, name)
                continue
            nesting = [n for _, n in inline]
            fixed, conditional = [], []
            for attr in ATTR_RE.findall(attrs or ""):
                value = PATH_VALUE.search(attr)
                if value:
                    (conditional if attr.lstrip("#[ ").startswith("cfg_attr") else fixed).append(
                        value.group(1)
                    )
            # A top-level `#[path]` is relative to this file's directory; inside
            # inline modules it is relative to the module directory. rustc
            # treats every `#[path]` file as mod-rs for its own children.
            path_dir = base.joinpath(*nesting) if nesting else here
            for value in fixed or conditional:
                self._visit(path_dir / value, reached, mod_rs=True)
            if not fixed:
                module_dir = base.joinpath(*nesting)
                flat = module_dir / f"{name}.rs"
                if flat.is_file():
                    self._visit(flat, reached, mod_rs=False)
                else:
                    self._visit(module_dir / name / "mod.rs", reached, mod_rs=True)

    def orphans(self) -> list[Violation]:
        violations = []
        for package in self.package_dirs:
            reached: set[Path] = set()
            for root in sorted(self.target_roots(package)):
                self._visit(root, reached, mod_rs=True)
            for path in sorted(self.candidates(package) - reached):
                violations.append(
                    Violation(
                        "orphans",
                        self.rel(path),
                        "no `mod`, `#[path]` or `include!` reaches this file, so it is never "
                        "compiled. Delete it, or mount it from its parent module.",
                    )
                )
        return violations

    # ── silencing ────────────────────────────────────────────────────────

    def silencing(self) -> list[Violation]:
        violations = []
        silenced_files = set()
        for package in self.package_dirs:
            for path in sorted(self.candidates(package)):
                text = self.stripped(path)
                lints = [lint for m in INNER_ALLOW.finditer(text) for lint in _lints(m.group(1))]
                for m in MOD_ITEM.finditer(text):
                    for attr in ATTR_RE.findall(m.group(1) or ""):
                        lints += [lint for a in OUTER_ALLOW.finditer(attr) for lint in _lints(a.group(1))]
                silenced = sorted({lint for lint in lints if SILENCED_LINT.match(lint)})
                if not silenced:
                    continue
                rel = self.rel(path)
                silenced_files.add(rel)
                if rel in SILENCING_EXCEPTIONS:
                    continue
                violations.append(
                    Violation(
                        "silencing",
                        rel,
                        f"module-wide allow of {', '.join(silenced)} hides dead code. Remove the "
                        "unused items, or put a scoped #[expect(..., reason = \"...\")] on the "
                        "item that needs it.",
                    )
                )
        for rel in sorted(set(SILENCING_EXCEPTIONS) - silenced_files):
            violations.append(
                Violation(
                    "silencing",
                    rel,
                    "no longer silences dead or unused code; remove its entry from "
                    "SILENCING_EXCEPTIONS in scripts/ci/source_tree_gate.py.",
                )
            )
        return violations

    # ── crates ───────────────────────────────────────────────────────────

    def crates(self) -> list[Violation]:
        violations = []
        members = set(self.members)
        for member in sorted(members - APPROVED_CRATES):
            violations.append(
                Violation(
                    "crates",
                    member,
                    "is not an approved workspace crate. ADR-026 §5 rule 1: a crate needs two "
                    "distinct consumers or a hard dependency boundary. Put the code in an "
                    "existing crate, or name the boundary in the PR and add the crate to "
                    "APPROVED_CRATES in scripts/ci/source_tree_gate.py.",
                )
            )
        for member in sorted(APPROVED_CRATES - members):
            violations.append(
                Violation(
                    "crates",
                    member,
                    "is no longer a workspace member; remove it from APPROVED_CRATES in "
                    "scripts/ci/source_tree_gate.py.",
                )
            )
        for package in self.package_dirs:
            manifest = tomllib.loads((package / "Cargo.toml").read_text())
            if manifest.get("lints", {}).get("workspace") is not True:
                violations.append(
                    Violation(
                        "crates",
                        self.rel(package / "Cargo.toml"),
                        "does not inherit the workspace lints; add `[lints]` with "
                        "`workspace = true`.",
                    )
                )
        return violations


def _lints(listing: str) -> list[str]:
    return [
        item.strip()
        for item in listing.split(",")
        if item.strip() and "=" not in item
    ]


def run(root: Path) -> list[Violation]:
    workspace = Workspace(root)
    return workspace.orphans() + workspace.silencing() + workspace.crates()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args(argv)
    violations = run(args.root)
    for v in violations:
        print(f"::error file={v.path}::[{v.check}] {v.path}: {v.message}")
    if violations:
        print(f"source tree gate: {len(violations)} violation(s)", file=sys.stderr)
        return 1
    print("source tree gate: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
