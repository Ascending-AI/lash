from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest
import unittest.mock


SCRIPT = Path(__file__).with_name("check_checkpoint_component_flatten.py")
SPEC = importlib.util.spec_from_file_location(
    "check_checkpoint_component_flatten", SCRIPT
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


FLATTENED_STRUCT = """\
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Flat {
    pub id: u32,
    #[serde(flatten)]
    pub inner: Inner,
}
"""

NESTED_STRUCT = """\
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Nested {
    pub id: u32,
    pub inner: Inner,
}
"""

MULTILINE_FLATTEN = """\
#[derive(serde::Serialize)]
pub struct Flat {
    #[serde(
        default,
        flatten
    )]
    pub inner: Inner,
}
"""

FLATTEN_OUTSIDE_SERDE = """\
/// Mentions serde(flatten) in prose; not an attribute.
pub struct DocOnly {
    /// The word flatten here is documentation, not a directive.
    pub inner: Inner,
}
"""


class SerdeAttributeSpans(unittest.TestCase):
    def test_flatten_flagged(self):
        self.assertEqual(1, len(MODULE.flatten_occurrences(FLATTENED_STRUCT)))

    def test_multiline_flatten_flagged(self):
        self.assertEqual(1, len(MODULE.flatten_occurrences(MULTILINE_FLATTEN)))

    def test_nested_struct_clean(self):
        self.assertEqual([], MODULE.flatten_occurrences(NESTED_STRUCT))

    def test_flatten_in_prose_ignored(self):
        self.assertEqual([], MODULE.flatten_occurrences(FLATTEN_OUTSIDE_SERDE))

    def test_string_literal_parens_do_not_truncate(self):
        text = (
            "#[serde(with = \"a)b\", flatten)]\n"
            "pub field: u32,\n"
        )
        self.assertEqual(1, len(MODULE.flatten_occurrences(text)))


class RealTree(unittest.TestCase):
    def test_current_tree_is_flatten_free(self):
        self.assertEqual(0, MODULE.main())


class InventoryFailures(unittest.TestCase):
    def test_missing_whole_file_fails(self):
        with unittest.mock.patch.object(
            MODULE, "WHOLE_FILES", ("nonexistent/file.rs",)
        ), unittest.mock.patch.object(MODULE, "SYMBOL_FILES", {}):
            self.assertEqual(1, MODULE.main())

    def test_missing_symbol_fails(self):
        with unittest.mock.patch.object(
            MODULE, "WHOLE_FILES", ()
        ), unittest.mock.patch.object(
            MODULE, "SYMBOL_FILES", {"scripts/check_version_bumps.py": ("Nope",)}
        ):
            self.assertEqual(1, MODULE.main())


if __name__ == "__main__":
    unittest.main()
