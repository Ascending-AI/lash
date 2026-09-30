#!/usr/bin/env python3
"""Red-side and context-boundary tests for the retired terminology gate."""

from pathlib import Path
import subprocess
import tempfile
import unittest

import check_retired_terms as checker


class RetiredTermsTests(unittest.TestCase):
    def check(self, body: str, suffix: str = ".md") -> list[str]:
        return checker.check_text(Path("docs/agents/planted" + suffix), body)

    def test_planted_current_adr_fails(self) -> None:
        self.assertEqual(len(checker.check_text(Path("docs/adr/planted.md"), "Every law runs on the in-memory store.\n")), 1)

    def test_adrs_have_no_history_exemptions(self) -> None:
        path = Path("docs/adr/planted.md")
        for term in checker.TERMS:
            with self.subTest(term=term.term):
                self.assertTrue(checker.check_text(path, f"{term.marker}: we used {term.term}."))
                self.assertTrue(checker.check_text(path, f"## Historical, {term.marker}\n\nWe used {term.term}."))
        self.assertTrue(checker.check_text(path, "Retired in c51e616528: exclusively owned recursive copies."))
        self.assertFalse(checker.check_text(path, "SQLite in-memory stores run the store laws."))

    def test_each_retirement_requires_its_own_commit(self) -> None:
        for term in checker.TERMS:
            with self.subTest(term=term.term):
                self.assertTrue(self.check(f"We use {term.term} today."))
                self.assertFalse(self.check(f"{term.marker}: we used {term.term}."))
                self.assertTrue(self.check(f"Retired in deadbeef00: we use {term.term}."))

    def test_sqlite_memory_passes_and_does_not_hide_another_sentence(self) -> None:
        self.assertFalse(self.check("SQLite in-memory stores run the store laws."))
        self.assertTrue(self.check("SQLite is supported. Every law runs on the in-memory store."))
        self.assertTrue(self.check("Every law runs on SQLite, PostgreSQL and the in-memory store."))
        self.assertTrue(self.check("SQLite in-memory stores and the in-memory store run laws."))

    def test_wrapped_plural_comment_fails_with_line_number(self) -> None:
        violations = self.check("// Stores include the in-memory\n// stores today.\n", ".rs")
        self.assertEqual(len(violations), 1)
        self.assertIn(":1:", violations[0])

    def test_historical_marker_cannot_escape_its_paragraph(self) -> None:
        self.assertTrue(self.check("Retired in 60e0e86b2a: the in-memory store is gone.\n\nWe use the in-memory store."))

    def test_historical_heading_ends_at_next_peer_section(self) -> None:
        body = "## Historical stores, retired in 60e0e86b2a\n\nThe in-memory store runs laws.\n\n### Details\n\nThe local process registry runs laws.\n\n## Today\n\nThe in-memory store runs laws.\n"
        violations = self.check(body)
        self.assertEqual(len(violations), 1)
        self.assertIn(":11:", violations[0])

    def test_status_history_does_not_allow_current_survival_claim(self) -> None:
        body = "## Status\n\nRetired in c51e616528: exclusively-owned copies are historical.\n\n## Amendment\n\nThe exclusively-owned-copy rule survives.\n"
        self.assertEqual(len(self.check(body)), 1)

    def test_wrapped_retirement_marker_passes(self) -> None:
        self.assertFalse(self.check("The local rewind was retired in\n9c1bbd2189."))

    def test_trailing_rust_comment_fails_but_url_literal_passes(self) -> None:
        self.assertTrue(self.check("let count = 1; // The local process registry owns this.", ".rs"))
        self.assertFalse(self.check('let url = "https://host/in-memory store";', ".rs"))

    def test_link_targets_and_code_identifiers_are_not_prose(self) -> None:
        self.assertFalse(self.check("See [ADR](0076-exclusively-owned-copies.md)."))
        self.assertFalse(self.check('let local_rewind = "in-memory store";', ".rs"))
        self.assertTrue(self.check("/// We use NativeEffectHost.", ".rs"))

    def test_repository_cli_rejects_a_planted_adr(self) -> None:
        # An ignored directory avoids exposing planted prose to a concurrent
        # repository gate. The fixture stays inside the current checkout.
        temporary_root = Path(__file__).resolve().parents[1] / "target"
        temporary_root.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=temporary_root) as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            adr = root / "docs" / "adr" / "planted.md"
            adr.parent.mkdir(parents=True)
            adr.write_text("Every law runs on the in-memory store.\n")
            command = ["python3", str(Path(checker.__file__).resolve()), "--root", str(root)]
            red = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(red.returncode, 1, red.stdout + red.stderr)
            self.assertIn("docs/adr/planted.md:1", red.stdout)
            adr.write_text("Store laws run on SQLite file/memory and PostgreSQL.\n")
            green = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(green.returncode, 0, green.stdout + green.stderr)


if __name__ == "__main__":
    unittest.main()
