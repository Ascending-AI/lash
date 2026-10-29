#!/usr/bin/env python3
"""Fixture tests for check-guarded-transactions.py's PostgreSQL section, plus
the whole check over the tree."""

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
pub(crate) async fn begin_pooled(connection: PoolConnection<Postgres>, statement: String) -> Tx {
    tokio::spawn(async move { Transaction::begin(connection, Some(statement.into())).await }).await
}
pub(crate) async fn begin_guarded(pool: &PgPool) -> Tx {
    begin_pooled(pool.acquire().await?, "BEGIN".into()).await
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

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")

    def check(self) -> list[str]:
        return [
            f"{site.path}:{site.line}: {site.function}: {site.what}"
            for site in gate.check_postgres(self.repo)
        ]

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
        problems = self.check()
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
        self.assertEqual(self.check(), [])

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
        problems = self.check()
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
        self.assertEqual(len(self.check()), 1)

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
        problems = self.check()
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
        self.assertEqual(len(self.check()), 1)

    def test_a_read_run_straight_on_a_pool_passes(self) -> None:
        self.widgets(
            """\
            async fn all(&self) {
                sqlx::query(SQL.select_all.sql()).fetch_all(&self.pool).await?;
            }
            """
        )
        self.assertEqual(self.check(), [])

    def test_unresolved_sql_on_a_pool_is_flagged(self) -> None:
        self.widgets(
            """\
            async fn listing(&self, sql: &str) {
                sqlx::query(sql).fetch_all(&self.pool).await?;
            }
            """
        )
        problems = self.check()
        self.assertEqual(len(problems), 1)
        self.assertIn("unresolved SQL", problems[0])

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
        self.assertEqual(self.check(), [])

    def test_pooled_begin_forms_cannot_use_readonly_exemptions(self) -> None:
        # FIG-7551: spelling changes and an exemption cannot bypass the owner.
        for call in (
            "pool.begin()", "connection.begin()", "connection.begin_with(sql)",
            "sqlx::Connection::begin(connection)", "Acquire::begin(connection)",
            "Transaction::begin(connection, None)", "PgTx::<Postgres>::begin(connection, None)",
            "Pool::<Postgres>::begin(pool)", "PgTransactionManager::begin(connection, None)",
            "<PgConnection as sqlx::Connection>::begin(connection)",
        ):
            with self.subTest(call=call):
                self.widgets(f"use sqlx::Transaction as PgTx;\nasync fn reader(pool: &PgPool) {{ {call}.await; }}")
                self.write("scripts/guarded-transaction-readonly.txt",
                           f"postgres {SRC}/postgres/widgets.rs::reader # only reads\n")
                problems = gate.check_pooled_begins(self.repo)
                self.assertEqual(len(problems), 1)
                self.assertIn("bypasses begin_pooled", problems[0].what)

    def test_owner_file_is_not_an_exemption(self) -> None:
        self.write(f"{SRC}/postgres/guarded_tx.rs", GUARD + "\nasync fn migration(connection: &mut PgConnection) { Connection::begin(connection).await; }\n")
        problems = gate.check_pooled_begins(self.repo)
        self.assertEqual([p.function for p in problems], ["migration"])

    def test_owned_dedicated_sessions_and_typed_savepoints_are_separate(self) -> None:
        self.write(f"{SRC}/postgres/guarded_tx.rs", GUARD + """
            struct DedicatedConnection { connection: PgConnection, }
            impl DedicatedConnection {
                async fn transaction(&mut self, statement: Option<String>) {
                    Transaction::begin(&mut self.connection, statement).await;
                }
            }
            async fn savepoint<'t, 'c>(tx: &'t mut Transaction<'c, Postgres>) {
                Acquire::begin(tx).await;
            }
        """)
        self.assertEqual(gate.check_pooled_begins(self.repo), [])
        # Merely naming a pooled wrapper DedicatedConnection is insufficient.
        path = self.repo / SRC / "postgres/guarded_tx.rs"
        path.write_text(path.read_text().replace("connection: PgConnection,", "connection: PoolConnection<Postgres>,"))
        self.assertEqual(len(gate.check_pooled_begins(self.repo)), 1)

    def test_new_ownership_rule_covers_facade_replay_and_testing_features(self) -> None:
        self.write("crates/lash/src/lib.rs", "#[path = \"postgres_replay.rs\"] mod replay;\n")
        self.write("crates/lash/src/postgres_replay.rs", "async fn read(pool: &PgPool) { pool.begin().await; }\n")
        self.widgets('#[cfg(feature = "testing")]\nasync fn fixture(pool: &PgPool) { pool.begin().await; }\n')
        self.assertEqual(len(gate.check_pooled_begins(self.repo)), 2)

    def test_pooled_begin_rule_ignores_test_modules_comments_and_literals(self) -> None:
        self.widgets('''
            // pool.begin() is forbidden in production, not in this comment.
            fn text() { let text = "pool.begin()"; }
            #[cfg(test)]
            mod tests { async fn test(pool: &PgPool) { pool.begin().await; } }
        ''')
        self.assertEqual(gate.check_pooled_begins(self.repo), [])


class TreeTests(unittest.TestCase):
    def test_pooled_begin_ownership(self) -> None:
        self.assertEqual(gate.check_pooled_begins(), [])

    def test_the_tree_passes_every_section(self) -> None:
        self.assertEqual(gate.main(), 0)


if __name__ == "__main__":
    unittest.main()
