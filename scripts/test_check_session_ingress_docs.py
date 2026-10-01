#!/usr/bin/env python3
"""Red-side and requirement tests for the session ingress doc gate."""

from pathlib import Path
import subprocess
import tempfile
import unittest

import check_session_ingress_docs as checker


ROOT = Path(__file__).resolve().parents[1]
DOC_PASSAGE = checker.Passage("unit", "docs/adr/planted.md", r"^", None)
ENTRY_PASSAGE = checker.Passage("unit", "CONTEXT.md", r"^", None)


def denied(text: str, passage: checker.Passage = DOC_PASSAGE) -> list[str]:
    violations: list[str] = []
    checker.check_denied(Path(passage.file), passage, text, violations)
    return violations


def required(text: str, *patterns: str) -> list[str]:
    passage = checker.Passage("unit", "docs/adr/planted.md", r"^", None, required=tuple(patterns))
    violations: list[str] = []
    checker.check_required(Path(passage.file), passage, text, violations)
    return violations


class DenialTests(unittest.TestCase):
    def test_parked_not_yet_implemented_is_retired(self) -> None:
        self.assertTrue(denied("- **Parked Turn**: Decided by ADR 0101, not yet implemented."))
        self.assertFalse(denied("- **Parked Turn**: Decided by ADR 0101, implemented."))

    def test_retired_patch_api_names_are_retired(self) -> None:
        for name in (
            "SessionConfigPatch",
            "ApplyConfigPatch",
            "patch_session_config",
            "resolve_session_config",
            "SessionConfigAdmin::update",
        ):
            with self.subTest(name=name):
                self.assertTrue(denied(f"A host calls {name} to change config."))

    def test_patch_wording_is_retired_in_every_form(self) -> None:
        for wording in (
            "a config patch on the command lane",
            "Adjacent config patches apply together",
            "changes configuration through a patch",
            "submit explicit patches",
            "before submitting the patch",
            "configuration and its patch",
        ):
            with self.subTest(wording=wording):
                self.assertTrue(denied(wording))
        for wording in (
            "a typed config transaction on the command lane",
            "a dispatch of the command run",
            "submitted under ConfigWrite",
        ):
            with self.subTest(wording=wording):
                self.assertFalse(denied(wording))

    def test_adjacent_commands_coalescing_is_retired(self) -> None:
        self.assertTrue(denied("Adjacent config transactions coalesce within the command bound."))
        self.assertFalse(denied("Each transaction applies alone."))

    def test_merge_key_and_authority_as_gates_are_retired(self) -> None:
        stale = (
            "authority principal, elevation, age, row count, and rendered "
            "context reserve remain independent gates"
        )
        self.assertTrue(denied(stale))
        self.assertTrue(denied("work kind, delivery boundary, authority and elevation remain\nindependent admission gates"))
        for current in (
            "merge key and authority are per-item data for the host's drain policy",
            "only batchable turn work sharing the head's delivery policy travels with it",
            "an opaque principal and elevation stamp",
        ):
            with self.subTest(current=current):
                self.assertFalse(denied(current))

    def test_turn_level_override_bans_are_retired(self) -> None:
        for stale in (
            "The runtime has no turn-level model overlay.",
            "_Avoid_: turn-level model override (one resolution point).",
            "A turn-level overlay is rejected.",
            "Durable per-root overrides are forbidden.",
        ):
            with self.subTest(stale=stale):
                self.assertTrue(denied(stale))
        for current in (
            "a RunSpec's recorded per-root overrides",
            "overrides shape that root only",
            "an ephemeral per-turn config channel beside RunSpec",
        ):
            with self.subTest(current=current):
                self.assertFalse(denied(current))

    def test_route_refusal_failing_a_queued_turn_is_retired(self) -> None:
        for stale in (
            "a refusal at apply leaves the model unchanged and fails the turn queued behind it",
            "the automatic failure of the following turn",
        ):
            with self.subTest(stale=stale):
                self.assertTrue(denied(stale))
        for current in (
            "a Refused outcome publishes no config",
            "the queued inputs behind it run unaffected",
            "opening a historical frame fails with a typed refusal",
        ):
            with self.subTest(current=current):
                self.assertFalse(denied(current))

    def test_submit_time_route_validation_is_retired(self) -> None:
        self.assertTrue(denied("The route is validated when the command is sent."))
        self.assertTrue(denied("Route validation at submission refuses an unknown provider."))
        for current in (
            "a changed route is validated at the transaction's recorded resolution",
            "submission decodes every entry and refuses what no registration serves",
        ):
            with self.subTest(current=current):
                self.assertFalse(denied(current))


