#!/usr/bin/env python3
"""Fixture tests for check-transcript-diff.py."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("check-transcript-diff.py")
SPEC = importlib.util.spec_from_file_location("check_transcript_diff", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


DURABLE_DIFF = """\
diff --git a/crates/demo/tests/transcript.rs b/crates/demo/tests/transcript.rs
--- a/crates/demo/tests/transcript.rs
+++ b/crates/demo/tests/transcript.rs
@@ -1,5 +1,5 @@
 fn transcript() {
     insta::assert_snapshot!(render(), @r#"
-      0003  Checkpoint         checkpoint.commit  rev=0->1
+      0003  Checkpoint         checkpoint.commit  rev=1->2
     "#);
 }
"""

LABEL_ONLY_DIFF = """\
diff --git a/crates/demo/tests/transcript.rs b/crates/demo/tests/transcript.rs
--- a/crates/demo/tests/transcript.rs
+++ b/crates/demo/tests/transcript.rs
@@ -1,5 +1,5 @@
 fn transcript() {
     insta::assert_snapshot!(render(), @r"
-      0002  Tool               tool.old
+      0002  Tool               tool.renamed
     ");
 }
"""


class TranscriptDiffTests(unittest.TestCase):
    def test_durable_line_is_reported_with_pair_and_location(self) -> None:
        findings = MODULE.classify_patch(DURABLE_DIFF)

        self.assertEqual(len(findings), 1)
        output = MODULE.render_findings(findings)
        self.assertIn("crates/demo/tests/transcript.rs:3->3", output)
        self.assertIn("-       0003  Checkpoint", output)
        self.assertIn("+       0003  Checkpoint", output)

    def test_label_only_change_is_quiet(self) -> None:
        findings = MODULE.classify_patch(LABEL_ONLY_DIFF)

        self.assertEqual(findings, [])
        self.assertEqual(
            MODULE.render_findings(findings),
            "No durable transcript snapshot lines changed.",
        )


if __name__ == "__main__":
    unittest.main()
