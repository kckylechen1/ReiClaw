#!/usr/bin/env python3
"""Mechanical checks on the shape of the Rust source tree.

Each check turns a rule that review used to enforce by hand into a failure
whose message names the fix (AGENTS.md rule 10):

* orphans: every ``.rs`` file under a package's ``src/``, ``tests/``,
  ``benches/`` or ``examples/`` (and its ``build.rs``) must be reachable from
  a Cargo target root through ``mod`` declarations, ``#[path]`` attributes or
  ``include!``. A cfg-gated module counts as reachable, so feature-gated code
  is never reported.
* silencing: no module-wide ``allow`` or ``expect`` of ``dead_code`` or
  ``unused*`` lints, either as a module-scope inner attribute (directly or
  through ``cfg_attr``) or on a ``mod`` item (AGENTS.md rule 7: remove unused
  code rather than silencing it).
* crates: the workspace members match ``APPROVED_CRATES`` (ADR-026 §5 rule 1)
  and every package inherits ``[workspace.lints]``.

Usage: ``python3 scripts/ci/source_tree_gate.py [--root DIR]``.
"""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from dataclasses import dataclass, field
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
        "crates/reiclaw-eval",
        "crates/reiclaw-gateway",
        "crates/reiclaw-gateway-client",
        "crates/reiclaw-http",
        "crates/reiclaw-infra",
        "crates/reiclaw-log",
        "crates/reiclaw-macros",
        "crates/reiclaw-memory",
        "crates/reiclaw-policy",
        "crates/reiclaw-provider-dispatch",
        "crates/reiclaw-providers",
        "crates/reiclaw-runtime",
        "crates/reiclaw-tool-call-parser",
        "crates/reiclaw-tools",
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
# Cargo target kinds: manifest table, auto-discovery flag, directory.
TARGET_KINDS = (
    ("bin", "autobins", "src/bin"),
    ("test", "autotests", "tests"),
    ("bench", "autobenches", "benches"),
    ("example", "autoexamples", "examples"),
)

RAW_STRING = re.compile(r'(?:b|c)?r(#*)"')
CHAR_LITERAL = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'")
# Identifiers: ASCII word characters plus any non-ASCII character other than
# Rust's non-ASCII whitespace, a superset of XID that rustc narrows itself.
IDENT_CHAR = r"(?:\w|[^\x00-\x7f\u0085\u200e\u200f\u2028\u2029])"
# Matched against lexed source, where every string literal is a `"N"`
# placeholder indexing the literal values.
TOKEN = re.compile(
    r"(?P<attr>#!?\[)"
    # `#` or `'` before the keyword makes it a raw identifier or raw label.
    rf"|(?P<mod>\b(?:pub(?:\s*\([^)]*\))?\s+)?(?<![#'])mod\s+(?:r#)?"
    rf"(?P<name>(?!\d){IDENT_CHAR}+)\s*(?P<term>[;{{]))"
    r'|(?P<include>\binclude!\s*\(\s*"(?P<lit>\d+)"\s*\))'
    r"|(?P<brace>[{}])"
)
PATH_ATTR = re.compile(r'#\[\s*path\s*=\s*"(\d+)"\s*\]')
CFG_ATTR = re.compile(r"#\[\s*cfg_attr\b")
PATH_VALUE = re.compile(r'\bpath\s*=\s*"(\d+)"')
LINT_LEVEL = re.compile(r"\b(?:allow|expect)\s*\(")
SILENCED_LINT = re.compile(r"^(?:dead_code|unused(?:_\w+)?)$")


@dataclass(frozen=True)
class Violation:
    check: str
    path: str
    message: str


def lex(text: str) -> tuple[str, list[str]]:
    """Blank comments and replace each string literal with a `"N"` placeholder.

    Returns the code and the literal values, so attribute and macro arguments
    stay readable while nothing inside a literal or comment looks like code.
    """
    out: list[str] = []
    literals: list[str] = []
    i, n = 0, len(text)

    def literal(value: str) -> None:
        out.append(f'"{len(literals)}"')
        literals.append(value)

    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            i = n if end < 0 else end
            out.append(" ")
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
            out.append(" ")
            continue
        boundary = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
        raw = RAW_STRING.match(text, i) if boundary else None
        if raw:
            close = '"' + raw.group(1)
            end = text.find(close, raw.end())
            end = n if end < 0 else end
            literal(text[raw.end() : end])
            i = min(n, end + len(close))
            continue
        if text[i] == '"' or (boundary and text[i] in "bc" and text.startswith('"', i + 1)):
            start = i + (1 if text[i] == '"' else 2)
            j = start
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            literal(text[start:j])
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
    return "".join(out), literals


def _close(text: str, start: int, opening: str, closing: str) -> int:
    depth = 0
    for i in range(start, len(text)):
        if text[i] == opening:
            depth += 1
        elif text[i] == closing:
            depth -= 1
            if depth == 0:
                return i
    return len(text) - 1


def silenced_lints(attr: str) -> set[str]:
    """Dead or unused lints an attribute allows or expects, through any cfg_attr nesting."""
    found = set()
    for level in LINT_LEVEL.finditer(attr):
        close = _close(attr, level.end() - 1, "(", ")")
        for item in attr[level.end() : close].split(","):
            if SILENCED_LINT.match(item.strip()):
                found.add(item.strip())
    return found


@dataclass
class FileScan:
    children: list[tuple[Path, bool]] = field(default_factory=list)
    silenced: set[str] = field(default_factory=set)


