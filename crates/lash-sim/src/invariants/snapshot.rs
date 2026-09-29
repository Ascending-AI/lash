//! A store set's final durable rows, as the global invariants read them.

use std::collections::BTreeSet;

use lash_sqlite_store::SqliteDatabase;
use lash_sqlite_store::testing::{RawRow, read_rows_for_testing};
use serde::Serialize;
use serde_json::Value;

const DATABASES: [SqliteDatabase; 3] = [
    SqliteDatabase::DurableCore,
    SqliteDatabase::ProcessRegistry,
    SqliteDatabase::Triggers,
];

/// One obligation column family on one row (ADR 0109 §1.1). A row that owes
/// nothing has `state: None`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ObligationRow {
    pub table: String,
    /// The family's column prefix: empty, or `start_` on `processes`.
    pub family: String,
    /// The row's primary key, rendered.
    pub key: String,
    pub id: Option<String>,
    pub state: Option<String>,
    /// When a relay may next take a `due` row (its backoff or deferral).
    pub due_at_ms: Option<u64>,
    pub stall_reason: Option<String>,
    pub last_error: Option<String>,
}

impl ObligationRow {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{}{} {}: obligation {} state={} due_at_ms={} stall_reason={} last_error={}",
            self.table,
            if self.family.is_empty() {
                String::new()
            } else {
                format!("[{}]", self.family.trim_end_matches('_'))
            },
            self.key,
            self.id.as_deref().unwrap_or("-"),
            self.state.as_deref().unwrap_or("NULL"),
            self.due_at_ms
                .map_or_else(|| "NULL".to_owned(), |due| due.to_string()),
            self.stall_reason.as_deref().unwrap_or("NULL"),
            self.last_error.as_deref().unwrap_or("NULL"),
        )
    }
}

/// One admitted ingress item: a host input or a queued-work batch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InputRow {
    /// `pending_turn_inputs` or `queued_work_batches`.
    pub table: String,
    pub session: String,
    pub id: String,
    /// The input's state; a queued batch's row has none of its own.
    pub state: Option<String>,
    pub admitted_root: Option<String>,
    pub obligation_state: Option<String>,
}

impl InputRow {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} {}/{}: state={} admitted_root={} obligation={}",
            self.table,
            self.session,
            self.id,
            self.state.as_deref().unwrap_or("-"),
            self.admitted_root.as_deref().unwrap_or("NULL"),
            self.obligation_state.as_deref().unwrap_or("NULL"),
        )
    }
}

/// One logical root of a session (`session_roots`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RootRow {
    pub session: String,
    pub root: String,
    pub admitted: bool,
    pub terminal_kind: Option<String>,
}

impl RootRow {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "session_roots {}/{}: admitted={} terminal={}",
            self.session,
            self.root,
            self.admitted,
            self.terminal_kind.as_deref().unwrap_or("NULL")
        )
    }
}

/// One stored artifact and the referrers that hold it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArtifactRow {
    pub namespace: String,
    pub artifact_ref: String,
    /// `(referrer_kind, referrer_id)` of every edge.
    pub referrers: Vec<(String, String)>,
}

/// One artifact cleanup owed by an ended referrer (ADR 0113 §2.5).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CleanupRow {
    pub referrer_kind: String,
    pub referrer_id: String,
    pub state: String,
}

/// One tool call part of a committed assistant message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TranscriptCall {
    /// The assistant message's index on the session's active path.
    pub message: usize,
    /// The part's index among that message's tool calls.
    pub index: usize,
    pub call_id: String,
    pub tool: String,
}

/// One committed tool result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TranscriptResult {
    pub call_id: String,
    pub tool: String,
    /// [`super::result_digest`] of the text the model reads.
    pub digest: String,
}

/// A session's committed transcript, as the checkers read it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TranscriptSession {
    pub session: String,
    pub calls: Vec<TranscriptCall>,
    pub results: Vec<TranscriptResult>,
}

/// One committed session graph node, as the frame-lineage checker reads it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphNodeRow {
    pub session: String,
    pub node_id: String,
    pub parent: Option<String>,
    /// The frame the store placed the node in.
    pub frame: String,
    /// Whether the node is a `FrameOpen`.
    pub frame_open: bool,
}

impl GraphNodeRow {
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "graph_nodes {}/{}: parent={} frame={} frame_open={}",
            self.session,
            self.node_id,
            self.parent.as_deref().unwrap_or("NULL"),
            self.frame,
            self.frame_open
        )
    }
}

/// One store set's final durable rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StoreSnapshot {
    pub label: String,
    pub obligations: Vec<ObligationRow>,
    pub inputs: Vec<InputRow>,
    pub roots: Vec<RootRow>,
    /// `(session, input, root)` of every `session_root_inputs` binding.
    pub root_inputs: Vec<(String, String, String)>,
    pub artifacts: Vec<ArtifactRow>,
    /// `(referrer_kind, referrer_id)` of every ended referrer's fence.
    pub fences: BTreeSet<(String, String)>,
    pub cleanups: Vec<CleanupRow>,
    pub transcripts: Vec<TranscriptSession>,
    /// Every committed graph node, in commit order.
    pub graph_nodes: Vec<GraphNodeRow>,
    /// `(session, leaf)` of every session head that has a leaf.
    pub heads: Vec<(String, String)>,
}

