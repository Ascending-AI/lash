#!/usr/bin/env python3
"""Fixture tests for check-transcript-diff.py."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


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
    def test_enforcement_fails_an_unacknowledged_durable_change(self) -> None:
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=completed),
            mock.patch.dict(MODULE.os.environ, {"GITHUB_EVENT_PATH": ""}),
        ):
            exit_code = MODULE.main(["--enforce", "base...head"])

        self.assertEqual(exit_code, 1)

    def test_enforcement_accepts_a_named_pr_justification(self) -> None:
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(
                json.dumps(
                    {
                        "pull_request": {
                            "body": "## Validation\n\nTranscript: the revision changes because the fixture commits twice."
                        }
                    }
                ),
                encoding="utf-8",
            )
            with (
                mock.patch.object(MODULE.subprocess, "run", return_value=completed),
                mock.patch.dict(
                    MODULE.os.environ, {"GITHUB_EVENT_PATH": str(event_path)}
                ),
            ):
                exit_code = MODULE.main(["--enforce", "base...head"])

        self.assertEqual(exit_code, 0)

    def test_advisory_mode_reports_without_failing(self) -> None:
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with mock.patch.object(MODULE.subprocess, "run", return_value=completed):
            exit_code = MODULE.main(["--advisory", "base...head"])

        self.assertEqual(exit_code, 0)

    def test_an_unevaluable_range_fails_in_both_modes(self) -> None:
        error = MODULE.subprocess.CalledProcessError(128, ["git", "diff"])
        for mode in ("--enforce", "--advisory"):
            with (
                self.subTest(mode=mode),
                mock.patch.object(MODULE.subprocess, "run", side_effect=error),
            ):
                self.assertEqual(MODULE.main([mode, "missing...range"]), 2)

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

    def test_each_marker_fires_alone(self) -> None:
        # One fixture per marker, each carrying ONLY that marker: losing any
        # single marker from DURABLE_MARKERS / USAGE_COMPONENT /
        # REV_TRANSITION must fail here.
        # (The original fixture carried Checkpoint AND rev=, so the regex
        # covered for a deleted marker tuple — proven by mutation in review.)
        marker_lines = {
            "Checkpoint": "      0004  Checkpoint         label.only",
            "DurableEffect": "      0005  DurableEffect      effect.park",
            "usage": "session-001              usage                 entries=1 input=2 output=3 cache_read=0 cache_write=0 reasoning=0 total=5",
            "stored logical=": "                 tool_state        stored logical=27 B",
            "ref (unchanged)": "                 tool_state        ref (unchanged)",
            "rev=": "      0006  Commit             commit rev=1->2",
        }
        for marker, line in marker_lines.items():
            with self.subTest(marker=marker):
                diff = (
                    "diff --git a/crates/demo/src/lib.rs b/crates/demo/src/lib.rs\n"
                    "--- a/crates/demo/src/lib.rs\n"
                    "+++ b/crates/demo/src/lib.rs\n"
                    "@@ -1,5 +1,5 @@\n"
                    " fn transcript() {\n"
                    '     insta::assert_snapshot!(render(), @r"\n'
                    f"-{line}\n"
                    f"+{line} changed\n"
                    '     ");\n'
                    " }\n"
                )
                findings = MODULE.classify_patch(diff)
                self.assertEqual(
                    len(findings), 1, f"marker {marker!r} must fire on its own"
                )

    def test_snapshot_outside_dedicated_test_file_is_seen(self) -> None:
        # Inline #[cfg(test)] modules live throughout src/; a path allowlist
        # that misses them is a silent-decay surface. The marker fixture above
        # already uses src/lib.rs, but pin the property by name too.
        diff = DURABLE_DIFF.replace(
            "crates/demo/tests/transcript.rs", "crates/demo/src/renderer.rs"
        )
        findings = MODULE.classify_patch(diff)
        self.assertEqual(len(findings), 1)


if __name__ == "__main__":
    unittest.main()
