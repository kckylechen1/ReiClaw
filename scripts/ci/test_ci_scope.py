import contextlib
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

from unittest import mock

from change_scope import classify, event_base, parse_changes, prose_path


class ScopeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.write("README.md", "initial\n")
        self.write("docs/book/src/page.md", "page\n")
        self.write("src/lib.rs", "fn initial() {}\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.DEVNULL).decode().strip()

    def write(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def scope(self, event="pull_request", base=None):
        base = self.base if base is None else base
        payload = {"pull_request": {"base": {"sha": base}}, "before": base, "merge_group": {"base_sha": base}}
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return classify(event, payload, self.root)

    def test_readme_and_adr_only_skip_code_on_all_supported_events(self):
        self.write("README.md", "updated\n")
        self.write("docs/book/src/architecture/decisions/ADR-fixture.md", "proposal\n")
        self.commit()
        expected = {
            "pull_request": {"full": "false", "docs_build": "false", "base_sha": self.base, "installer": "false", "web": "false"},
            "push": {"full": "false", "docs_build": "false", "base_sha": self.base, "installer": "true", "web": "true"},
            "merge_group": {"full": "false", "docs_build": "false", "base_sha": self.base, "installer": "true", "web": "true"},
        }
        for event, want in expected.items():
            with self.subTest(event=event):
                self.assertEqual(self.scope(event), want)

    def test_root_prose_files_skip_code_across_events_while_symlinks_keep_full(self):
        for path in ("AGENTS.md", "CLAUDE.md", ".github/pull_request_template.md"):
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, "prose update\n")
                self.commit()
                for event in ("pull_request", "push", "merge_group"):
                    scope = self.scope(event, base=base)
                    self.assertEqual(scope["full"], "false")
                    self.assertEqual(scope["docs_build"], "false")
                    if event == "pull_request":
                        self.assertEqual(scope["installer"], "false")
                        self.assertEqual(scope["web"], "false")
                sym_base = self.git("rev-parse", "HEAD")
                target = self.root / path
                target.unlink()
                target.symlink_to("src/lib.rs")
                self.commit()
                for event in ("pull_request", "push", "merge_group"):
                    self.assertEqual(
                        self.scope(event, base=sym_base),
                        {"full": "true", "docs_build": "true", "base_sha": "", "installer": "true", "web": "true"},
                    )

    def test_summary_requires_render_without_unrelated_runtime_matrix(self):
        self.write("docs/book/src/SUMMARY.md", "# Summary\n")
        self.commit()
        self.assertEqual(self.scope()["full"], "false")
        self.assertEqual(self.scope()["docs_build"], "true")

    def test_mixed_code_and_docs_keep_full_lane(self):
        self.write("README.md", "updated\n")
        self.write("src/lib.rs", "fn changed() {}\n")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_book_generator_inputs_require_real_render(self):
        for path in ("docs/book/peer-groups.toml", "docs/book/feature-matrix-parity.toml", "docs/book/env-vars.toml", "src/main.rs", "web/src/contexts/themes.json"):
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, "changed\n")
                self.commit()
                self.assertEqual(self.scope(base=base)["docs_build"], "true")

    def test_added_and_removed_custom_directives_reach_real_renderer(self):
        for directive in ("{{#peer-group definitely_missing}}", "{{ #include snippet.md}}"):
            with self.subTest(directive=directive):
                base = self.git("rev-parse", "HEAD")
                self.write("docs/book/src/page.md", directive + "\n")
                directive_commit = self.commit()
                self.assertEqual(self.scope(base=base)["full"], "false")
                self.assertEqual(self.scope(base=base)["docs_build"], "true")
                self.write("docs/book/src/page.md", "plain prose again\n")
                self.commit()
                self.assertEqual(self.scope(base=directive_commit)["docs_build"], "true")

    def test_runtime_markdown_and_unknown_paths_are_not_prose(self):
        for path in (
            "crates/reiclaw-runtime/src/skills/review_prompt.md",
            "crates/reiclaw-runtime/src/agent/personality_templates/SOUL.md",
            "crates/reiclaw-runtime/AGENTS.md",
            ".github/CODEOWNERS",
            ".github/workflows/ci.yml",
            "unknown.md",
            "../README.md",
        ):
            with self.subTest(path=path):
                self.assertFalse(prose_path(path))

    def test_rename_from_code_to_docs_does_not_hide_deleted_code(self):
        self.git("mv", "src/lib.rs", "docs/book/src/moved.md")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_deleted_doc_remains_in_documentation_lane(self):
        (self.root / "docs/book/src/page.md").unlink()
        self.commit()
        self.assertEqual(self.scope()["full"], "false")

    def test_stack_uses_actual_parent_instead_of_master(self):
        self.write("src/lib.rs", "fn parent_change() {}\n")
        parent = self.commit()
        self.write("README.md", "child prose\n")
        self.commit()
        self.assertEqual(self.scope(base=parent)["full"], "false")
        self.assertEqual(self.scope(base=self.base)["full"], "true")

    def test_unavailable_zero_invalid_or_empty_diff_keeps_every_check(self):
        for base in ("0" * 40, "1" * 40, "--help", "", self.base):
            for event in ("pull_request", "merge_group", "push"):
                with self.subTest(base=base, event=event):
                    self.assertEqual(
                        self.scope(event, base=base),
                        {"full": "true", "docs_build": "true", "base_sha": "", "installer": "true", "web": "true"},
                    )

    def test_targeted_selection_on_pull_request_push_and_merge_group(self):
        cases = [
            ("crates/reiclaw-runtime/src/lib.rs", "false", "false"),
            ("crates/reiclaw-runtime/Cargo.toml", "true", "false"),
            ("Cargo.lock", "true", "false"),
            ("xtask/src/main.rs", "true", "false"),
            ("dev/ci/test-partition.json", "true", "false"),
            ("scripts/ci/install_source_smoke.sh", "true", "false"),
            ("install.sh", "true", "false"),
            (".cargo/config.toml", "true", "false"),
            ("web/src/App.tsx", "false", "true"),
            (".nvmrc", "false", "true"),
            ("package.json", "false", "true"),
            ("package-lock.json", "false", "true"),
            ("npm-shrinkwrap.json", "false", "true"),
            ("pnpm-lock.yaml", "false", "true"),
            ("yarn.lock", "false", "true"),
            (".npmrc", "false", "true"),
            ("crates/reiclaw-gateway/src/lib.rs", "false", "true"),
            (".github/workflows/ci.yml", "true", "true"),
        ]
        for path, installer, web in cases:
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, f"change {path}\n")
                self.commit()
                pr = self.scope("pull_request", base=base)
                self.assertEqual((pr["installer"], pr["web"]), (installer, web))
                push = self.scope("push", base=base)
                self.assertEqual((push["installer"], push["web"]), ("true", "true"))
                mg = self.scope("merge_group", base=base)
                self.assertEqual((mg["installer"], mg["web"]), ("true", "true"))

    def test_rename_inside_crate_and_delete_plus_add_fail_closed(self):
        self.write("crates/reiclaw-eval/tests/cli.rs", "fn cli_test() {}\n")
        base_mv = self.commit()
        self.git("mv", "crates/reiclaw-eval/tests/cli.rs", "crates/reiclaw-eval/tests/replay_cli.rs")
        self.commit()
        self.assertEqual(
            self.scope("pull_request", base=base_mv),
            {"full": "true", "docs_build": "true", "base_sha": "", "installer": "true", "web": "true"},
        )

        # Dissimilar deletion + addition (even across unrelated paths) also fails closed.
        base_da = self.git("rev-parse", "HEAD")
        (self.root / "crates/reiclaw-eval/tests/replay_cli.rs").unlink()
        self.write("docs/book/src/brand_new.md", "# Brand new unrelated prose\n")
        self.commit()
        self.assertEqual(
            self.scope("pull_request", base=base_da),
            {"full": "true", "docs_build": "true", "base_sha": "", "installer": "true", "web": "true"},
        )

    def test_unsafe_path_in_diff_fails_closed(self):
        self.write("crates/reiclaw-runtime/src/lib.rs", "fn safe() {}\n")
        self.commit()
        with mock.patch("change_scope.parse_change_records", return_value=[("../outside.rs", "100644", "100644", "M")]):
            for event in ("pull_request", "push", "merge_group"):
                self.assertEqual(
                    self.scope(event),
                    {"full": "true", "docs_build": "true", "base_sha": "", "installer": "true", "web": "true"},
                )

    def test_symlink_or_executable_markdown_is_not_exempt(self):
        target = self.root / "README.md"
        target.unlink()
        target.symlink_to("src/lib.rs")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_nonancestor_push_falls_back_to_full(self):
        self.git("checkout", "-qb", "other")
        self.write("README.md", "other\n")
        other = self.commit()
        self.git("checkout", "--detach", self.base)
        self.write("README.md", "different\n")
        self.commit()
        self.assertEqual(self.scope("push", other)["base_sha"], "")

    def test_nul_parser_rejects_partial_or_rename_records(self):
        for raw in (b"broken", b":100644 100644 a b R100\0old\0new\0", b":100644 100644 a b M\0"):
            with self.subTest(raw=raw):
                with self.assertRaises(ValueError):
                    parse_changes(raw)

    def test_event_selection_never_uses_default_branch(self):
        payload = {"pull_request": {"base": {"sha": "parent"}}, "before": "before", "merge_group": {"base_sha": "queue-base"}}
        self.assertEqual(event_base("pull_request", payload), "parent")
        self.assertEqual(event_base("push", payload), "before")
        self.assertEqual(event_base("merge_group", payload), "queue-base")


if __name__ == "__main__":
    unittest.main()