fn text(row: &RawRow, column: &str) -> Option<String> {
    row.iter()
        .find(|(name, _)| name == column)
        .and_then(|(_, value)| match value {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            other => Some(other.to_string()),
        })
}

fn required(row: &RawRow, column: &str) -> String {
    text(row, column).unwrap_or_default()
}

fn read(
    stores: &lash_sqlite_store::SqliteStoreSet,
    database: SqliteDatabase,
    sql: &str,
) -> Result<Vec<RawRow>, String> {
    read_rows_for_testing(stores, database, sql)
}

fn has_table(
    stores: &lash_sqlite_store::SqliteStoreSet,
    database: SqliteDatabase,
    table: &str,
) -> Result<bool, String> {
    Ok(!read(
        stores,
        database,
        &format!("SELECT name FROM sqlite_master WHERE type = 'table' AND name = '{table}'"),
    )?
    .is_empty())
}

impl StoreSnapshot {
    /// Read `stores`' final rows. Every table that carries an ADR 0109
    /// obligation column family is found from the schema, so a ledger added
    /// later is judged without a change here.
    pub fn read(
        label: impl Into<String>,
        stores: &lash_sqlite_store::SqliteStoreSet,
    ) -> Result<Self, String> {
        let mut snapshot = Self {
            label: label.into(),
            ..Self::default()
        };
        for database in DATABASES {
            snapshot.read_obligations(stores, database)?;
        }
        let core = SqliteDatabase::DurableCore;
        for row in read(
            stores,
            core,
            "SELECT session_id, input_id, state, admitted_root, obligation_state \
             FROM pending_turn_inputs ORDER BY session_id, enqueue_seq",
        )? {
            snapshot.inputs.push(InputRow {
                table: "pending_turn_inputs".to_owned(),
                session: required(&row, "session_id"),
                id: required(&row, "input_id"),
                state: text(&row, "state"),
                admitted_root: text(&row, "admitted_root"),
                obligation_state: text(&row, "obligation_state"),
            });
        }
        for row in read(
            stores,
            core,
            "SELECT session_id, batch_id, admitted_root, obligation_state \
             FROM queued_work_batches ORDER BY session_id, enqueue_seq",
        )? {
            snapshot.inputs.push(InputRow {
                table: "queued_work_batches".to_owned(),
                session: required(&row, "session_id"),
                id: required(&row, "batch_id"),
                state: None,
                admitted_root: text(&row, "admitted_root"),
                obligation_state: text(&row, "obligation_state"),
            });
        }
        for row in read(
            stores,
            core,
            "SELECT session_id, root, admission_json IS NOT NULL AS admitted, terminal_kind \
             FROM session_roots ORDER BY session_id, root",
        )? {
            snapshot.roots.push(RootRow {
                session: required(&row, "session_id"),
                root: required(&row, "root"),
                admitted: text(&row, "admitted").as_deref() == Some("1"),
                terminal_kind: text(&row, "terminal_kind"),
            });
        }
        for row in read(
            stores,
            core,
            "SELECT session_id, input_id, root FROM session_root_inputs \
             ORDER BY session_id, input_id",
        )? {
            snapshot.root_inputs.push((
                required(&row, "session_id"),
                required(&row, "input_id"),
                required(&row, "root"),
            ));
        }
        for row in read(
            stores,
            core,
            "SELECT r.namespace, r.artifact_ref, e.referrer_kind, e.referrer_id \
             FROM artifact_refs r LEFT JOIN artifact_referrer_edges e \
             ON e.namespace = r.namespace AND e.artifact_ref = r.artifact_ref \
             ORDER BY r.namespace, r.artifact_ref, e.referrer_kind, e.referrer_id",
        )? {
            let namespace = required(&row, "namespace");
            let artifact_ref = required(&row, "artifact_ref");
            let referrer = text(&row, "referrer_kind").zip(text(&row, "referrer_id"));
            match snapshot.artifacts.last_mut() {
                Some(last) if last.namespace == namespace && last.artifact_ref == artifact_ref => {
                    last.referrers.extend(referrer);
                }
                _ => snapshot.artifacts.push(ArtifactRow {
                    namespace,
                    artifact_ref,
                    referrers: referrer.into_iter().collect(),
                }),
            }
        }
        for row in read(
            stores,
            core,
            "SELECT session_id, node_id, parent_node_id, frame_node_id, node_json \
             FROM graph_nodes ORDER BY session_id, generation",
        )? {
            let node_id = required(&row, "node_id");
            let parent = text(&row, "parent_node_id");
            let record = lash_core::SessionNodeRecord::decode_storage_body(
                node_id.clone(),
                parent.clone(),
                &required(&row, "node_json"),
            )
            .map_err(|error| format!("decode graph node {node_id}: {error}"))?;
            snapshot.graph_nodes.push(GraphNodeRow {
                session: required(&row, "session_id"),
                node_id,
                parent,
                frame: required(&row, "frame_node_id"),
                frame_open: matches!(
                    record.payload,
                    lash_core::SessionNodePayload::FrameOpen { .. }
                ),
            });
        }
        for row in read(
            stores,
            core,
            "SELECT session_id, leaf_node_id FROM session_head \
             WHERE leaf_node_id IS NOT NULL ORDER BY session_id",
        )? {
            snapshot
                .heads
                .push((required(&row, "session_id"), required(&row, "leaf_node_id")));
        }
        for row in read(
            stores,
            core,
            "SELECT referrer_kind, referrer_id FROM artifact_referrer_fences",
        )? {
            snapshot.fences.insert((
                required(&row, "referrer_kind"),
                required(&row, "referrer_id"),
            ));
        }
        if has_table(stores, core, "artifact_cleanup_obligations")? {
            for row in read(
                stores,
                core,
                "SELECT referrer_kind, referrer_id, obligation_state \
                 FROM artifact_cleanup_obligations ORDER BY referrer_kind, referrer_id",
            )? {
                snapshot.cleanups.push(CleanupRow {
                    referrer_kind: required(&row, "referrer_kind"),
                    referrer_id: required(&row, "referrer_id"),
                    state: required(&row, "obligation_state"),
                });
            }
        }
        Ok(snapshot)
    }