class RequiredTests(unittest.TestCase):
    def test_missing_required_terms_fail(self) -> None:
        violations = required("Config changes ride the command lane.", r"\bADR 0126\b", r"\bConfigTransaction\b")
        self.assertEqual(len(violations), 2)

    def test_adr_0101_a5_must_name_its_adr(self) -> None:
        self.assertTrue(required("see §A5", checker.ADR_0101_A5))
        self.assertFalse(required("see ADR 0101 §A5", checker.ADR_0101_A5))
        self.assertFalse(required("decided by [[ADR 0101]] (§A5)", checker.ADR_0101_A5))


class LinkTests(unittest.TestCase):
    def test_code_refs_and_markdown_links_resolve(self) -> None:
        violations: list[str] = []
        checker.check_links(
            ROOT,
            ROOT / "docs/adr/planted.md",
            DOC_PASSAGE,
            "See `crates/lash/src/send.rs:1` and [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md).",
            violations,
        )
        self.assertEqual(violations, [])

    def test_missing_files_and_past_end_anchors_fail(self) -> None:
        violations: list[str] = []
        checker.check_links(
            ROOT,
            ROOT / "docs/adr/planted.md",
            DOC_PASSAGE,
            "See `crates/lash/src/no_such_file.rs`, `crates/lash/src/send.rs:999999` "
            "and [gone](0999-no-such-adr.md).",
            violations,
        )
        self.assertEqual(len(violations), 3)


class RepositoryTests(unittest.TestCase):
    def test_the_current_tree_passes(self) -> None:
        self.assertEqual(checker.check(ROOT), [])

    def test_a_planted_stale_tree_fails(self) -> None:
        temporary_root = ROOT / "target"
        temporary_root.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=temporary_root) as directory:
            root = Path(directory)
            for passage in checker.PASSAGES:
                target = root / passage.file
                target.parent.mkdir(parents=True, exist_ok=True)
            (root / checker.CONTEXT).write_text(
                "- **Queued Work**: matching merge keys and equal authority "
                "principal, elevation, row count and rendered context reserve "
                "remain independent gates.\n"
                "- **Session Model**: it changes only by a session command, a "
                "config patch on the command lane. The route is validated when "
                "the command is sent and again when it is applied; a refusal "
                "at apply fails the turn queued behind it. _Avoid_: "
                "turn-level model override.\n"
                "- **Parked Turn**: decided by ADR 0101, not yet implemented.\n"
            )
            (root / checker.ADR_0030).write_text(
                "## Decision\n\nThe runtime has no turn-level model overlay. "
                "A product host changes configuration through a "
                "SessionConfigPatch.\n"
            )
            (root / checker.ADR_0101).write_text(
                "### 4. Commands\n\nAdjacent config patches coalesce within "
                "the command bound. Other commands apply alone.\n\n### 5. Ordering\n"
            )
            (root / checker.WAKE_DOC).write_text(
                "/// Constant producer-selected merge key for process wakes.\n"
                "/// Authority, elevation, row count, age, and rendered size "
                "remain independent admission gates.\n"
                "pub const PROCESS_WAKE_MERGE_KEY: &str = \"wake\";\n"
            )
            violations = checker.check(root)
            self.assertGreaterEqual(len(violations), 8, "\n".join(violations))
            for ruling, _ in checker.DENIED:
                self.assertTrue(
                    any(ruling in violation for violation in violations),
                    f"no violation names the ruling {ruling!r}",
                )


if __name__ == "__main__":
    unittest.main()
