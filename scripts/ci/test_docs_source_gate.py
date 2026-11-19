#!/usr/bin/env python3
"""Publication/source regressions; fixtures are real books and temporary git repos."""

from contextlib import contextmanager
from contextlib import redirect_stdout
import io
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from collect_changed_links import select_docs_files
from docs_lint_filter import changed_lines, filter_output
from docs_source_gate import BookSources, DocsError, markdown_links


SCRIPTS = Path(__file__).resolve().parent


@contextmanager
def in_directory(path):
    previous = Path.cwd()
    os.chdir(path)
    try:
        yield
    finally:
        os.chdir(previous)


class BookFixture(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.write("docs/book/src/intro.md", "# Intro\n")
        self.summary("intro.md")

    def write(self, name, text):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
        return path

    def summary(self, *chapters):
        self.write("docs/book/src/SUMMARY.md", "# Summary\n\n" + "".join(f"- [{path}]({path})\n" for path in chapters))

    def validate(self):
        book = BookSources(self.repo)
        book.check_book()
        return book


class SourceGateTests(BookFixture):
    def test_renamed_page_breaks_an_unchanged_incoming_link(self):
        self.write("docs/book/src/target.md", "# Target\n")
        self.write("docs/book/src/intro.md", "# Intro\n\n[Target](target.md)\n")
        self.summary("intro.md", "target.md")
        self.validate()
        (self.repo / "docs/book/src/target.md").rename(self.repo / "docs/book/src/renamed.md")
        self.summary("intro.md", "renamed.md")
        with self.assertRaisesRegex(DocsError, "not a SUMMARY.md chapter"):
            self.validate()

    def test_deleted_chapter_is_rejected_even_without_new_links(self):
        (self.repo / "docs/book/src/intro.md").unlink()
        with self.assertRaisesRegex(DocsError, "SUMMARY.md chapter is missing"):
            self.validate()

    def test_source_file_not_in_summary_is_not_a_published_page(self):
        self.write("docs/book/src/unlisted.md", "# Unlisted\n")
        self.write("docs/book/src/intro.md", "[Page](unlisted.md)\n")
        with self.assertRaisesRegex(DocsError, "not a SUMMARY.md chapter"):
            self.validate()

    def test_book_may_link_repository_files_but_not_escape_the_repository(self):
        self.write("crates/edge/README.md", "# Edge\n")
        self.write("docs/book/src/intro.md", "[Edge](../../../crates/edge/README.md)\n")
        self.validate()
        self.write("docs/book/src/intro.md", "[Outside](../../../../outside.md)\n")
        with self.assertRaisesRegex(DocsError, "escapes repository"):
            self.validate()

    def test_mdbook_directives_are_rejected_outside_code(self):
        for directive in ("{{#include absent.md}}", "{{ #config-fields agents}}", "- item\n  {{#peer-group telegram}}"):
            with self.subTest(directive=directive):
                self.write("docs/book/src/intro.md", directive + "\n")
                with self.assertRaisesRegex(DocsError, "mdBook directive"):
                    self.validate()
        self.write("docs/book/src/intro.md", "`{{#if x}}`\n\n```hbs\n{{#each items}}\n```\n")
        self.validate()

    def test_commonmark_examples_are_not_links_but_reference_links_are(self):
        content = "```md\n[example](absent.md)\n```\n\n`[example](absent.md)`\n\n[real][target]\n\n[target]: intro.md\n<!-- [comment](absent.md) -->\n"
        self.assertEqual(markdown_links(content), ["intro.md"])
        self.write("docs/book/src/intro.md", content)
        self.validate()
        self.write("docs/book/src/intro.md", content.replace("[target]: intro.md", "[target]: absent.md"))
        with self.assertRaises(DocsError):
            self.validate()

    def test_readme_directory_and_html_links_resolve_to_real_chapters(self):
        self.write("docs/book/src/foundations/README.md", "# Foundations\n")
        self.summary("intro.md", "foundations/README.md")
        self.write("docs/book/src/intro.md", "[Directory](foundations/) [Page](foundations/index.html)\n")
        self.assertEqual(self.validate().checked_links, 2)

    def test_bad_utf8_and_unknown_site_absolute_links_fail(self):
        (self.repo / "docs/book/src/intro.md").write_bytes(b"\xff")
        with self.assertRaisesRegex(DocsError, "Cannot read"):
            self.validate()
        self.write("docs/book/src/intro.md", "[Typo](/api/not-a-known-crate/index.html)\n")
        with self.assertRaisesRegex(DocsError, "Unknown site-absolute"):
            self.validate()


class GitSelectionFixture(BookFixture):
    def setUp(self):
        super().setUp()
        self.git("init", "-q")
        self.write("README.md", "# Project\n")
        self.write("crates/runtime/prompt.md", "# Runtime prompt\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, check=True, text=True, capture_output=True).stdout.strip()

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Docs test", "-c", "user.email=docs@example.invalid", "commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def collector(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPTS / "collect_changed_links.py"), "--output", str(self.repo / "links.txt"), "--check-local-targets", *args],
            cwd=self.repo, text=True, capture_output=True,
        )

    def test_readme_style_scope_and_invalid_base_full_scan(self):
        self.write("README.md", "# Updated project\n")
        self.commit()
        with in_directory(self.repo):
            self.assertEqual(select_docs_files(self.base, quality_only=True), ["README.md"])
            for base in ("", "not-a-commit"):
                selected = select_docs_files(base, quality_only=True)
                self.assertIn("README.md", selected)
                self.assertIn("docs/book/src/intro.md", selected)
                self.assertNotIn("crates/runtime/prompt.md", selected)

    def test_no_doc_delta_still_checks_deleted_page_inbound_links(self):
        self.write("docs/book/src/target.md", "# Target\n")
        self.write("docs/book/src/intro.md", "[Target](target.md)\n")
        self.summary("intro.md", "target.md")
        base = self.commit()
        (self.repo / "docs/book/src/target.md").unlink()
        self.summary("intro.md")
        self.commit()
        result = self.collector("--base", base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a SUMMARY.md chapter", result.stderr)

    def test_rename_detects_old_inbound_link_only_in_unchanged_readme(self):
        self.write("README.md", "[Intro](docs/book/src/intro.md)\n")
        base = self.commit()
        (self.repo / "docs/book/src/intro.md").rename(self.repo / "docs/book/src/renamed.md")
        self.summary("renamed.md")
        self.commit()
        result = self.collector("--base", base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Missing local link target", result.stderr)

    def test_deleted_asset_checks_unchanged_readme_with_no_markdown_delta(self):
        self.write("logo.svg", "<svg/>\n")
        self.write("README.md", "![Logo](logo.svg)\n")
        base = self.commit()
        (self.repo / "logo.svg").unlink()
        self.commit()
        result = self.collector("--base", base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Missing local link target", result.stderr)

    def test_missing_base_checks_old_repository_readme_links(self):
        self.write("README.md", "[Missing](missing.md)\n")
        self.commit()
        for base in ("", "not-a-commit"):
            result = self.collector("--base", base)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("Missing local link target", result.stderr)

    def test_deleted_nonbook_markdown_is_allowed_but_unknown_file_is_not(self):
        (self.repo / "crates/runtime/prompt.md").unlink()
        self.commit()
        self.assertEqual(self.collector("--base", self.base).returncode, 0)
        result = self.collector("--base", self.base, "--docs-files", "unknown.md")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot read", result.stderr)

    def test_readme_unrelated_edit_does_not_force_old_punctuation_cleanup(self):
        self.write("README.md", "# Project\n\nExisting — punctuation.\n\nOld line.\n")
        base = self.commit()
        self.write("README.md", "# Project\n\nExisting — punctuation.\n\nUpdated line.\n")
        self.commit()
        self.write(".markdownlint-cli2.yaml", "config:\n  default: true\n")
        for name in ("docs_quality_gate.sh", "collect_changed_links.py", "docs_source_gate.py", "docs_lint_filter.py"):
            destination = self.repo / "scripts/ci" / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(SCRIPTS / name, destination)
        # This test exercises selection/punctuation; markdownlint itself is
        # external and has its own tests. No network installation in fixtures.
        npx = self.write("bin/npx", "#!/bin/sh\nexit 0\n")
        npx.chmod(0o755)
        env = {**os.environ, "BASE_SHA": base, "PATH": f"{self.repo / 'bin'}:{os.environ['PATH']}"}
        command = ["bash", str(self.repo / "scripts/ci/docs_quality_gate.sh")]
        result = subprocess.run(command, cwd=self.repo, env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.write("README.md", "# Project\n\nExisting — punctuation.\n\nNew — punctuation.\n")
        self.commit()
        result = subprocess.run(command, cwd=self.repo, env=env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Blocking prose em-dashes", result.stdout)

    def test_diff_failure_cannot_turn_lint_errors_into_baseline(self):
        with in_directory(self.repo):
            with self.assertRaises(subprocess.CalledProcessError):
                changed_lines("missing-base", ["README.md"])


class LintFilterTests(unittest.TestCase):
    def filtered(self, diagnostics, changed, exit_code=1, extra=""):
        output = f"markdownlint-cli2 v0.20.0 (markdownlint v0.40.0)\nFinding: README.md\nLinting: 1 file(s)\nSummary: {len(diagnostics)} error(s)\n" + "\n".join(diagnostics) + extra
        with redirect_stdout(io.StringIO()):
            return filter_output(output, exit_code, changed)

    def test_column_diagnostic_on_changed_line_blocks(self):
        # Actual pinned CLI output shape: MD009 and MD034 include a column.
        error = 'README.md:3:6 error MD034/no-bare-urls Bare URL used [Context: "https://example.com"]'
        self.assertEqual(self.filtered([error], {"README.md": {3}}), 1)
        self.assertEqual(self.filtered([error], {"README.md": {5}}), 0)

    def test_baseline_does_not_hide_unknown_output_or_execution_exit(self):
        error = "README.md:2 error MD025/single-title Multiple top-level headings"
        self.assertEqual(self.filtered([error], {"README.md": {5}}, extra="\nError: failed to load rule"), 2)
        self.assertEqual(self.filtered([error], {"README.md": {5}}, exit_code=2), 2)

    def test_unknown_file_or_missing_diagnostics_fails(self):
        error = "unknown.md:2 error MD025/single-title Multiple top-level headings"
        self.assertEqual(self.filtered([error], {"README.md": {5}}), 2)
        self.assertEqual(self.filtered([], {"README.md": {5}}), 2)


if __name__ == "__main__":
    unittest.main()