    fn read_obligations(
        &mut self,
        stores: &lash_sqlite_store::SqliteStoreSet,
        database: SqliteDatabase,
    ) -> Result<(), String> {
        let tables = read(
            stores,
            database,
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        )?;
        for table in tables.iter().map(|row| required(row, "name")) {
            let columns = read(
                stores,
                database,
                &format!("SELECT name, pk FROM pragma_table_info('{table}') ORDER BY cid"),
            )?;
            let mut key = columns
                .iter()
                .filter_map(|row| {
                    let pk = text(row, "pk")?.parse::<u32>().ok().filter(|pk| *pk > 0)?;
                    Some((pk, required(row, "name")))
                })
                .collect::<Vec<_>>();
            key.sort();
            let key = if key.is_empty() {
                "rowid".to_owned()
            } else {
                key.into_iter()
                    .map(|(_, name)| name)
                    .collect::<Vec<_>>()
                    .join(" || '/' || ")
            };
            let families = columns
                .iter()
                .filter_map(|row| {
                    required(row, "name")
                        .strip_suffix("obligation_state")
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>();
            for family in families {
                for row in read(
                    stores,
                    database,
                    &format!(
                        "SELECT {key} AS row_key, {family}obligation_id AS id, \
                         {family}obligation_state AS state, \
                         {family}obligation_due_at_ms AS due_at, \
                         {family}obligation_stall_reason AS stall_reason, \
                         {family}obligation_last_error AS last_error \
                         FROM {table} ORDER BY row_key"
                    ),
                )? {
                    self.obligations.push(ObligationRow {
                        table: table.clone(),
                        family: family.clone(),
                        key: required(&row, "row_key"),
                        id: text(&row, "id"),
                        state: text(&row, "state"),
                        due_at_ms: text(&row, "due_at").and_then(|due| due.parse().ok()),
                        stall_reason: text(&row, "stall_reason"),
                        last_error: text(&row, "last_error"),
                    });
                }
            }
        }
        Ok(())
    }

    /// Read every session's committed transcript back through `stores`'
    /// session factory. A session the factory cannot reopen as a root (a
    /// child or process-owned session) has no transcript here.
    pub async fn read_transcripts(
        &mut self,
        stores: &lash_sqlite_store::SqliteStoreSet,
    ) -> Result<(), String> {
        let factory = stores.session_store_factory();
        let sessions = read(
            stores,
            SqliteDatabase::DurableCore,
            "SELECT session_id FROM session_meta WHERE relation_kind = 'root' \
             ORDER BY session_id",
        )?;
        for session in sessions.iter().map(|row| required(row, "session_id")) {
            let Ok(Some(reopened)) =
                crate::content_oracle::reopen_session(factory.as_ref(), &session).await
            else {
                continue;
            };
            let mut transcript = TranscriptSession {
                session: session.clone(),
                ..TranscriptSession::default()
            };
            for (message, committed) in reopened.assistant_messages.iter().enumerate() {
                for (index, call) in committed.tool_calls.iter().enumerate() {
                    transcript.calls.push(TranscriptCall {
                        message,
                        index,
                        call_id: call.call_id.clone(),
                        tool: call.tool_name.clone(),
                    });
                }
            }
            for result in &reopened.tool_results {
                transcript.results.push(TranscriptResult {
                    call_id: result.call_id.clone(),
                    tool: result.tool_name.clone(),
                    digest: super::result_digest(&result.content),
                });
            }
            self.transcripts.push(transcript);
        }
        Ok(())
    }

    /// The transcript of `session`, when this store holds one.
    #[must_use]
    pub fn transcript(&self, session: &str) -> Option<&TranscriptSession> {
        self.transcripts
            .iter()
            .find(|transcript| transcript.session == session)
    }
}
