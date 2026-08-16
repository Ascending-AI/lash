#!/usr/bin/env python3
"""Fixture tests for check-transcript-diff.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("check-transcript-diff.py")
SPEC = importlib.util.spec_from_file_location("check_transcript_diff", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


# Every environment variable the script reads, read out of the script itself
# rather than listed here: a hand-maintained list decays the moment the script
# consults one more variable, and the decay is invisible — the tests keep
# passing locally and start answering to the CI job's environment instead of
# their own fixtures.
ENV_READ = re.compile(
    r"""os\.(?:environ\.get|getenv)\(\s*["'](?P<name>[A-Z0-9_]+)["']"""
    r"""|os\.environ\[\s*["'](?P<subscript>[A-Z0-9_]+)["']"""
)
CONSULTED_ENV = frozenset(
    match.group("name") or match.group("subscript")
    for match in ENV_READ.finditer(SCRIPT.read_text(encoding="utf-8"))
)
# The extraction is only as good as its regex, so pin what it must have found.
# If the script stops reading one of these, drop it here deliberately; if the
# regex stops matching, this fails loudly instead of silently isolating nothing.
KNOWN_CONSULTED_ENV = frozenset(
    {
        "GITHUB_EVENT_PATH",
        "GITHUB_HEAD_REF",
        "GITHUB_REF_NAME",
        "GITHUB_TOKEN",
        "GITHUB_REPOSITORY",
        "GITHUB_STEP_SUMMARY",
    }
)


class IsolatedEnvironmentTestCase(unittest.TestCase):
    """A test case whose every test runs with no ambient GitHub context.

    The gate is a program that reads its answer out of the environment, so a
    test of it inherits the environment of whatever ran it. In a CI
    `pull_request` job that environment is a live one: `GITHUB_EVENT_PATH`
    points at the real payload of the very PR under test, `GITHUB_TOKEN` and
    `GITHUB_REPOSITORY` are set, and `GITHUB_STEP_SUMMARY` points at the job's
    real summary file. Tests that assert on the absence of a signal then pass
    locally and answer to the PR body in CI, and tests that call `main` write
    into the job summary and query the GitHub API for real.

    Stripping the whole consulted set before every test — not patching the
    variables a given test happens to think about — makes the isolation hold
    for tests written later, which cannot know what the script grew to read.
    """

    def setUp(self) -> None:
        super().setUp()
        patcher = mock.patch.dict(os.environ)
        patcher.start()
        self.addCleanup(patcher.stop)
        for name in CONSULTED_ENV:
            os.environ.pop(name, None)


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


class TranscriptDiffTests(IsolatedEnvironmentTestCase):
    def test_the_isolation_covers_every_variable_the_script_reads(self) -> None:
        # The fixture is the thing under test here: whatever the script
        # consults must be absent by the time a test body runs, and the
        # extraction that decides "whatever the script consults" must still be
        # finding the variables we know are in there.
        self.assertTrue(KNOWN_CONSULTED_ENV.issubset(CONSULTED_ENV))
        for name in CONSULTED_ENV:
            with self.subTest(variable=name):
                self.assertNotIn(name, os.environ)

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

    def test_a_dispatched_run_reads_the_justification_from_the_pull_request(
        self,
    ) -> None:
        # A workflow_dispatch payload carries no pull request at all, which is
        # what made the sanctioned recovery path unable to pass a gate the PR
        # body already satisfied.
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(
                json.dumps({"inputs": {}, "ref": "refs/heads/topic"}),
                encoding="utf-8",
            )
            with (
                mock.patch.object(MODULE.subprocess, "run", return_value=completed),
                mock.patch.dict(
                    MODULE.os.environ,
                    {
                        "GITHUB_EVENT_PATH": str(event_path),
                        "GITHUB_TOKEN": "token",
                        "GITHUB_REPOSITORY": "owner/repo",
                        "GITHUB_REF_NAME": "topic",
                        "GITHUB_HEAD_REF": "",
                    },
                ),
                mock.patch.object(
                    MODULE.urllib.request,
                    "urlopen",
                    side_effect=self.fake_pull_request_query(
                        [{"body": "Transcript: the fixture commits twice."}]
                    ),
                ),
            ):
                exit_code = MODULE.main(["--enforce", "base...head"])

        self.assertEqual(exit_code, 0)

    def test_the_query_asks_for_this_branch_in_this_repository(self) -> None:
        seen: list[str] = []

        def record(request, timeout=None):  # noqa: ANN001, ARG001
            seen.append(request.full_url)
            self.assertEqual(request.headers["Authorization"], "Bearer token")
            return self.json_response([])

        with (
            mock.patch.dict(
                MODULE.os.environ,
                {
                    "GITHUB_TOKEN": "token",
                    "GITHUB_REPOSITORY": "owner/repo",
                    "GITHUB_REF_NAME": "topic",
                    "GITHUB_HEAD_REF": "",
                },
            ),
            mock.patch.object(MODULE.urllib.request, "urlopen", side_effect=record),
        ):
            MODULE.queried_pull_request_body()

        self.assertEqual(len(seen), 1)
        self.assertIn("/repos/owner/repo/pulls", seen[0])
        self.assertIn("head=owner%3Atopic", seen[0])
        self.assertIn("state=open", seen[0])

    def test_a_dispatched_run_still_fails_without_a_justification(self) -> None:
        # The fallback must not become a way through: an open PR whose body
        # says nothing is the same answer as no PR.
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=completed),
            mock.patch.dict(
                MODULE.os.environ,
                {
                    "GITHUB_EVENT_PATH": "",
                    "GITHUB_TOKEN": "token",
                    "GITHUB_REPOSITORY": "owner/repo",
                    "GITHUB_REF_NAME": "topic",
                    "GITHUB_HEAD_REF": "",
                },
            ),
            mock.patch.object(
                MODULE.urllib.request,
                "urlopen",
                side_effect=self.fake_pull_request_query([{"body": "## Validation"}]),
            ),
        ):
            exit_code = MODULE.main(["--enforce", "base...head"])

        self.assertEqual(exit_code, 1)

    def test_an_unreachable_api_fails_closed(self) -> None:
        completed = mock.Mock(stdout=DURABLE_DIFF)
        with (
            mock.patch.object(MODULE.subprocess, "run", return_value=completed),
            mock.patch.dict(
                MODULE.os.environ,
                {
                    "GITHUB_EVENT_PATH": "",
                    "GITHUB_TOKEN": "token",
                    "GITHUB_REPOSITORY": "owner/repo",
                    "GITHUB_REF_NAME": "topic",
                    "GITHUB_HEAD_REF": "",
                },
            ),
            mock.patch.object(
                MODULE.urllib.request,
                "urlopen",
                side_effect=OSError("no route to host"),
            ),
        ):
            exit_code = MODULE.main(["--enforce", "base...head"])

        self.assertEqual(exit_code, 1)

    def test_no_credentials_means_no_query(self) -> None:
        # Locally there is no token and no event payload, so the gate must not
        # try to reach the network at all — and must not treat the silence as
        # approval. The fixture's stripped environment is what makes this the
        # real question: with an ambient payload in scope the gate would answer
        # out of the surrounding job's PR body instead.
        with mock.patch.object(
            MODULE.urllib.request, "urlopen", side_effect=AssertionError("queried")
        ):
            self.assertIsNone(MODULE.event_pull_request_body())
            self.assertIsNone(MODULE.queried_pull_request_body())
            self.assertFalse(MODULE.has_transcript_justification())

    def test_an_event_body_without_a_sentence_consults_the_live_pull_request(
        self,
    ) -> None:
        # A payload is a snapshot taken when the event fired. If the sentence
        # was added to the PR afterwards, the payload is stale, and the branch
        # should not need another push to say something already written.
        with tempfile.TemporaryDirectory() as directory:
            event_path = Path(directory) / "event.json"
            event_path.write_text(
                json.dumps({"pull_request": {"body": "## Validation"}}),
                encoding="utf-8",
            )
            with (
                mock.patch.dict(
                    MODULE.os.environ,
                    {
                        "GITHUB_EVENT_PATH": str(event_path),
                        "GITHUB_TOKEN": "token",
                        "GITHUB_REPOSITORY": "owner/repo",
                        "GITHUB_REF_NAME": "topic",
                        "GITHUB_HEAD_REF": "",
                    },
                ),
                mock.patch.object(
                    MODULE.urllib.request,
                    "urlopen",
                    side_effect=self.fake_pull_request_query(
                        [{"body": "Transcript: added after the push."}]
                    ),
                ),
            ):
                self.assertTrue(MODULE.has_transcript_justification())

    @staticmethod
    def json_response(payload: object):
        response = io.BytesIO(json.dumps(payload).encode("utf-8"))
        return contextlib.closing(response)

    def fake_pull_request_query(self, payload: object):
        def respond(request, timeout=None):  # noqa: ANN001, ARG001
            return self.json_response(payload)

        return respond

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
