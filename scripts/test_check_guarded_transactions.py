#!/usr/bin/env python3
"""Fixture tests for check-guarded-transactions.py, plus the tree itself."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check-guarded-transactions.py")
SPEC = importlib.util.spec_from_file_location("check_guarded_transactions", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)

SRC = "crates/lash-postgres-store/src"

STATEMENTS = """\
lash_store_sql::statements! {
    pub struct WidgetStatements @ "widgets" {
        /// A write.
        insert = "INSERT INTO widgets (id) VALUES (?1)";
        select_all = "SELECT id FROM widgets";
        bump = "UPDATE widgets SET n = n + 1 WHERE id = ?1";
    }
}
"""

LIB = """\
#[path = "postgres/guarded_tx.rs"]
mod guarded_tx;
#[path = "postgres/widgets.rs"]
mod widgets;
#[cfg(test)]
#[path = "postgres/widget_tests.rs"]
mod widget_tests;
"""

GUARD = """\
pub(crate) async fn begin_guarded(pool: &PgPool) -> Tx {
    pool.begin().await
}
"""


class CheckTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.write("crates/lash-store-sql/src/widgets.rs", STATEMENTS)
        self.write(f"{SRC}/lib.rs", LIB)
        self.write(f"{SRC}/postgres/guarded_tx.rs", GUARD)
        self.allow("")

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")

    def allow(self, text: str) -> None:
        self.write(gate.ALLOWLIST, text)

    def widgets(self, body: str) -> None:
        self.write(f"{SRC}/postgres/widgets.rs", body)

    def test_a_transaction_begun_outside_the_guard_is_flagged(self) -> None:
        self.widgets(
            """\
            async fn save(&self) {
                let mut tx = self.pool.begin().await?;
                tx.commit().await?;
            }
            """
        )
        problems = gate.check(self.repo)
        self.assertEqual(len(problems), 1)
        self.assertIn(f"{SRC}/postgres/widgets.rs:2: save", problems[0])

    def test_the_guarded_entry_and_a_guarded_site_pass(self) -> None:
        self.widgets(
            """\
            async fn save(&self) {
                let mut tx = begin_guarded(&self.pool, &self.fence).await?;
                sqlx::query(SQL.insert.sql()).execute(&mut **tx).await?;
                tx.commit().await?;
            }
            """
        )
        self.assertEqual(gate.check(self.repo), [])

    def test_an_isolation_level_after_the_fence_is_flagged(self) -> None:
        self.widgets(
            """\
            async fn snapshot(&self) {
                let mut tx = begin_guarded(&self.pool, &self.fence).await?;
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                    .execute(&mut **tx)
                    .await?;
            }
            """
        )
        problems = gate.check(self.repo)
        self.assertEqual(len(problems), 1)
        self.assertIn("isolation level set after the fence", problems[0])

    def test_connection_begin_spellings_are_flagged(self) -> None:
        self.widgets(
            """\
            async fn step(connection: &mut PgConnection) {
                let tx = sqlx::Connection::begin(&mut *connection).await?;
            }
            """
        )
        self.assertEqual(len(gate.check(self.repo)), 1)

    def test_a_named_write_run_straight_on_a_pool_is_flagged(self) -> None:
        self.widgets(
            """\
            async fn bump(&self, id: &str) {
                sqlx::query(SQL.bump.sql())
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            """
        )
        problems = gate.check(self.repo)
        self.assertEqual(len(problems), 1)
        self.assertIn("mutating statement run straight on a pool", problems[0])

    def test_a_literal_write_on_a_checked_out_connection_is_flagged(self) -> None:
        self.widgets(
            """\
            async fn wipe(&self) {
                let mut connection = acquire_runtime_connection(&self.pool).await?;
                sqlx::query("DELETE FROM lash_widgets").execute(&mut *connection).await?;
            }
            """
        )
        self.assertEqual(len(gate.check(self.repo)), 1)

    def test_a_read_run_straight_on_a_pool_passes(self) -> None:
        self.widgets(
            """\
            async fn all(&self) {
                sqlx::query(SQL.select_all.sql()).fetch_all(&self.pool).await?;
            }
            """
        )
        self.assertEqual(gate.check(self.repo), [])

    def test_unresolved_sql_on_a_pool_is_flagged_until_allowlisted(self) -> None:
        self.widgets(
            """\
            async fn listing(&self, sql: &str) {
                sqlx::query(sql).fetch_all(&self.pool).await?;
            }
            """
        )
        problems = gate.check(self.repo)
        self.assertEqual(len(problems), 1)
        self.assertIn("unresolved SQL", problems[0])
        self.allow(f"{SRC}/postgres/widgets.rs::listing -- runs the listing SELECT its callers pass\n")
        self.assertEqual(gate.check(self.repo), [])

    def test_test_code_and_comments_are_out_of_scope(self) -> None:
        self.write(
            f"{SRC}/postgres/widget_tests.rs",
            "async fn seeds(pool: &PgPool) { let tx = pool.begin().await; }\n",
        )
        self.widgets(
            """\
            // A comment may say `pool.begin()` without beginning anything.
            fn nothing() {}

            #[cfg(test)]
            mod tests {
                async fn seeds(pool: &PgPool) {
                    let tx = pool.begin().await;
                }
            }
            """
        )
        self.assertEqual(gate.check(self.repo), [])

    def test_an_allowlist_entry_needs_a_reason_and_a_site(self) -> None:
        self.widgets("fn nothing() {}\n")
        self.allow(
            f"{SRC}/postgres/widgets.rs::nothing\n"
            f"{SRC}/postgres/widgets.rs::gone -- used to read\n"
        )
        problems = gate.check(self.repo)
        self.assertEqual(len(problems), 2)
        self.assertIn("expected", problems[0])
        self.assertIn("stale entry", problems[1])


class TreeTests(unittest.TestCase):
    def test_the_tree_begins_every_mutating_transaction_at_the_guard(self) -> None:
        self.assertEqual(gate.check(gate.REPO_ROOT), [])


if __name__ == "__main__":
    unittest.main()
