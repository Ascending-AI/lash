#!/usr/bin/env python3
"""Fixture tests for the ADR convention gate, including its rollout modes."""

from __future__ import annotations

import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_adr_current as gate


class AdrCurrentTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(dir=gate.ROOT / ".tmp" if (gate.ROOT / ".tmp").exists() else None)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.write("docs/adr/0001-a-durable-decision.md", "# A durable decision\n\n## 1. Decision\nThe store records the result durably.\n\n### 1.1 Bounds\nResults are no longer than 64 bytes.\n")
        self.write(gate.README, "# Architecture decisions\n\n" + gate.CONVENTION_BAN + "\n\n" + gate.INDEX_START + "\n" + gate.INDEX_END + "\n")
        gate.check(self.root, write_index=True)

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    @staticmethod
    def citation(number: str, suffix: str = "") -> str:
        # Compose fixture citations so they are not repository citations.
        return "ADR " + number + suffix

    def run_gate(self, *args: str) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = gate.main(["--root", str(self.root), *args])
        return status, output.getvalue()

    def test_current_state_text_and_citations_pass_enforcement(self) -> None:
        self.write("crates/example/src/lib.rs", "// " + self.citation("0001", " §1.1") + "\n")
        status, output = self.run_gate("--enforce-narration", "--enforce-citations")
        self.assertEqual(0, status, output)
        self.assertIn("0 findings (enforcing)", output)

    def test_planted_amendment_block_fails(self) -> None:
        self.write("docs/adr/0001-a-durable-decision.md", "# A durable decision\n\n## Amendment (2026-09-30)\nThe decision changes.\n")
        status, output = self.run_gate("--enforce-narration")
        self.assertEqual(1, status)
        self.assertIn("0001-a-durable-decision.md:3: history narration", output)

    def test_narration_bans_cover_inline_blocks_and_wrapped_phrases(self) -> None:
        for phrase in ("**Amendment.**", "superseded", "supersedes", "no longer", "previously", "formerly", "used\nto", "was retired", "originally", "Amended 2026-09-30", "Updated 2026-09-30", "## 2026-09-30"):
            with self.subTest(phrase=phrase):
                self.write("docs/adr/0001-a-durable-decision.md", "# A durable decision\n" + phrase + "\n")
                self.assertEqual(1, self.run_gate("--enforce-narration")[0])

    def test_allowlist_does_not_hide_other_narration_on_same_line(self) -> None:
        self.write("docs/adr/0001-a-durable-decision.md", "# A durable decision\nResults are no longer than 64 bytes; formerly they were larger.\n")
        status, output = self.run_gate("--enforce-narration")
        self.assertEqual(1, status)
        self.assertIn("formerly", output)

    def test_missing_adr_fails_in_each_scanned_root(self) -> None:
        for directory in gate.SCAN_ROOTS:
            with self.subTest(directory=directory):
                relative = directory + "/citation.txt"
                self.write(relative, self.citation("0999") + "\n")
                status, output = self.run_gate("--enforce-citations")
                self.assertEqual(1, status)
                self.assertIn(relative + ":1:", output)
                (self.root / relative).unlink()

    def test_missing_section_fails(self) -> None:
        self.write("crates/example/src/lib.rs", "// " + self.citation("0001", " §9") + "\n")
        status, output = self.run_gate("--enforce-citations")
        self.assertEqual(1, status)
        self.assertIn("has no section §9", output)

    def test_linked_wrapped_and_listed_sections_resolve(self) -> None:
        self.write("docs/citations.md", "[" + self.citation("0001") + "](adr/0001-a-durable-decision.md) §1 and §1.1\n" + self.citation("0001", "\n§1.1") + "\n" + self.citation("0001", " §1/§1.1") + "\n")
        self.write("crates/example/src/lib.rs", "// ADR\n// " + "0001 §1.1\n")
        status, output = self.run_gate("--enforce-citations")
        self.assertEqual(0, status, output)
        self.assertEqual(6, gate.check(self.root).section_count)

    def test_linked_section_and_range_endpoint_cannot_escape(self) -> None:
        for suffix in ("](adr/0001-a-durable-decision.md) §9", " §1 and §9", " §1–9"):
            with self.subTest(suffix=suffix):
                prefix = "[" if suffix.startswith("]") else ""
                self.write("docs/citations.md", prefix + self.citation("0001", suffix) + "\n")
                self.assertEqual(1, self.run_gate("--enforce-citations")[0])

    def test_bare_section_in_adr_resolves_locally(self) -> None:
        path = self.root / "docs/adr/0001-a-durable-decision.md"
        path.write_text(path.read_text() + "See §9.\n", encoding="utf-8")
        self.assertEqual(1, self.run_gate("--enforce-citations")[0])

    def test_external_section_in_adr_is_not_rechecked_locally(self) -> None:
        self.write("docs/adr/0002-another-decision.md", "# Another decision\n## 2. Rule\nSee " + self.citation("0001", " §1 and §1.1") + ".\n")
        gate.check(self.root, write_index=True)
        status, output = self.run_gate("--enforce-citations")
        self.assertEqual(0, status, output)

    def test_qualified_external_sections_and_annotated_lists_resolve(self) -> None:
        self.write("docs/adr/0002-another-decision.md", "# Another decision\n## 2. Rule\n" + self.citation("0001", "'s bounds (its §1.1).\n\n") + self.citation("0001", "'s rule (§1 there).\n\n") + self.citation("0001", " §1 (the rule) and §1.1 (the bound).\n\n") + self.citation("0001", " §1 to §1.1.\n"))
        gate.check(self.root, write_index=True)
        status, output = self.run_gate("--enforce-citations")
        self.assertEqual(0, status, output)
        self.assertEqual(6, gate.check(self.root).section_count)

    def test_code_fence_cannot_supply_a_section_heading(self) -> None:
        path = self.root / "docs/adr/0001-a-durable-decision.md"
        path.write_text(path.read_text() + "```markdown\n## 9. Fake heading\n```\n", encoding="utf-8")
        self.write("docs/citations.md", self.citation("0001", " §9") + "\n")
        self.assertEqual(1, self.run_gate("--enforce-citations")[0])

    def test_rollout_warns_and_succeeds_with_both_defects(self) -> None:
        path = self.root / "docs/adr/0001-a-durable-decision.md"
        path.write_text(path.read_text() + "## Amendment\n", encoding="utf-8")
        self.write("docs/citations.md", self.citation("0999") + "\n")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            # Independent of the eventual enforcing repository defaults.
            with mock.patch.object(gate, "ENFORCE_NARRATION", False), mock.patch.object(gate, "ENFORCE_CITATIONS", False):
                status = gate.main(["--root", str(self.root)])
        self.assertEqual(0, status)
        self.assertIn("WARN", output.getvalue())
        self.assertIn("report-only", output.getvalue())

    def test_stale_index_fails_and_regeneration_preserves_convention(self) -> None:
        self.write("docs/adr/0002-another-decision.md", "# 0002: Another decision\n")
        self.assertEqual(1, self.run_gate()[0])
        status, output = self.run_gate("--write-index")
        self.assertEqual(0, status, output)
        self.assertIn(gate.CONVENTION_BAN, (self.root / gate.README).read_text())
        self.assertIn("| 0002 | [Another decision]", (self.root / gate.README).read_text())
        (self.root / "docs/adr/0002-another-decision.md").unlink()
        self.assertEqual(1, self.run_gate()[0])
        self.assertEqual(0, self.run_gate("--write-index")[0])

    def test_title_drift_fails(self) -> None:
        self.write("docs/adr/0001-a-durable-decision.md", "# A different title\n")
        self.assertEqual(1, self.run_gate()[0])

    def test_missing_duplicate_or_reversed_index_markers_fail(self) -> None:
        for text in ("# No index\n", gate.INDEX_START * 2 + gate.INDEX_END, gate.INDEX_END + gate.INDEX_START):
            with self.subTest(text=text):
                self.write(gate.README, text)
                self.assertEqual(1, self.run_gate("--write-index")[0])

    def test_duplicate_adr_number_fails(self) -> None:
        self.write("docs/adr/0001-duplicate.md", "# Duplicate\n")
        status, output = self.run_gate("--write-index")
        self.assertEqual(1, status)
        self.assertIn("duplicate ADR number", output)

    def test_binary_and_dependency_trees_do_not_enter_scan(self) -> None:
        self.write("examples/node_modules/dependency/citation.md", self.citation("0999"))
        (self.root / "docs/binary.bin").write_bytes(b"\0" + self.citation("0999").encode())
        self.assertEqual(0, self.run_gate("--enforce-citations")[0])


if __name__ == "__main__":
    unittest.main()
