use super::*;
use sqlx::Row as _;

type PendingTurnInputRow = (String, String, Option<String>, Option<String>);

/// One `session_roots` row's terminal and obligation columns, as each
/// backend's durable read hands them to [`scope_close_obligations`].
type ScopeCloseObligationRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
);

/// The `session_roots` obligation projection, with ADR 0109 §1.8's
/// detection bound asserted per row as the durable read returns it:
///
/// - terminal evidence and the `ScopeClose` obligation arm together — a
///   `terminal_kind` with no `obligation_id`, or an obligation id that is
///   not the row's derived [`scope_close_obligation_id`], is a producer bug;
/// - an undelivered obligation is always on the due index and its schedule
///   is bounded — `obligation_due_at_ms` is never further out than one
///   claim TTL (`claimed`) or one maximum backoff (`due`) from the read, so
///   the next due pass finds it inside the tick bound rather than any pass
///   rescanning terminal roots;
/// - a settled row stamps when it settled.
///
/// `now_ms` is the clock the backend stamped these rows with: the harness's
/// injected clock, so the bound is asserted in the same time base the
/// obligations were armed under.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn scope_close_obligations(
    session_id: &SessionId,
    now_ms: u64,
    rows: Vec<ScopeCloseObligationRow>,
) -> Vec<ScopeCloseObligationObservation> {
    let policy = lash_core::runtime::drive::relay::RelayPolicy::default();
    rows.into_iter()
        .map(
            |(
                root,
                terminal_kind,
                terminal_at_ms,
                obligation_id,
                obligation_state,
                obligation_attempts,
                obligation_due_at_ms,
                obligation_stall_reason,
                obligation_settled_at_ms,
            )| {
                assert_eq!(
                    terminal_kind.is_some(),
                    obligation_id.is_some(),
                    "root `{root}`: terminal evidence and its scope-close obligation arm \
                     together (ADR 0109 §3)"
                );
                if let Some(id) = &obligation_id {
                    let derived = lash_core::store::scope_close_obligation_id(
                        session_id,
                        &lash_core::TurnId::from(root.as_str()),
                    );
                    assert_eq!(
                        id.as_str(),
                        derived.as_str(),
                        "root `{root}`: the armed obligation carries the row's derived id"
                    );
                }
                match obligation_state.as_deref() {
                    None => {}
                    Some("due") | Some("claimed") => {
                        let horizon = if obligation_state.as_deref() == Some("claimed") {
                            policy.claim_ttl_ms
                        } else {
                            policy.max_backoff_ms
                        };
                        let due_at_ms = obligation_due_at_ms
                            .expect("an undelivered obligation is on the due index")
                            as u64;
                        assert!(
                            due_at_ms <= now_ms + horizon,
                            "root `{root}`: obligation due at {due_at_ms} is beyond the \
                             §1.8 detection horizon {horizon} ms past {now_ms}"
                        );
                    }
                    Some("delivered") | Some("stalled") => {
                        assert!(
                            obligation_settled_at_ms.is_some(),
                            "root `{root}`: a settled obligation stamps when it settled"
                        );
                    }
                    Some(state) => panic!("root `{root}`: unknown obligation state `{state}`"),
                }
                ScopeCloseObligationObservation {
                    root,
                    terminal_kind,
                    terminal_at_ms: terminal_at_ms.map(|at_ms| at_ms as u64),
                    obligation_id,
                    obligation_state,
                    obligation_attempts: obligation_attempts as u64,
                    obligation_due_at_ms: obligation_due_at_ms.map(|at_ms| at_ms as u64),
                    obligation_stall_reason,
                    obligation_settled_at_ms: obligation_settled_at_ms.map(|at_ms| at_ms as u64),
                }
            },
        )
        .collect()
}

impl RawDurableReader {
    pub(super) fn detach_store(&mut self) {
        match self {
            Self::Sqlite { store, .. } | Self::Postgres { store, .. } => {
                store.take();
            }
        }
    }