class Workspace:
    def __init__(self, root: Path):
        self.root = root.resolve()
        manifest = tomllib.loads((self.root / "Cargo.toml").read_text())
        self.members = [str(m) for m in manifest.get("workspace", {}).get("members", [])]
        self.package_dirs = [self.root] + [(self.root / m).resolve() for m in self.members]
        self._lexed: dict[Path, tuple[str, list[str]]] = {}

    def rel(self, path: Path) -> str:
        return path.relative_to(self.root).as_posix()

    def scan(self, path: Path, mod_rs: bool) -> FileScan:
        """Child module files of `path` and the lints it silences module-wide.

        Follows rustc: children of a mod-rs file (crate root or mod.rs) live
        beside it, those of any other file in a directory named after it. A
        `#[path]` outside inline modules is relative to the file's directory;
        inside them, to the inline module's directory. rustc treats every
        `#[path]` file as mod-rs for its own children.
        """
        if path not in self._lexed:
            self._lexed[path] = lex(path.read_text(errors="replace"))
        text, literals = self._lexed[path]
        here = path.parent
        base = here if mod_rs else here / path.stem
        result = FileScan()
        depth = 0
        inline: list[tuple[int, Path]] = []  # (body depth, directory for children)
        attrs: list[str] = []
        attrs_end = 0
        pos = 0
        while (match := TOKEN.search(text, pos)) is not None:
            if attrs and text[attrs_end : match.start()].strip():
                attrs = []
            if match.group("attr"):
                end = _close(text, match.end() - 1, "[", "]")
                attr = text[match.start() : end + 1]
                pos = end + 1
                if attr.startswith("#!"):
                    # Inner attributes of a fn or impl body are not module-wide.
                    if depth == (inline[-1][0] if inline else 0):
                        result.silenced |= silenced_lints(attr)
                else:
                    attrs.append(attr)
                    attrs_end = pos
                continue
            pos = match.end()
            if match.group("brace"):
                if match.group("brace") == "{":
                    depth += 1
                else:
                    if inline and inline[-1][0] == depth:
                        inline.pop()
                    depth -= 1
                attrs = []
                continue
            if match.group("include"):
                result.children.append((here / literals[int(match.group("lit"))], True))
                attrs = []
                continue

            mod_attrs, attrs = attrs, []
            fixed, conditional = [], []
            for attr in mod_attrs:
                result.silenced |= silenced_lints(attr)
                if path_attr := PATH_ATTR.fullmatch(attr):
                    fixed.append(literals[int(path_attr.group(1))])
                elif CFG_ATTR.match(attr):
                    conditional += [literals[int(v)] for v in PATH_VALUE.findall(attr)]
            context = inline[-1][1] if inline else None
            name = match.group("name")
            if match.group("term") == "{":
                depth += 1
                directory = (context or here) / fixed[0] if fixed else (context or base) / name
                inline.append((depth, directory))
                continue
            for value in fixed or conditional:
                result.children.append(((context or here) / value, True))
            if not fixed:
                module_dir = context or base
                flat = module_dir / f"{name}.rs"
                if flat.is_file():
                    result.children.append((flat, False))
                else:
                    result.children.append((module_dir / name / "mod.rs", True))
        return result

    # ── orphans ──────────────────────────────────────────────────────────

    def target_roots(self, package: Path) -> set[Path]:
        """Cargo target roots, with its auto-discovery and explicit targets."""
        manifest = tomllib.loads((package / "Cargo.toml").read_text())
        meta = manifest.get("package", {})

        def auto(flag: str) -> bool:
            return meta.get(flag, True) is not False

        roots: list[Path] = []
        # Disabling discovery never drops an explicitly declared target. All
        # packages here are edition 2024, so 2015's legacy paths do not apply.
        lib = manifest.get("lib")
        if lib is not None and "path" in lib:
            roots.append(package / lib["path"])
        elif lib is not None or auto("autolib"):
            roots.append(package / "src/lib.rs")
        if auto("autobins"):
            roots.append(package / "src/main.rs")
        for kind, flag, directory in TARGET_KINDS:
            for target in manifest.get(kind, []):
                if "path" in target:
                    roots.append(package / target["path"])
                elif "name" in target:
                    name = target["name"]
                    roots += [package / directory / f"{name}.rs", package / directory / name / "main.rs"]
                    if kind == "bin" and name == meta.get("name"):
                        roots.append(package / "src/main.rs")
            if auto(flag):
                roots += (package / directory).glob("*.rs")
                roots += (package / directory).glob("*/main.rs")
        build = meta.get("build")
        if isinstance(build, str):
            roots.append(package / build)
        elif build is not False:
            roots.append(package / "build.rs")
        return {r.resolve() for r in roots if r.is_file()}

    def candidates(self, package: Path) -> set[Path]:
        package = package.resolve()
        # Skip files that belong to a package nested inside this one.
        nested = {d for d in self.package_dirs if package in d.parents}
        found: set[Path] = set()
        for sub in SOURCE_DIRS:
            for path in (package / sub).rglob("*.rs"):
                path = path.resolve()
                if not any(parent in nested for parent in path.parents):
                    found.add(path)
        if (package / "build.rs").is_file():
            found.add((package / "build.rs").resolve())
        return found

    def _visit(self, path: Path, reached: set[Path], mod_rs: bool) -> None:
        path = path.resolve()
        if path in reached or not path.is_file():
            return
        reached.add(path)
        for child, child_mod_rs in self.scan(path, mod_rs).children:
            self._visit(child, reached, child_mod_rs)

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
                silenced = sorted(self.scan(path, mod_rs=True).silenced)
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
                        f"module-wide allow or expect of {', '.join(silenced)} hides dead code. "
                        "Remove the unused items, or put a scoped "
                        "#[expect(..., reason = \"...\")] on the item that needs it.",
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
