import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

import source_tree_gate as gate

PACKAGE = '[package]\nname = "{name}"\n\n[lints]\nworkspace = true\n'


def workspace(files):
    root = Path(tempfile.mkdtemp())
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text))
    return root


def paths(violations, check):
    return sorted(v.path for v in violations if v.check == check)


class OrphanTests(unittest.TestCase):
    def test_follows_rustc_module_resolution(self):
        root = workspace({
            "Cargo.toml": '[workspace]\nmembers = ["crates/a"]\n\n' + PACKAGE.format(name="root"),
            # A crate root resolves children beside itself whatever its name.
            "src/main.rs": """
                mod cli;
                #[cfg(feature = "extra")]
                mod gated;
                mod outer {
                    const OPEN: char = '{';
                    const TEXT: &str = "}; mod ghost;";
                    mod inner;
                }
                // mod commented;
            """,
            # A non-mod-rs file's children live in a directory named after it;
            # a top-level #[path] is relative to the file's own directory.
            "src/cli.rs": '#[path = "shared_impl.rs"]\nmod shared;\nmod nested;\n',
            "src/cli/nested.rs": "",
            # rustc treats a #[path] file as mod-rs for its own children.
            "src/shared_impl.rs": "mod sibling;\n",
            "src/sibling.rs": "",
            "src/gated/mod.rs": "",
            "src/outer/inner.rs": "",
            "src/ghost.rs": "",
            "src/commented.rs": "",
            "tests/suite.rs": "mod helpers;\n",
            "tests/helpers/mod.rs": 'include!("cases.rs");\n',
            "tests/helpers/cases.rs": "",
            "crates/a/Cargo.toml": PACKAGE.format(name="a"),
            # An inline module shadows the file of the same name.
            "crates/a/src/lib.rs": "mod b;\nmod shadowed {}\n",
            # Inside an inline module of a non-mod-rs file, #[path] starts in
            # the file's module directory.
            "crates/a/src/b.rs": 'mod inl {\n    #[path = "x.rs"]\n    mod y;\n}\n',
            "crates/a/src/b/inl/x.rs": "",
            "crates/a/src/shadowed.rs": "",
        })
        self.assertEqual(
            paths(gate.Workspace(root).orphans(), "orphans"),
            ["crates/a/src/shadowed.rs", "src/commented.rs", "src/ghost.rs"],
        )


class SilencingTests(unittest.TestCase):
    def test_rejects_module_wide_silencing_and_stale_exceptions(self):
        root = workspace({
            "Cargo.toml": "[workspace]\nmembers = []\n\n" + PACKAGE.format(name="root"),
            "src/lib.rs": """
                #![allow(clippy::too_many_lines, reason = "dead_code is only text here")]
                mod inner;
                #[allow(unused_imports)]
                mod legacy;
                mod item;
                mod clean;
            """,
            "src/inner.rs": "#![cfg_attr(test, allow(unused))]\n",
            "src/legacy.rs": "",
            "src/item.rs": "#[allow(dead_code)]\nstruct Scoped;\n",
            "src/clean.rs": "",
        })
        exceptions = {"src/clean.rs": "fixture"}
        with mock.patch.object(gate, "SILENCING_EXCEPTIONS", exceptions):
            found = paths(gate.Workspace(root).silencing(), "silencing")
        self.assertEqual(found, ["src/clean.rs", "src/inner.rs", "src/lib.rs"])


class CrateTests(unittest.TestCase):
    def test_members_match_the_approved_list_and_inherit_lints(self):
        root = workspace({
            "Cargo.toml": '[workspace]\nmembers = ["crates/a", "crates/new"]\n\n'
            + PACKAGE.format(name="root"),
            "crates/a/Cargo.toml": PACKAGE.format(name="a"),
            "crates/new/Cargo.toml": '[package]\nname = "new"\n',
        })
        approved = frozenset({"crates/a", "crates/gone"})
        with mock.patch.object(gate, "APPROVED_CRATES", approved):
            found = paths(gate.Workspace(root).crates(), "crates")
        self.assertEqual(found, ["crates/gone", "crates/new", "crates/new/Cargo.toml"])


if __name__ == "__main__":
    unittest.main()