    /// See `residue.rs` for why this is separate from [`RawDurableReader::observe`].
    pub(super) async fn residue_digest(&self) -> ResidueDigest {
        match self {
            Self::Sqlite {
                path, session_id, ..
            } => sqlite_residue_digest(path, session_id),
            Self::Postgres {
                pool, session_id, ..
            } => postgres_residue_digest(pool, session_id).await,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    /// `now_ms` is the caller's clock in the same time base the backend
    /// stamped its rows with; the scope-close §1.8 bound is asserted
    /// against it.
    pub(super) async fn observe(&self, now_ms: u64) -> RawDurableState {
        match self {
            Self::Sqlite {
                path,
                session_id,
                store,
            } => {
                read_sqlite_durable_state(
                    path,
                    session_id,
                    store
                        .as_ref()
                        .expect("SQLite reader is attached to a store"),
                    now_ms,
                )
                .await
            }
            Self::Postgres {
                pool,
                session_id,
                store,
            } => {
                let store = store
                    .as_ref()
                    .expect("Postgres reader is attached to a store");
                let head: Option<(i64, Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT head_revision, leaf_node_id, checkpoint_ref
                     FROM lash_sessions
                     WHERE session_id = $1",
                )
                .bind(session_id.as_str())
                .fetch_optional(pool)
                .await
                .expect("read Postgres durable head");
                let (head_revision, leaf_node_id, checkpoint_ref) = head.map_or(
                    (None, None, None),
                    |(revision, leaf_node_id, checkpoint_ref)| {
                        (
                            Some(revision as u64),
                            leaf_node_id,
                            checkpoint_ref.map(BlobRef),
                        )
                    },
                );
                let checkpoint = read_postgres_checkpoint_observation(pool, checkpoint_ref).await;
                let rows: Vec<(i64, String, Option<String>, String)> = sqlx::query_as(
                    "SELECT generation, node_id, parent_node_id, node_json
                     FROM lash_graph_nodes
                     WHERE session_id = $1 AND tombstoned = FALSE
                     ORDER BY generation ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres durable nodes");
                let durable_nodes = rows
                    .into_iter()
                    .enumerate()
                    .map(
                        |(ordinal, (_generation, node_id, parent_node_id, node_json))| {
                            DurableNode {
                                ordinal,
                                node_id,
                                parent_node_id,
                                bytes: normalized_sql_node_json(&node_json),
                            }
                        },
                    )
                    .collect();
                let receipt_rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT turn_id, turn_commit_hash, result_json
                     FROM lash_runtime_turn_commits
                     WHERE session_id = $1
                     ORDER BY turn_id ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres turn-commit receipts");
                let runtime_turn_commits = receipt_rows
                    .into_iter()
                    .map(|(operation, turn_commit_hash, result_json)| {
                        RuntimeTurnCommitObservation {
                            operation,
                            turn_commit_hash,
                            result: serde_json::from_str(&result_json)
                                .expect("decode Postgres turn-commit result"),
                        }
                    })
                    .collect();
                let attachment_rows: Vec<AttachmentRow> = sqlx::query_as(
                    "SELECT attachment_id, canonical_uri, intent_at_ms, written_at_ms,
                            committed_at_ms, owner_kind, owner_id
                     FROM lash_attachment_manifest
                     WHERE session_id = $1
                     ORDER BY attachment_id ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres attachment manifest");
                let attachment_manifest = attachment_rows
                    .into_iter()
                    .map(
                        |(
                            attachment_id,
                            canonical_uri,
                            intent_at_epoch_ms,
                            written_at_epoch_ms,
                            committed_at_epoch_ms,
                            owner_kind,
                            owner_id,
                        )| AttachmentManifestObservation {
                            attachment_id: AttachmentId::parse(attachment_id)
                                .expect("valid attachment id"),
                            canonical_uri,
                            intent_at_epoch_ms: intent_at_epoch_ms as u64,
                            written: written_at_epoch_ms.is_some(),
                            committed: committed_at_epoch_ms.is_some(),
                            owner_kind: decode_attachment_owner_kind(owner_kind.as_deref()),
                            owner_id,
                        },
                    )
                    .collect();
                let anchor_rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT node_id, checkpoint_ref, source_session_id
                     FROM lash_node_anchors
                     WHERE source_session_id = $1
                     ORDER BY node_id ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres node anchors");
                let node_anchors = anchor_rows
                    .into_iter()
                    .map(
                        |(node_id, checkpoint_ref, source_session_id)| NodeAnchorObservation {
                            node_id,
                            checkpoint_ref: BlobRef(checkpoint_ref),
                            source_session_id: SessionId::from(source_session_id),
                        },
                    )
                    .collect();
                let usage_rows = sqlx::query(
                    "SELECT source, model, input_tokens, output_tokens,
                                cache_read_input_tokens, cache_write_input_tokens,
                                reasoning_output_tokens
                         FROM lash_usage_deltas
                         WHERE session_id = $1
                         ORDER BY seq ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres usage deltas");
                let usage_deltas = usage_rows
                    .into_iter()
                    .map(|row| {
                        usage_delta_observation(TokenLedgerEntry {
                            source: row.get(0),
                            model: row.get(1),
                            usage: TokenUsage {
                                input_tokens: row.get(2),
                                output_tokens: row.get(3),
                                cache_read_input_tokens: row.get(4),
                                cache_write_input_tokens: row.get(5),
                                reasoning_output_tokens: row.get(6),
                            },
                            usage_disposition: Default::default(),
                        })
                    })
                    .collect();
                let session_meta = store
                    .load_session_meta(session_id)
                    .await
                    .expect("read PostgreSQL session metadata")
                    .map(session_meta_observation);
                let pending_rows: Vec<PendingTurnInputRow> = sqlx::query_as(
                    "SELECT input_id, state, admitted_root, admitted_by
                     FROM lash_pending_turn_inputs
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres pending turn inputs");
                let pending_turn_inputs = pending_rows
                    .into_iter()
                    .map(|(input_id, state, admitted_root, admitted_by)| {
                        PendingTurnInputObservation {
                            input_id,
                            state: TurnInputStateKind::from_wire_str(&state)
                                .expect("decode Postgres pending-input state"),
                            admitted_root,
                            admitted_by,
                        }
                    })
                    .collect();
                let queued_work_batches: Vec<QueuedWorkBatchRow> = sqlx::query_as(
                    "SELECT enqueue_seq, batch_id, source_key, delivery_policy, work_kind,
                            authority_json, merge_key, admitted_root, admitted_by
                     FROM lash_queued_work_batches
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres queued-work batches");
                let queued_work_items: Vec<QueuedWorkItemRow> = sqlx::query_as(
                    "SELECT item.batch_id, item.item_index::BIGINT, item.payload_json
                     FROM lash_queued_work_items AS item
                     JOIN lash_queued_work_batches AS batch
                       ON batch.batch_id = item.batch_id
                     WHERE batch.session_id = $1
                     ORDER BY batch.enqueue_seq ASC, item.item_index ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres queued-work items");
                let queued_work =
                    queued_work_observations_from_sql_rows(queued_work_batches, queued_work_items);
                let obligation_rows: Vec<ScopeCloseObligationRow> = sqlx::query_as(
                    "SELECT root, terminal_kind, terminal_at_ms, obligation_id,
                            obligation_state, obligation_attempts::BIGINT, obligation_due_at_ms,
                            obligation_stall_reason, obligation_settled_at_ms
                     FROM lash_session_roots
                     WHERE session_id = $1
                       AND (terminal_kind IS NOT NULL OR obligation_state IS NOT NULL)
                     ORDER BY root ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres scope-close obligations");
                let scope_close_obligations =
                    scope_close_obligations(session_id, now_ms, obligation_rows);
                RawDurableState {
                    head_revision,
                    leaf_node_id,
                    checkpoint,
                    durable_nodes,
                    runtime_turn_commits,
                    attachment_manifest,
                    node_anchors,
                    usage_deltas,
                    session_meta,
                    pending_turn_inputs,
                    queued_work,
                    scope_close_obligations,
                }
            }
        }
    }
}

/// Decode the full SQLite durable surface into the normalized, cross-backend
/// comparable `RawDurableState`. Lives here rather than in the harness root so
/// the root stays inside the repository's test-file line budget.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_sqlite_durable_state(
    path: &Path,
    session_id: &SessionId,
    store: &Arc<dyn RuntimeStore>,
    now_ms: u64,
) -> RawDurableState {
    let connection = rusqlite::Connection::open(path).expect("open SQLite durable reader");
    connection
        .busy_timeout(Duration::from_secs(15))
        .expect("configure SQLite durable reader busy timeout");
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .expect("configure SQLite durable reader WAL mode");
    connection
        .execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")
        .expect("configure SQLite durable reader pragmas");

    let head: Option<(i64, Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT head_revision, leaf_node_id, checkpoint_ref
             FROM session_head
             WHERE session_id = ?1",
            [session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .expect("read SQLite durable head");
    let (head_revision, leaf_node_id, checkpoint_ref) = head.map_or(
        (None, None, None),
        |(revision, leaf_node_id, checkpoint_ref)| {
            (
                Some(revision as u64),
                leaf_node_id,
                checkpoint_ref.map(BlobRef),
            )
        },
    );
    let checkpoint = read_sqlite_checkpoint_observation(path, checkpoint_ref);
    let durable_nodes = {
        let mut statement = connection
            .prepare(
                "SELECT generation, node_id, parent_node_id, node_json
                 FROM graph_nodes
                 WHERE session_id = ?1 AND tombstoned = 0
                 ORDER BY generation ASC",
            )
            .expect("prepare SQLite durable node read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .expect("read SQLite durable nodes")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite durable nodes")
            .into_iter()
            .enumerate()
            .map(
                |(ordinal, (_generation, node_id, parent_node_id, node_json))| DurableNode {
                    ordinal,
                    node_id,
                    parent_node_id,
                    bytes: normalized_sql_node_json(&node_json),
                },
            )
            .collect()
    };
    let runtime_turn_commits = {
        let mut statement = connection
            .prepare(
                "SELECT turn_id, turn_commit_hash, result_json
                 FROM runtime_turn_commits
                 WHERE session_id = ?1
                 ORDER BY turn_id ASC",
            )
            .expect("prepare SQLite turn-commit receipt read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("read SQLite turn-commit receipts")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite turn-commit receipts")
            .into_iter()
            .map(
                |(operation, turn_commit_hash, result_json)| RuntimeTurnCommitObservation {
                    operation,
                    turn_commit_hash,
                    result: serde_json::from_str(&result_json)
                        .expect("decode SQLite turn-commit result"),
                },
            )
            .collect()
    };
    let attachment_manifest = {
        let mut statement = connection
            .prepare(
                "SELECT attachment_id, canonical_uri, intent_at_ms, written_at_ms,
                        committed_at_ms, owner_kind, owner_id
                 FROM attachment_manifest
                 WHERE session_id = ?1
                 ORDER BY attachment_id ASC",
            )
            .expect("prepare SQLite attachment-manifest read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .expect("read SQLite attachment manifest")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite attachment manifest")
            .into_iter()
            .map(
                |(
                    attachment_id,
                    canonical_uri,
                    intent_at_epoch_ms,
                    written_at_epoch_ms,
                    committed_at_epoch_ms,
                    owner_kind,
                    owner_id,
                )| AttachmentManifestObservation {
                    attachment_id: AttachmentId::parse(attachment_id).expect("valid attachment id"),
                    canonical_uri,
                    intent_at_epoch_ms: intent_at_epoch_ms as u64,
                    written: written_at_epoch_ms.is_some(),
                    committed: committed_at_epoch_ms.is_some(),
                    owner_kind: decode_attachment_owner_kind(owner_kind.as_deref()),
                    owner_id,
                },
            )
            .collect()
    };
    let node_anchors = {
        let mut statement = connection
            .prepare(
                "SELECT node_id, checkpoint_ref, source_session_id
                 FROM node_anchors
                 WHERE source_session_id = ?1
                 ORDER BY node_id ASC",
            )
            .expect("prepare SQLite node-anchor read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok(NodeAnchorObservation {
                    node_id: row.get(0)?,
                    checkpoint_ref: BlobRef(row.get(1)?),
                    source_session_id: SessionId::from(row.get::<_, String>(2)?),
                })
            })
            .expect("read SQLite node anchors")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite node anchors")
    };
    let usage_deltas = {
        let mut statement = connection
            .prepare(
                "SELECT source, model, input_tokens, output_tokens,
                        cache_read_input_tokens, cache_write_input_tokens,
                        reasoning_output_tokens
                 FROM usage_deltas
                 WHERE session_id = ?1
                 ORDER BY seq ASC",
            )
            .expect("prepare SQLite usage-delta read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok(UsageDeltaObservation {
                    source: row.get(0)?,
                    model: row.get(1)?,
                    usage: TokenUsage {
                        input_tokens: row.get(2)?,
                        output_tokens: row.get(3)?,
                        cache_read_input_tokens: row.get(4)?,
                        cache_write_input_tokens: row.get(5)?,
                        reasoning_output_tokens: row.get(6)?,
                    },
                })
            })
            .expect("read SQLite usage deltas")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite usage deltas")
    };
    let session_meta = store
        .load_session_meta(session_id)
        .await
        .expect("read SQLite session metadata")
        .map(session_meta_observation);
    let pending_turn_inputs = {
        let mut statement = connection
            .prepare(
                "SELECT input_id, state, admitted_root, admitted_by
                 FROM pending_turn_inputs
                 WHERE session_id = ?1
                 ORDER BY enqueue_seq ASC",
            )
            .expect("prepare SQLite pending-input read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .expect("read SQLite pending turn inputs")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite pending turn inputs")
            .into_iter()
            .map(
                |(input_id, state, admitted_root, admitted_by)| PendingTurnInputObservation {
                    input_id,
                    state: TurnInputStateKind::from_wire_str(&state)
                        .expect("decode SQLite pending-input state"),
                    admitted_root,
                    admitted_by,
                },
            )
            .collect()
    };
    let queued_work_batches = {
        let mut statement = connection
            .prepare(
                "SELECT enqueue_seq, batch_id, source_key, delivery_policy, work_kind,
                        authority_json, merge_key, admitted_root, admitted_by
                 FROM queued_work_batches
                 WHERE session_id = ?1
                 ORDER BY enqueue_seq ASC",
            )
            .expect("prepare SQLite queued-work batch read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })
            .expect("read SQLite queued-work batches")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite queued-work batches")
    };
    let queued_work_items = {
        let mut statement = connection
            .prepare(
                "SELECT item.batch_id, item.item_index, item.payload_json
                 FROM queued_work_items AS item
                 JOIN queued_work_batches AS batch ON batch.batch_id = item.batch_id
                 WHERE batch.session_id = ?1
                 ORDER BY batch.enqueue_seq ASC, item.item_index ASC",
            )
            .expect("prepare SQLite queued-work item read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("read SQLite queued-work items")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite queued-work items")
    };
    let queued_work =
        queued_work_observations_from_sql_rows(queued_work_batches, queued_work_items);
    let obligation_rows: Vec<ScopeCloseObligationRow> = {
        let mut statement = connection
            .prepare(
                "SELECT root, terminal_kind, terminal_at_ms, obligation_id,
                        obligation_state, obligation_attempts, obligation_due_at_ms,
                        obligation_stall_reason, obligation_settled_at_ms
                 FROM session_roots
                 WHERE session_id = ?1
                   AND (terminal_kind IS NOT NULL OR obligation_state IS NOT NULL)
                 ORDER BY root ASC",
            )
            .expect("prepare SQLite scope-close obligation read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })
            .expect("read SQLite scope-close obligations")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite scope-close obligations")
    };
    let scope_close_obligations = scope_close_obligations(session_id, now_ms, obligation_rows);
    RawDurableState {
        head_revision,
        leaf_node_id,
        checkpoint,
        durable_nodes,
        runtime_turn_commits,
        attachment_manifest,
        node_anchors,
        usage_deltas,
        session_meta,
        pending_turn_inputs,
        queued_work,
        scope_close_obligations,
    }
}
