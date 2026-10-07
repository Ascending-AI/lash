//! The change feeds' clocks, taken at a transaction's tail (FIG-5275).
//!
//! `turns_changed_since` and the process feed hand out cursors over a
//! sequence that must never skip a lower number that commits late. Each feed
//! keeps that law with one singleton clock row: a transaction that records a
//! change bumps the row, and the row lock it takes orders the transactions
//! that bump it by sequence, from the bump to `COMMIT`. The lock is
//! fleet-wide, so where the bump sits decides how long every other writer of
//! the feed waits.
//!
//! A guarded transaction therefore records its changes here as it goes, and
//! [`ChangeFeeds::flush`] writes them as the transaction's last statements,
//! right before `COMMIT`: each bumps its clock and writes the row it
//! sequences in one statement. The clock is held for that statement and the
//! `COMMIT` only, and the feed's ordering is the one it always had: no two
//! transactions hold a clock at once, and the one that bumps first commits
//! first. The turn clock is taken before the process clock, so two
//! transactions that record both never wait on each other in a cycle.

use sqlx::PgConnection;

use crate::process_sql::process_sql;
use crate::session_sql::session_sql;

/// A turn's commit receipt, sequenced at the tail.
#[derive(Debug)]
pub(crate) struct TurnReceipt {
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) turn_commit_hash: String,
    pub(crate) result_json: String,
    pub(crate) outcome_code: Option<String>,
    pub(crate) committed_at_ms: i64,
    pub(crate) request_identity_hash: Option<String>,
    pub(crate) requested_node_count: Option<i64>,
    pub(crate) identity_encoding_version: Option<i32>,
    pub(crate) failure_evidence: bool,
}

/// One change of the turn feed.
#[derive(Debug)]
pub(crate) enum TurnChange {
    /// A runtime commit's receipt.
    Receipt(TurnReceipt),
    /// A session's terminal record: its deletion, or its fault.
    SessionTerminal {
        session_id: String,
        fault_json: Option<String>,
        recorded_at_ms: i64,
    },
}

/// The changes one transaction recorded, not yet sequenced.
#[derive(Debug, Default)]
pub(crate) struct ChangeFeeds {
    turns: Vec<TurnChange>,
    /// Every process save, in order; a process saved twice holds the
    /// sequence of its last save.
    processes: Vec<String>,
}

impl ChangeFeeds {
    pub(crate) fn record_turn(&mut self, change: TurnChange) {
        self.turns.push(change);
    }

    pub(crate) fn record_process(&mut self, process_id: &str) {
        self.processes.push(process_id.to_owned());
    }

    /// Write every recorded change, each with its feed's next sequence:
    /// the transaction's last statements before `COMMIT`.
    pub(crate) async fn flush(&mut self, tx: &mut PgConnection) -> Result<(), sqlx::Error> {
        for change in std::mem::take(&mut self.turns) {
            match change {
                TurnChange::Receipt(receipt) => {
                    sqlx::query(session_sql().turn_commits_postgres.insert_sequenced.sql())
                        .bind(receipt.session_id)
                        .bind(receipt.turn_id)
                        .bind(receipt.turn_commit_hash)
                        .bind(receipt.result_json)
                        .bind(receipt.outcome_code)
                        .bind(receipt.committed_at_ms)
                        .bind(receipt.request_identity_hash)
                        .bind(receipt.requested_node_count)
                        .bind(receipt.identity_encoding_version)
                        .bind(receipt.failure_evidence)
                        .execute(crate::observed_sql::executor(&mut *tx))
                        .await?;
                }
                TurnChange::SessionTerminal {
                    session_id,
                    fault_json,
                    recorded_at_ms,
                } => {
                    sqlx::query(
                        session_sql()
                            .turn_commits_postgres
                            .insert_session_terminal_sequenced
                            .sql(),
                    )
                    .bind(session_id)
                    .bind(fault_json)
                    .bind(recorded_at_ms)
                    .execute(crate::observed_sql::executor(&mut *tx))
                    .await?;
                }
            }
        }
        let processes = std::mem::take(&mut self.processes);
        if !processes.is_empty() {
            sqlx::query(process_sql().clock_postgres.sequence_changes.sql())
                .bind(&processes)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
        }
        Ok(())
    }
}
