#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


SCRIPT = Path(__file__).with_name("check_durable_entropy.py")
SPEC = importlib.util.spec_from_file_location("check_durable_entropy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

SCAN = MODULE.scan_source
PATH = "crates/lash-core/src/runtime/example.rs"
TEST_PATH = "crates/lash-core/src/runtime/example/tests.rs"


class EntropyFlagging(unittest.TestCase):
    def test_unannotated_uuid_is_flagged(self):
        source = "fn mint() -> String {\n    uuid::Uuid::new_v4().to_string()\n}\n"
        violations = SCAN(PATH, source)
        self.assertEqual(len(violations), 1)
        self.assertIn(f"{PATH}:2", violations[0])

    def test_same_line_annotation_allows(self):
        source = (
            "fn mint() -> String {\n"
            "    uuid::Uuid::new_v4().to_string() // durable-entropy: fencing nonce\n"
            "}\n"
        )
        self.assertEqual(SCAN(PATH, source), [])

    def test_annotation_above_allows(self):
        source = (
            "let reservation_id =\n"
            "    // durable-entropy: in-process bookkeeping\n"
            "    uuid::Uuid::new_v4().to_string();\n"
        )
        self.assertEqual(SCAN(PATH, source), [])

    def test_distant_annotation_does_not_cover(self):
        source = (
            "// durable-entropy: fencing nonce\n"
            "let a = 1;\n"
            "let b = 2;\n"
            "let c = 3;\n"
            "let d = 4;\n"
            "let leaked = uuid::Uuid::new_v4().to_string();\n"
        )
        self.assertEqual(len(SCAN(PATH, source)), 1)

    def test_system_time_and_process_id_are_flagged(self):
        source = (
            "fn stamp() -> u64 {\n"
            "    let a = std::time::SystemTime::now();\n"
            "    let b = std::process::id();\n"
            "    0\n"
            "}\n"
        )
        violations = SCAN(PATH, source)
        self.assertEqual(len(violations), 2, violations)

    def test_rand_family_is_flagged(self):
        source = "let n = rand::random::<u64>();\nlet m = fastrand::u64(..);\n"
        self.assertEqual(len(SCAN(PATH, source)), 2)

    def test_cfg_test_module_is_exempt(self):
        source = (
            "fn production() -> String {\n"
            "    \"ok\".to_string()\n"
            "}\n"
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    fn case() {\n"
            "        let _ = uuid::Uuid::new_v4();\n"
            "    }\n"
            "}\n"
        )
        self.assertEqual(SCAN(PATH, source), [])

    def test_tokio_test_fn_is_exempt_but_code_after_is_not(self):
        source = (
            "#[tokio::test]\n"
            "async fn case() {\n"
            "    let _ = uuid::Uuid::new_v4();\n"
            "}\n"
            "fn production() {\n"
            "    let _ = uuid::Uuid::new_v4();\n"
            "}\n"
        )
        violations = SCAN(PATH, source)
        self.assertEqual(len(violations), 1)
        self.assertIn(":6", violations[0])

    def test_test_path_is_exempt(self):
        source = "let _ = uuid::Uuid::new_v4();\n"
        self.assertEqual(SCAN(TEST_PATH, source), [])

    def test_doc_comment_mention_is_not_a_hit(self):
        source = (
            "/// Unlike `uuid::Uuid::new_v4()` this derives from the journal.\n"
            "fn derived() {}\n"
        )
        self.assertEqual(SCAN(PATH, source), [])

    def test_string_literal_is_not_a_hit(self):
        source = 'let s = "Uuid::new_v4";\n'
        self.assertEqual(SCAN(PATH, source), [])


if __name__ == "__main__":
    unittest.main()
