#!/usr/bin/env python3
"""Self-test for `scripts/check-store-sql-ownership.py`.

Every test seeds one violation into a copy of the real repository tree and
asserts the gate refuses it, because a gate that has only ever been observed
passing proves nothing. Each case runs against a throwaway copy of the real
trees the gate reads, with one file mutated, so the seed lands on the actual
statements the stores declare rather than on a fixture that can drift.
"""

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


def load_gate():
    module_path = ROOT / "scripts" / "check-store-sql-ownership.py"
    spec = importlib.util.spec_from_file_location("check_store_sql_ownership", module_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Registered before execution: the gate uses `@dataclass`, which resolves
    # its own annotations through `sys.modules`.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


GATE = load_gate()

# Everything the gate reads. Copying only these keeps a case under a second.
COPIED = (
    "crates/lash-store-sql",
    "crates/lash-sqlite-store/src",
    "crates/lash-postgres-store/src",
    # The manifest exempts this subtree, and the gate checks it is really there.
    "runbooks/restate-postgres-workers/src",
)
# The manifest's exempted sources, which the gate checks still exist.
COPIED_FILES = ("crates/lash-sim/src/postgres_replay.rs",)


class SeededTree:
    """A throwaway copy of the trees the gate reads."""

    def __init__(self) -> None:
        self.directory = Path(tempfile.mkdtemp(prefix="store-sql-gate-"))
        for relative in COPIED:
            source = ROOT / relative
            destination = self.directory / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(source, destination)
        # `production_sources` globs `crates|examples|runbooks`; the two it is
        # not given must exist so the glob is empty rather than an error.
        for relative in COPIED_FILES:
            destination = self.directory / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, destination)
        for base in ("examples", "runbooks"):
            (self.directory / base).mkdir(exist_ok=True, parents=True)

    def read(self, relative: str) -> str:
        return (self.directory / relative).read_text(encoding="utf-8")

    def write(self, relative: str, text: str) -> None:
        path = self.directory / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def substitute(self, relative: str, old: str, new: str) -> None:
        text = self.read(relative)
        assert old in text, f"{relative} no longer contains the anchor this test seeds into"
        self.write(relative, text.replace(old, new, 1))

    def failures(self) -> list[str]:
        return GATE.check(self.directory)

    def close(self) -> None:
        shutil.rmtree(self.directory, ignore_errors=True)


class StoreSqlOwnershipGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tree = SeededTree()
        self.addCleanup(self.tree.close)

    def assert_refused(self, fragment: str) -> None:
        failures = self.tree.failures()
        self.assertTrue(failures, "the gate accepted a seeded violation")
        self.assertTrue(
            any(fragment in failure for failure in failures),
            f"no finding mentioned {fragment!r}; got {failures}",
        )

    def test_the_real_tree_passes(self) -> None:
        self.assertEqual(self.tree.failures(), [])

    def test_a_stray_sql_literal_outside_the_table_module_is_refused(self) -> None:
        # `retention.rs` is exactly where the old copy of this read lived.
        self.tree.substitute(
            "crates/lash-sqlite-store/src/retention.rs",
            "let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
            'let _stray = "SELECT scope_id FROM runtime_effect_replay WHERE session_id IS NULL";\n'
            "    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
        )
        self.assert_refused("crates/lash-sqlite-store/src/retention.rs")

    def test_an_unnamed_sql_literal_inside_an_owner_module_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-postgres-store/src/postgres/effect_replay.rs",
            "fn hex_digest(bytes: &[u8]) -> String {",
            'const STRAY: &str = "DELETE FROM lash_runtime_effect_group WHERE group_key = $9";\n\n'
            "fn hex_digest(bytes: &[u8]) -> String {",
        )
        self.assert_refused("is not one of this module's declared statements")

    def test_a_duplicated_statement_inside_one_backend_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-sqlite-store/src/scope_fence.rs",
            '        select_all_scope_ids = "SELECT scope_id FROM effect_scope_retirements";',
            '        select_all_scope_ids = "SELECT scope_id FROM effect_scope_retirements";\n\n'
            "        /// A second name for a statement that already has one.\n"
            '        every_fenced_scope = "SELECT scope_id   FROM effect_scope_retirements";',
        )
        self.assert_refused("has the same text as")

    def test_a_shared_statement_shadowed_per_backend_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-sqlite-store/src/scope_fence.rs",
            '        select_all_scope_ids = "SELECT scope_id FROM effect_scope_retirements";',
            '        select_all_scope_ids = "SELECT scope_id FROM effect_scope_retirements";\n\n'
            "        /// A per-backend copy of a name the shared crate owns.\n"
            '        delete_by_scope = "DELETE FROM effect_scope_retirements\n'
            '             WHERE scope_id = ?1 AND ?1 <> \'\'";',
        )
        self.assert_refused("shadows the shared statement")

    def test_an_operation_present_in_one_backend_only_and_unmanifested_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-postgres-store/src/await_event.rs",
            '        cancel_session_promises = "UPDATE await_event_waits',
            "        /// A PostgreSQL-only operation nobody declared.\n"
            '        expire_stale = "DELETE FROM await_event_waits WHERE created_at_ms < ?1";\n\n'
            '        cancel_session_promises = "UPDATE await_event_waits',
        )
        self.assert_refused("await_event_wait.expire_stale")

    def test_a_manifest_entry_naming_a_backend_that_does_not_declare_it_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            'statement = "effect_scope_retirement.select_all_scope_ids"\nbackends = ["sqlite"]',
            'statement = "effect_scope_retirement.select_all_scope_ids"\n'
            'backends = ["sqlite", "postgres"]',
        )
        self.assert_refused("that store declares no such statement")

    def test_a_manifest_entry_without_a_reason_is_refused(self) -> None:
        text = self.tree.read("crates/lash-store-sql/dialect-only.toml")
        marker = 'statement = "effect_replay.renew_lease"'
        start = text.index(marker)
        reason_start = text.index("reason =", start)
        reason_end = text.index("\n", reason_start)
        self.tree.write(
            "crates/lash-store-sql/dialect-only.toml",
            text[:reason_start] + 'reason = ""' + text[reason_end:],
        )
        self.assert_refused("carries no reason")

    def test_a_column_subset_that_is_not_a_named_projection_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/src/effect/group.rs",
            '        select_by_key = "SELECT group_key, scope_id, session_id, wake, loser_disposition,\n'
            "                    children, created_at_ms",
            '        select_by_key = "SELECT group_key, scope_id, session_id, wake, loser_disposition,\n'
            "                    children, created_at_ms, next_seq",
        )
        self.assert_refused("is not one of the column lists")

    def test_a_family_removed_from_converted_makes_the_gate_silent_about_it(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            'converted = ["artifact", "attachment", "effect", "wait"]',
            'converted = ["artifact", "attachment", "effect"]',
        )
        self.tree.substitute(
            "crates/lash-sqlite-store/src/retention.rs",
            "let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
            'let _stray = "SELECT key_id FROM await_event_waits WHERE key_id = ?1";\n'
            "    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
        )
        failures = self.tree.failures()
        self.assertFalse(
            any("await_event_waits" in failure for failure in failures),
            f"the gate is meant to be silent about an unconverted family; got {failures}",
        )

    # --- FIG-3399 -------------------------------------------------------

    def test_a_cross_family_statement_without_a_manifest_entry_is_refused(self) -> None:
        """The shared quiescence read spans the effect and wait families."""
        text = self.tree.read("crates/lash-store-sql/dialect-only.toml")
        marker = '[[cross_family]]\nstatement = "effect_journal.scope_is_quiescent"'
        start = text.index(marker)
        end = text.index('"""', text.index("reason =", start) + len('reason = """')) + 3
        self.tree.write(
            "crates/lash-store-sql/dialect-only.toml", text[:start] + text[end:]
        )
        self.assert_refused("is not declared in crates/lash-store-sql/dialect-only.toml")

    def test_a_cross_family_entry_that_misstates_what_it_touches_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            'owner = "crates/lash-store-sql/src/effect.rs"\ntouches = ["await_event_waits"]',
            'owner = "crates/lash-store-sql/src/effect.rs"\ntouches = ["await_event_meta"]',
        )
        self.assert_refused("but the cross-family entry lists")

    def test_a_cross_family_entry_naming_the_wrong_owner_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            'owner = "crates/lash-store-sql/src/effect.rs"\ntouches = ["await_event_waits"]',
            'owner = "crates/lash-store-sql/src/wait/waits.rs"\ntouches = ["await_event_waits"]',
        )
        self.assert_refused("names owner `crates/lash-store-sql/src/wait/waits.rs`")

    def test_a_cross_family_entry_for_a_statement_that_is_not_cross_family_is_refused(
        self,
    ) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            '[[cross_family]]\nstatement = "effect_journal.scope_is_quiescent"',
            '[[cross_family]]\nstatement = "effect_journal.select_session_free_scope_ids"\n'
            'owner = "crates/lash-store-sql/src/effect.rs"\n'
            'touches = ["await_event_waits"]\n'
            'reason = "invented"\n\n'
            '[[cross_family]]\nstatement = "effect_journal.scope_is_quiescent"',
        )
        self.assert_refused("reaches no converted table outside its own family")

    def test_a_statement_spelling_a_vocabulary_literal_itself_is_refused(self) -> None:
        """FIG-2844's rule, held over the statement text this gate parses.

        `effect_journal.scope_is_quiescent` really does spell
        `status = 'in_progress'`; declaring that column vocabulary-valued is
        what makes spelling it a finding, and a `{{term(column)}}` token the
        remedy.
        """
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            "[families.effect.table_modules]",
            "[families.effect.vocabulary_columns]\n"
            'runtime_effect_replay = ["status"]\n\n'
            "[families.effect.table_modules]",
        )
        self.assert_refused("spells `status = '…'` over `runtime_effect_replay`")

    def test_vocabulary_columns_on_a_table_the_family_does_not_own_is_refused(self) -> None:
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            "[families.effect.table_modules]",
            "[families.effect.vocabulary_columns]\n"
            'await_event_waits = ["status"]\n\n'
            "[families.effect.table_modules]",
        )
        self.assert_refused("which is not one of its tables")

    def test_prose_that_merely_carries_a_sql_word_is_not_a_statement(self) -> None:
        """The gate matches SQL structure, not a keyword in a sentence.

        Each of these was found in the tree by the keyword-only rule: a
        conformance test name, a tool name, an HTTP route. The fourth is a
        real statement that looks like the first.
        """
        for prose, table in (
            ("the constant wake merge key must batch compatible wakes across processes", "processes"),
            ("api.sessions.select", "sessions"),
            ("/api/sessions/select", "sessions"),
            ("triggers.update", "triggers"),
            ("SELECT the processes to update, then merge them", "processes"),
        ):
            # Precondition: the keyword-only rule this replaces did match each
            # of them, so the case proves a change rather than a tautology.
            self.assertTrue(
                GATE.names_table(prose, table)
                and any(
                    keyword in prose.upper() for keyword in GATE.SQL_KEYWORDS
                ),
                f"case no longer exercises the old rule: {prose!r}",
            )
            self.assertFalse(
                GATE.is_sql_over(prose, table),
                f"prose read as SQL over `{table}`: {prose!r}",
            )
        for sql, table in (
            ("SELECT record_json FROM processes WHERE process_id = ?1", "processes"),
            ("UPDATE lash_processes SET status = $2 WHERE process_id = $1", "processes"),
            ("INSERT INTO processes (process_id) VALUES (?1)", "processes"),
            ("DELETE FROM lash_sessions WHERE session_id = $1", "sessions"),
        ):
            self.assertTrue(GATE.is_sql_over(sql, table), f"real SQL missed: {sql!r}")

    def test_a_prose_literal_naming_a_converted_table_is_not_stray_sql(self) -> None:
        self.tree.substitute(
            "crates/lash-sqlite-store/src/retention.rs",
            "let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
            'let _prose = "a SELECT over await_event_waits is how the sweep reads a promise";\n'
            "    let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
        )
        self.assertEqual(self.tree.failures(), [])

    def test_an_exempted_subtree_may_spell_sql_and_its_neighbours_may_not(self) -> None:
        stray = 'pub const PROBE: &str = "SELECT key_id FROM await_event_waits WHERE key_id = ?1";\n'
        self.tree.write("runbooks/restate-postgres-workers/src/probe_fig3399.rs", stray)
        self.assertEqual(self.tree.failures(), [])
        self.tree.write("runbooks/other-harness/src/probe_fig3399.rs", stray)
        self.assert_refused("runbooks/other-harness/src/probe_fig3399.rs")

    # --- FIG-3406 -------------------------------------------------------

    def test_a_stray_attachment_literal_is_refused_now_the_family_is_converted(self) -> None:
        """The attachment family joined `converted`, so the gate is total for it.

        `session_factory.rs` is exactly where the PostgreSQL half of the
        live-root probe was read from while it was still `format!`-built, so a
        literal reappearing there is the regression this rule exists to catch.
        """
        stray = (
            'const STRAY: &str = "SELECT 1 FROM lash_attachment_manifest '
            'WHERE attachment_id = $1";\n'
        )
        self.tree.substitute(
            "crates/lash-postgres-store/src/postgres/session_factory.rs",
            "impl PostgresSessionStoreFactory {",
            f"{stray}\nimpl PostgresSessionStoreFactory {{",
        )
        self.assert_refused("attachment_manifest")

    def test_the_gate_was_silent_about_that_literal_before_the_family_converted(self) -> None:
        """The red side of the case above: the rule, not the seed, is new."""
        self.tree.substitute(
            "crates/lash-store-sql/dialect-only.toml",
            'converted = ["artifact", "attachment", "effect", "wait"]',
            'converted = ["artifact", "effect", "wait"]',
        )
        stray = (
            'const STRAY: &str = "SELECT 1 FROM lash_attachment_manifest '
            'WHERE attachment_id = $1";\n'
        )
        self.tree.substitute(
            "crates/lash-postgres-store/src/postgres/session_factory.rs",
            "impl PostgresSessionStoreFactory {",
            f"{stray}\nimpl PostgresSessionStoreFactory {{",
        )
        # Removing the family from `converted` also orphans its manifest
        # entries, which the gate reports; the claim here is narrower, and it
        # is the whole claim: the stray literal itself goes unreported.
        self.assertFalse(
            any("session_factory.rs" in failure for failure in self.tree.failures()),
            "the gate must be silent about a family that is not in `converted`",
        )

    def test_an_attachment_statement_spelling_an_owner_label_itself_is_refused(self) -> None:
        """`owner_kind` is vocabulary-valued, so the label may only be named.

        The live-root probe carries `{{turn_attachment_owner(…)}}`; spelling
        `= 'turn'` instead is FIG-2815 undone one statement at a time.
        """
        self.tree.substitute(
            "crates/lash-store-sql/src/attachment/manifest.rs",
            "{{turn_attachment_owner(manifest.owner_kind)}}",
            "manifest.owner_kind = 'turn'",
        )
        self.assert_refused("spells `owner_kind = '…'` over `attachment_manifest`")

    def test_a_test_module_may_spell_sql_freely(self) -> None:
        self.tree.substitute(
            "crates/lash-sqlite-store/src/retention.rs",
            "let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
            "let cutoff = clamp_epoch_ms(bound.committed_before_epoch_ms);",
        )
        self.tree.write(
            "crates/lash-sqlite-store/src/retention_probe.rs",
            "#[cfg(test)]\nmod probe {\n"
            '    const SQL: &str = "SELECT scope_id FROM runtime_effect_replay";\n'
            "}\n",
        )
        self.assertEqual(self.tree.failures(), [])


if __name__ == "__main__":
    os.environ.setdefault("PYTHONHASHSEED", "0")
    unittest.main(verbosity=2)
