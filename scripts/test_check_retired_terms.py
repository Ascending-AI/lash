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

    def test_retired_identifiers_fail_without_history_exemption(self) -> None:
        for identifier in (
            "memory_store_backend", "memory_store_set", "memory_store",
            "memory_store_with_options", "a_memory_stores_lives_until_its_last_handle_drops",
            "law_in_memory", "InMemorySessionStore", "JournaledCommitController",
            "store_journal_host", "store-journal",
        ):
            for path in ("crates/example/src/lib.rs", "scripts/run.sh", "examples/demo/Cargo.toml"):
                with self.subTest(identifier=identifier, path=path):
                    self.assertTrue(checker.check_text(Path(path), f'fn {identifier}() {{}} // Retired in 60e0e86b2a\n'))

    def test_current_identifiers_and_real_memory_models_pass(self) -> None:
        for identifier in (
            "sqlite_memory_store_backend", "sqlite_memory_store_set",
            "sqlite_memory_store_with_options", "law_on_sqlite_memory",
            "EngineOwnedCommitLayer", "open_in_memory",
            "InMemoryLiveReplayStore", "InMemoryLiveReplayStoreConfig",
            "InMemoryRootLedger", "InMemoryRoots", "InMemoryDriveEpochs",
            "InMemoryLashlangArtifactStore", "InMemoryArtifactState",
            "InMemorySpanExporter", "InMemoryMetricExporter", "InMemory",
            "s3_attachment_store_satisfies_conformance_with_in_memory_object_store",
        ):
            with self.subTest(identifier=identifier):
                self.assertFalse(checker.check_text(Path("crates/example/src/lib.rs"), f"fn {identifier}() {{}}"))

    def test_identifier_rules_cover_filters_and_features(self) -> None:
        self.assertTrue(checker.check_text(Path("scripts/run.sh"), "kiln test --test_arg=law_in_memory"))
        self.assertTrue(checker.check_text(Path("crates/example/Cargo.toml"), '[features]\nstore-journal = []'))
        self.assertTrue(checker.check_text(Path("crates/example/src/lib.rs"), 'let label = "memory_store_backend";'))
        self.assertFalse(checker.check_text(Path("crates/example/src/lib.rs"), '// The store-journal host was retired in 476264fbea.'))

    def test_identifier_exceptions_cannot_hide_retired_names(self) -> None:
        path = Path("crates/example/src/lib.rs")
        for body in (
            '#[cfg(feature = "store-journal")] fn example() {}',
            "fn sqlite_memory_store_JournaledCommit() {}",
            "fn InMemoryLiveReplayStoreBackend() {}",
            "fn notsqlite_memory_store() {}",
        ):
            with self.subTest(body=body):
                self.assertTrue(checker.check_text(path, body))
        self.assertFalse(checker.check_text(Path("scripts/check-guarded-transactions.py"), 'pattern = "_in_memory"'))
        self.assertTrue(checker.check_text(Path("scripts/other.py"), 'pattern = "_in_memory"'))
        self.assertFalse(checker.check_text(Path("scripts/check-substrate-boundary.sh"), 'retired="InMemorySessionStore"'))
        self.assertTrue(checker.check_text(Path("crates/example/src/lib.rs"), "struct InMemorySessionStore;"))

    def test_queue_drain_ownership_is_refused_everywhere(self) -> None:
        planted = (
            ("crates/example/src/lib.rs", "enum Scope { QueueDrain { drain_id: String } }"),
            ("crates/example/src/lib.rs", "let scope = AdmittedScope::queue_drain(session, id);"),
            ("crates/example/src/lib.rs", "let scope = state.queue_drain_scope(batch);"),
            ("crates/example/src/lib.rs", "EffectOpener::session_queue_drain_encoding_range(id)"),
            ("crates/example/src/lib.rs", "// A queued root runs under its QueueDrain scope."),
            ("crates/example/tests/law.rs", "let scope = ExecutionScope::queue_drain(s, d);"),
            ("crates/example/schema.sql", "CHECK (parent_kind IN ('turn', 'queue_drain'))"),
            ("schemas/host/planted/v1.schema.json", '"const": "queue_drain",'),
            ("docs/adr/planted.md", "An opener is `Turn`, `QueueDrain` or `Process`."),
            ("docs/adr/planted.md", "Retired in 60e0e86b2a: the QueueDrain opener."),
        )
        for path, body in planted:
            with self.subTest(path=path, body=body):
                self.assertTrue(checker.check_queue_drain(Path(path), body))
        self.assertTrue(checker.check_text(Path("crates/example/src/lib.rs"), planted[0][1]))
        self.assertTrue(checker.check_text(Path("docs/adr/planted.md"), planted[-1][1]))
        for body in (
            "let scope = ExecutionScope::session_operation(session, batch);",
            '"runtime.command_only_queue_drain" => facts.push(command_queue_drain_fact()),',
            "const COMMAND_ONLY_QUEUE_DRAIN: Coverage = coverage();",
        ):
            with self.subTest(body=body):
                self.assertFalse(checker.check_queue_drain(Path("crates/example/src/lib.rs"), body))

    def test_repository_cli_rejects_planted_identifiers(self) -> None:
        temporary_root = Path(__file__).resolve().parents[1] / "target"
        temporary_root.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir=temporary_root) as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            source = root / "crates" / "planted" / "src" / "lib.rs"
            source.parent.mkdir(parents=True)
            command = ["python3", str(Path(checker.__file__).resolve()), "--root", str(root)]
            source.write_text("fn memory_store_backend() {}\nfn law_in_memory() {}\n")
            red = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(red.returncode, 1, red.stdout + red.stderr)
            self.assertIn("crates/planted/src/lib.rs:1", red.stdout)
            self.assertIn("crates/planted/src/lib.rs:2", red.stdout)
            source.write_text("fn sqlite_memory_store_backend() {}\nfn law_on_sqlite_memory() {}\n")
            green = subprocess.run(command, text=True, capture_output=True)
            self.assertEqual(green.returncode, 0, green.stdout + green.stderr)

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
