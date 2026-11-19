import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from select_test_matrix import main, select_matrix


ALL_MATRIX = [
    {"name": "runtime", "leg": "runtime", "phase": "main"},
    {"name": "runtime / landlock", "leg": "runtime", "phase": "landlock"},
    {"name": "channels", "leg": "channels", "phase": "main"},
    {"name": "channels / voice-surface", "leg": "channels", "phase": "voice-surface"},
    {"name": "app", "leg": "app", "phase": "main"},
    {"name": "tail", "leg": "tail", "phase": "main"},
]

PARTITION = {
    "legs": {
        "runtime": ["reiclaw-runtime", "reiclaw-eval"],
        "channels": ["reiclaw-gateway"],
        "app": ["reiclaw", "reiclaw-config"],
        "tail": [
            "reiclaw-leaf",
            "reiclaw-dev-leaf",
            "reiclaw-build-leaf",
            "reiclaw-bridge-acp",
            "xtask",
        ],
    },
    "excluded": [],
}


def build_metadata(workspace_root: str) -> dict:
    return {
        "workspace_root": workspace_root,
        "workspace_members": [
            "pkg-root",
            "pkg-config",
            "pkg-runtime",
            "pkg-eval",
            "pkg-gateway",
            "pkg-leaf",
            "pkg-dev-leaf",
            "pkg-build-leaf",
            "pkg-bridge-acp",
            "pkg-xtask",
        ],
        "packages": [
            {
                "id": "pkg-root",
                "name": "reiclaw",
                "manifest_path": f"{workspace_root}/Cargo.toml",
                "dependencies": [
                    {"name": "reiclaw-config", "kind": None},
                    {"name": "reiclaw-runtime", "kind": None},
                    {"name": "reiclaw-gateway", "kind": None},
                    {"name": "reiclaw-leaf", "kind": None},
                ],
            },
            {
                "id": "pkg-config",
                "name": "reiclaw-config",
                "manifest_path": f"{workspace_root}/crates/reiclaw-config/Cargo.toml",
                "dependencies": [],
            },
            {
                "id": "pkg-runtime",
                "name": "reiclaw-runtime",
                "manifest_path": f"{workspace_root}/crates/reiclaw-runtime/Cargo.toml",
                "dependencies": [
                    {"name": "reiclaw-config", "kind": None},
                    {"name": "reiclaw-dev-leaf", "kind": "dev"},
                ],
            },
            {
                "id": "pkg-eval",
                "name": "reiclaw-eval",
                "manifest_path": f"{workspace_root}/crates/reiclaw-eval/Cargo.toml",
                "dependencies": [
                    {"name": "reiclaw-runtime", "kind": None},
                ],
            },
            {
                "id": "pkg-gateway",
                "name": "reiclaw-gateway",
                "manifest_path": f"{workspace_root}/crates/reiclaw-gateway/Cargo.toml",
                "dependencies": [
                    {"name": "reiclaw-config", "kind": None},
                    {"name": "reiclaw-build-leaf", "kind": "build"},
                ],
            },
            {
                "id": "pkg-leaf",
                "name": "reiclaw-leaf",
                "manifest_path": f"{workspace_root}/crates/reiclaw-leaf/Cargo.toml",
                "dependencies": [],
            },
            {
                "id": "pkg-dev-leaf",
                "name": "reiclaw-dev-leaf",
                "manifest_path": f"{workspace_root}/crates/reiclaw-dev-leaf/Cargo.toml",
                "dependencies": [],
            },
            {
                "id": "pkg-build-leaf",
                "name": "reiclaw-build-leaf",
                "manifest_path": f"{workspace_root}/crates/reiclaw-build-leaf/Cargo.toml",
                "dependencies": [],
            },
            {
                "id": "pkg-bridge-acp",
                "name": "reiclaw-bridge-acp",
                "manifest_path": f"{workspace_root}/bridges/acp/Cargo.toml",
                "dependencies": [],
            },
            {
                "id": "pkg-xtask",
                "name": "xtask",
                "manifest_path": f"{workspace_root}/xtask/Cargo.toml",
                "dependencies": [],
            },
        ],
    }


class TestMatrixSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")

        self.write("Cargo.toml", "[workspace]\nmembers = []\n")
        self.write("Cargo.lock", "# lock\n")
        self.write("README.md", "readme\n")
        self.write("AGENTS.md", "agents\n")
        self.write("src/lib.rs", "pub fn root() {}\n")
        self.write("docs/book/src/guide.md", "guide\n")
        for member in (
            "crates/reiclaw-config",
            "crates/reiclaw-runtime",
            "crates/reiclaw-eval",
            "crates/reiclaw-gateway",
            "crates/reiclaw-leaf",
            "crates/reiclaw-dev-leaf",
            "crates/reiclaw-build-leaf",
            "bridges/acp",
            "xtask",
        ):
            self.write(f"{member}/Cargo.toml", "[package]\n")
            self.write(f"{member}/src/lib.rs", "pub fn ok() {}\n")
        self.write("bridges/acp/src/main.rs", "fn main() {}\n")
        self.write("bridges/acp/README.md", "bridge\n")
        self.write("crates/reiclaw-eval/tests/cli.rs", "fn cli() {}\n")
        self.write("crates/reiclaw-eval/fixtures/trace.json", "{}\n")
        self.write("crates/reiclaw-gateway/build.rs", "fn main() {}\n")
        self.write("crates/reiclaw-runtime/locales/en/cli.ftl", "hello = Hello\n")

        self.partition_file = Path(self.temp.name) / "test-partition.json"
        self.partition_file.write_text(json.dumps(PARTITION), encoding="utf-8")
        self.metadata_file = Path(self.temp.name) / "metadata.json"
        self.metadata_file.write_text(json.dumps(build_metadata("/fixture/repo")), encoding="utf-8")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(
            ["git", *args], cwd=self.root, stderr=subprocess.DEVNULL
        ).decode().strip()

    def write(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def select(self, event="pull_request", base=None, metadata_file=None):
        base = self.base if base is None else base
        mfile = self.metadata_file if metadata_file is None else metadata_file
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return select_matrix(
                event,
                base,
                self.partition_file,
                ALL_MATRIX,
                self.root,
                metadata_file=mfile,
            )

    def leg_names(self, selected):
        return [entry["name"] for entry in selected]

    def test_true_leaf_selects_only_its_owning_leg_for_non_rs_and_adds_app_for_rs(self):
        base = self.git("rev-parse", "HEAD")
        self.write("crates/reiclaw-eval/fixtures/trace.json", "{\"ok\":true}\n")
        self.commit()
        self.assertEqual(self.leg_names(self.select(base=base)), ["runtime", "runtime / landlock"])

        base2 = self.git("rev-parse", "HEAD")
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.commit()
        self.assertEqual(
            self.leg_names(self.select(base=base2)),
            ["runtime", "runtime / landlock", "app"],
        )

    def test_bridges_change_selects_app_and_tail_legs(self):
        base = self.git("rev-parse", "HEAD")
        self.write("bridges/acp/src/main.rs", "fn main() { println!(\"acp\"); }\n")
        self.commit()
        self.assertEqual(self.leg_names(self.select(base=base)), ["app", "tail"])

        base2 = self.git("rev-parse", "HEAD")
        self.write("bridges/acp/README.md", "updated bridge docs\n")
        self.commit()
        self.assertEqual(self.leg_names(self.select(base=base2)), ["app", "tail"])

    def test_leaf_depended_on_by_root_also_selects_app_leg(self):
        self.write("crates/reiclaw-leaf/src/lib.rs", "pub fn leaf() {}\n")
        self.commit()
        self.assertEqual(self.leg_names(self.select()), ["app", "tail"])

    def test_dev_and_build_dependencies_propagate_in_reverse_closure(self):
        base = self.git("rev-parse", "HEAD")
        self.write("crates/reiclaw-dev-leaf/src/lib.rs", "pub fn dev() {}\n")
        self.commit()
        self.assertEqual(
            self.leg_names(self.select(base=base)),
            ["runtime", "runtime / landlock", "app", "tail"],
        )

        base2 = self.git("rev-parse", "HEAD")
        self.write("crates/reiclaw-build-leaf/src/lib.rs", "pub fn bld() {}\n")
        self.commit()
        self.assertEqual(
            self.leg_names(self.select(base=base2)),
            ["channels", "channels / voice-surface", "app", "tail"],
        )

    def test_locales_change_adds_xtask_tail_leg(self):
        self.write("crates/reiclaw-runtime/locales/en/cli.ftl", "hello = Updated\n")
        self.commit()
        # runtime -> app (via root), plus xtask -> tail (via locales rule)
        self.assertEqual(
            self.leg_names(self.select()),
            ["runtime", "runtime / landlock", "app", "tail"],
        )

    def test_mixed_book_and_code_selects_xtask_tail_leg(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.write("docs/book/src/reference/index.md", "# Reference\n")
        self.commit()
        self.assertEqual(
            self.leg_names(self.select()),
            ["runtime", "runtime / landlock", "app", "tail"],
        )

    def test_prose_alongside_member_change_does_not_force_extra_legs(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.write("README.md", "updated\n")
        self.write("AGENTS.md", "updated\n")
        self.commit()
        self.assertEqual(
            self.leg_names(self.select()),
            ["runtime", "runtime / landlock", "app"],
        )

    def test_executable_prose_file_falls_back_to_full_matrix(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        (self.root / "README.md").chmod(0o755)
        self.commit()
        self.assertEqual(self.select(), ALL_MATRIX)

    def test_root_and_non_member_paths_fall_back_to_full_matrix(self):
        for path in (
            "src/lib.rs",
            "tests/integration.rs",
            "Cargo.toml",
            "Cargo.lock",
            "scripts/ci/change_scope.py",
            "dev/ci/test-partition.json",
            "locales.toml",
            ".github/workflows/ci.yml",
            "unknown_root_file.txt",
        ):
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, "changed\n")
                self.commit()
                self.assertEqual(self.select(base=base), ALL_MATRIX)

    def test_member_manifest_or_build_rs_falls_back_to_full_matrix(self):
        for path in ("crates/reiclaw-eval/Cargo.toml", "crates/reiclaw-gateway/build.rs"):
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, "# changed\n")
                self.commit()
                self.assertEqual(self.select(base=base), ALL_MATRIX)

    def test_non_pr_events_and_invalid_base_fall_back_to_full_matrix(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.commit()
        for event in ("push", "merge_group", "schedule", ""):
            with self.subTest(event=event):
                self.assertEqual(self.select(event=event), ALL_MATRIX)
        for bad_base in ("", "0" * 40, "1" * 40, "--help", self.git("rev-parse", "HEAD")):
            with self.subTest(bad_base=bad_base):
                self.assertEqual(self.select(base=bad_base), ALL_MATRIX)

    def test_non_ancestor_base_falls_back_to_full_matrix(self):
        self.git("checkout", "-qb", "other")
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn other() {}\n")
        other = self.commit()
        self.git("checkout", "--detach", self.base)
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn mine() {}\n")
        self.commit()
        self.assertEqual(self.select(base=other), ALL_MATRIX)

    def test_git_mv_and_delete_plus_add_fall_back_to_full_matrix(self):
        base_mv = self.git("rev-parse", "HEAD")
        self.git("mv", "crates/reiclaw-eval/tests/cli.rs", "crates/reiclaw-eval/tests/replay_cli.rs")
        self.commit()
        self.assertEqual(self.select(base=base_mv), ALL_MATRIX)

        base_da = self.git("rev-parse", "HEAD")
        (self.root / "crates/reiclaw-eval/tests/replay_cli.rs").unlink()
        self.write(
            "crates/reiclaw-eval/tests/completely_different.rs",
            "#[test]\nfn brand_new_unrelated_suite() { assert_eq!(1 + 1, 2); }\n",
        )
        self.commit()
        self.assertEqual(self.select(base=base_da), ALL_MATRIX)

    def test_symlink_rename_and_malformed_diff_fall_back_to_full_matrix(self):
        base = self.git("rev-parse", "HEAD")
        link = self.root / "crates/reiclaw-eval/src/link.rs"
        link.symlink_to("../../../src/lib.rs")
        self.commit()
        self.assertEqual(self.select(base=base), ALL_MATRIX)

        base2 = self.git("rev-parse", "HEAD")
        self.write("crates/reiclaw-eval/src/new.rs", "pub fn ok() {}\n")
        self.commit()
        with mock.patch(
            "change_scope.parse_change_records",
            side_effect=ValueError("unexpected raw diff record"),
        ):
            self.assertEqual(self.select(base=base2), ALL_MATRIX)
        with mock.patch(
            "change_scope.parse_change_records",
            return_value=[("../outside.rs", "100644", "100644", "M")],
        ):
            self.assertEqual(self.select(base=base2), ALL_MATRIX)

    def test_metadata_error_falls_back_to_full_matrix_without_cargo(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.commit()
        missing = Path(self.temp.name) / "missing.json"
        self.assertEqual(self.select(metadata_file=missing), ALL_MATRIX)
        bad = Path(self.temp.name) / "bad.json"
        bad.write_text("{not-json", encoding="utf-8")
        self.assertEqual(self.select(metadata_file=bad), ALL_MATRIX)

    def test_cli_reads_at_matrix_and_writes_github_output(self):
        self.write("crates/reiclaw-eval/src/lib.rs", "pub fn eval_only() {}\n")
        self.commit()
        all_matrix_file = Path(self.temp.name) / "all-matrix.json"
        all_matrix_file.write_text(json.dumps(ALL_MATRIX), encoding="utf-8")
        gh_out = Path(self.temp.name) / "gh-output"

        old_cwd = Path.cwd()
        stdout = io.StringIO()
        try:
            os.chdir(self.root)
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(gh_out)}):
                with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(io.StringIO()):
                    rc = main([
                        "--event", "pull_request",
                        "--base-sha", self.base,
                        "--partition", str(self.partition_file),
                        "--all-matrix", f"@{all_matrix_file}",
                        "--metadata-file", str(self.metadata_file),
                        "--github-output",
                    ])
        finally:
            os.chdir(old_cwd)

        self.assertEqual(rc, 0)
        expected = [entry for entry in ALL_MATRIX if entry["leg"] in {"runtime", "app"}]
        self.assertEqual(json.loads(stdout.getvalue()), expected)
        output_line = gh_out.read_text(encoding="utf-8").strip()
        self.assertTrue(output_line.startswith("matrix="))
        self.assertEqual(json.loads(output_line[len("matrix="):]), expected)


if __name__ == "__main__":
    unittest.main()
