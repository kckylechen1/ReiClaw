#!/usr/bin/env python3
"""Check the docs index (SUMMARY.md) and local link targets of the repository docs.

The docs are plain Markdown read in the repository; nothing renders them, so
mdBook directives are rejected. Anchors and remote availability are outside this
gate's coverage.
"""

from __future__ import annotations

import argparse
from html.parser import HTMLParser
from pathlib import Path
import re
import sys
from urllib.parse import unquote, urlsplit

try:
    from markdown_it import MarkdownIt
except ImportError as exc:
    raise SystemExit(
        "Docs checks require: python3 -m pip install -r scripts/ci/docs-requirements.txt"
    ) from exc


# `{{#include}}`, `{{#config-fields}}` and the other mdBook directives render as
# literal text in plain Markdown. Code spans and blocks may show template syntax.
DIRECTIVE_RE = re.compile(r"\{\{\s*#")
MARKDOWN = MarkdownIt("commonmark")


class DocsError(ValueError):
    pass


class HtmlLinks(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        for name, value in attrs:
            if value is not None and ((tag == "a" and name == "href") or name == "src"):
                self.links.append(value)


def markdown_links(text: str) -> list[str]:
    """Use CommonMark tokens so examples, escaped syntax and references stay distinct."""
    links: list[str] = []

    def visit(tokens) -> None:
        for token in tokens:
            if token.type == "link_open":
                links.append(token.attrGet("href") or "")
            elif token.type == "image":
                links.append(token.attrGet("src") or "")
            elif token.type in {"html_inline", "html_block"}:
                html = HtmlLinks()
                html.feed(token.content)
                html.close()
                links.extend(html.links)
            if token.children:
                visit(token.children)

    visit(MARKDOWN.parse(text))
    return links


def has_directive(text: str) -> bool:
    def visit(tokens) -> bool:
        for token in tokens:
            if token.type in {"fence", "code_block", "code_inline"}:
                continue
            if token.children is not None:
                if visit(token.children):
                    return True
            elif DIRECTIVE_RE.search(token.content):
                return True
        return False

    return visit(MARKDOWN.parse(text))


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise DocsError(f"Cannot read {path}: {exc}") from exc


def remote_link(target: str) -> bool:
    return target.startswith("//") or urlsplit(target).scheme.lower() in {"http", "https", "mailto", "tel"}


class BookSources:
    def __init__(self, repo: Path) -> None:
        self.repo = repo.resolve()
        self.source = (self.repo / "docs/book/src").resolve()
        if not self.source.is_relative_to(self.repo) or not self.source.is_dir():
            raise DocsError(f"Docs source must be an existing directory in the repository: {self.source}")
        self.published: set[Path] = set()
        self.outputs: dict[Path, Path] = {}
        self.checked_links = 0

    def load_summary(self) -> None:
        summary = self.source / "SUMMARY.md"
        for target in markdown_links(read_text(summary)):
            if not target:  # mdBook permits label-only/draft navigation entries.
                continue
            url = urlsplit(target)
            if url.scheme or url.netloc or url.query or url.fragment or url.path.startswith("/"):
                raise DocsError(f"SUMMARY.md chapter must be a source-relative Markdown path: {target}")
            path = (self.source / unquote(url.path)).resolve()
            if not path.is_relative_to(self.source) or path.suffix not in {".md", ".mdx"}:
                raise DocsError(f"SUMMARY.md chapter escapes the book or is not Markdown: {target}")
            if path in self.published:
                raise DocsError(f"SUMMARY.md lists a chapter twice: {target}")
            if not path.is_file():
                raise DocsError(f"SUMMARY.md chapter is missing: {target}")
            self.published.add(path)
            output = path.with_name("index.html") if path.name == "README.md" else path.with_suffix(".html")
            if output in self.outputs:
                raise DocsError(f"SUMMARY.md chapters produce the same HTML path: {target}")
            self.outputs[output] = path
        if not self.published:
            raise DocsError("SUMMARY.md has no publishable chapters")

    def check_target(self, source: Path, target: str, *, in_book: bool) -> None:
        if not target or remote_link(target):
            return
        url = urlsplit(target)
        if url.scheme or url.netloc:
            raise DocsError(f"Unsupported local link: {source} -> {target}")
        if not url.path:  # Anchor existence needs rendered output, not a source-path guess.
            return
        if in_book and url.path.startswith("/"):
            raise DocsError(f"Unknown site-absolute link: {source} -> {target}")
        path = ((self.repo if url.path.startswith("/") else source.parent) / unquote(url.path).lstrip("/")).resolve()
        # The docs are read in the repository (there is no deployed site), so a
        # book page may link to any repository file, such as source code.
        if not path.is_relative_to(self.repo):
            raise DocsError(f"Relative link escapes repository: {source} -> {target}")
        if in_book and (url.path.endswith("/") or path.is_dir()):
            path = path / "index.html"
        if in_book and path.suffix in {".md", ".mdx"} and path.is_relative_to(self.source):
            if path not in self.published:
                raise DocsError(f"Linked Markdown is not a SUMMARY.md chapter: {source} -> {target}")
        elif in_book and path.suffix == ".html" and path in self.outputs:
            path = self.outputs[path]
        if not path.is_file() and not (not in_book and path.is_dir()):
            raise DocsError(f"Missing local link target: {source} -> {target}")
        self.checked_links += 1

    def check_document(self, path: Path, *, in_book: bool) -> list[str]:
        text = read_text(path)
        if in_book and has_directive(text):
            raise DocsError(f"mdBook directive in plain Markdown (nothing renders it; inline the content): {path}")
        links = markdown_links(text)
        for target in links:
            self.check_target(path, target, in_book=in_book)
        return links

    def check_book(self) -> None:
        self.load_summary()
        for path in sorted(self.published):
            self.check_document(path, in_book=True)

    def report(self) -> None:
        print(f"Docs source gate: {len(self.published)} chapters; {self.checked_links} local targets checked.")
        print("Coverage: SUMMARY.md navigation and local target paths; no anchor or HTTP availability claim.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        book = BookSources(args.repo)
        book.check_book()
        book.report()
        return 0
    except (DocsError, OSError, ValueError) as exc:
        print(f"Docs source gate failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
