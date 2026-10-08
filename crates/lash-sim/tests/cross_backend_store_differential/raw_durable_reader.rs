use super::*;

type PendingTurnInputRow = (String, String, Option<String>, Option<String>);

/// One `session_runs` row's terminal evidence, as each backend's durable
/// read hands it back.
type RunTerminalRow = (String, Option<String>, Option<i64>);

fn run_terminals(rows: Vec<RunTerminalRow>) -> Vec<RunTerminalObservation> {
    rows.into_iter()
        .map(
            |(run, terminal_kind, terminal_at_ms)| RunTerminalObservation {
                run,
                terminal_kind,
                terminal_at_ms: terminal_at_ms.map(|at_ms| at_ms as u64),
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
    pub(super) async fn observe(&self) -> RawDurableState {
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
                     FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision)
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
                let attachment_rows: Vec<AttachmentRow> = sqlx::query_as("SELECT r.attachment_id, r.referrer_kind, r.referrer_id,
 EXISTS(SELECT 1 FROM lash_attachment_referrer_edges e WHERE e.attachment_id = r.attachment_id AND e.referrer_kind = r.referrer_kind AND e.referrer_id = r.referrer_id),
 EXISTS(SELECT 1 FROM lash_attachment_uploads u WHERE u.attachment_id = r.attachment_id),
 (SELECT COUNT(*) FROM lash_attachment_pending_writes p WHERE p.attachment_id = r.attachment_id AND p.referrer_kind = r.referrer_kind AND p.referrer_id = r.referrer_id)
 FROM (SELECT attachment_id, referrer_kind, referrer_id FROM lash_attachment_referrer_edges
 UNION SELECT attachment_id, referrer_kind, referrer_id FROM lash_attachment_pending_writes) r
 WHERE (r.referrer_kind = 'session' AND r.referrer_id = $1)
 OR (r.referrer_kind = 'process_record' AND r.referrer_id = $2)
 ORDER BY r.attachment_id, r.referrer_kind, r.referrer_id")
                .bind(session_id.as_str())
                .bind(lash_sansio::ProcessId::fixture(session_id.as_str()).as_str())
                .fetch_all(pool).await.expect("read attachment roots");
                let attachment_referrers = attachment_rows
                    .into_iter()
                    .map(
                        |(id, referrer_kind, referrer_id, edge, written, pending_writes)| {
                            AttachmentReferrerObservation {
                                attachment_id: AttachmentId::parse(id).expect("digest"),
                                referrer_kind,
                                referrer_id,
                                edge,
                                written,
                                pending_writes,
                            }
                        },
                    )
                    .collect();
                let revision_rows: Vec<(i64, Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT head_revision, leaf_node_id, checkpoint_ref
                     FROM lash_session_revisions
                     WHERE session_id = $1
                     ORDER BY head_revision ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres session revisions");
                let revisions = revision_rows
                    .into_iter()
                    .map(
                        |(head_revision, leaf_node_id, checkpoint_ref)| RevisionObservation {
                            head_revision,
                            leaf_node_id,
                            checkpoint_ref: checkpoint_ref.map(BlobRef),
                        },
                    )
                    .collect();
                let pin_rows: Vec<(String, String)> = sqlx::query_as(
                    "SELECT target_kind, target_id
                     FROM lash_pins
                     WHERE session_id = $1
                     ORDER BY target_kind ASC, target_id ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres pins");
                let pins = pin_rows
                    .into_iter()
                    .map(|(target_kind, target_id)| PinObservation {
                        target_kind,
                        target_id,
                    })
                    .collect();
                let session_meta = store
                    .load_session_meta(session_id)
                    .await
                    .expect("read PostgreSQL session metadata")
                    .map(session_meta_observation);
                let pending_rows: Vec<PendingTurnInputRow> = sqlx::query_as(
                    "SELECT input_id, state, admitted_run, admitted_by
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
                    .map(|(input_id, state, admitted_run, admitted_by)| {
                        PendingTurnInputObservation {
                            input_id,
                            state: TurnInputStateKind::from_wire_str(&state)
                                .expect("decode Postgres pending-input state"),
                            admitted_run,
                            admitted_by,
                        }
                    })
                    .collect();
                let queued_work_batches: Vec<QueuedWorkBatchRow> = sqlx::query_as(
                    "SELECT enqueue_seq, batch_id, source_key, delivery_policy,
                            authority_json, merge_key, admitted_run, admitted_by, payload_json
                     FROM lash_queued_work_batches
                     WHERE session_id = $1
                     ORDER BY enqueue_seq ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres queued-work batches");
                let queued_work = queued_work_observations_from_sql_rows(queued_work_batches);
                let terminal_rows: Vec<RunTerminalRow> = sqlx::query_as(
                    "SELECT run, terminal_kind, terminal_at_ms
                     FROM lash_session_runs
                     WHERE session_id = $1 AND terminal_kind IS NOT NULL
                     ORDER BY run ASC",
                )
                .bind(session_id.as_str())
                .fetch_all(pool)
                .await
                .expect("read Postgres run terminals");
                let run_terminals = run_terminals(terminal_rows);
                RawDurableState {
                    head_revision,
                    leaf_node_id,
                    checkpoint,
                    durable_nodes,
                    runtime_turn_commits,
                    attachment_referrers,
                    revisions,
                    pins,
                    session_meta,
                    pending_turn_inputs,
                    queued_work,
                    run_terminals,
                }
            }
        }
    }
}

/// Decode the full SQLite durable surface into the normalized, cross-backend
/// comparable `RawDurableState`. Lives here rather than in the harness run so
/// the run stays inside the repository's test-file line budget.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn read_sqlite_durable_state(
    path: &Path,
    session_id: &SessionId,
    store: &Arc<dyn RuntimeStore>,
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
             FROM session_head JOIN session_revisions USING (session_id, head_revision)
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
    let attachment_referrers = {
        let mut statement = connection.prepare("SELECT r.attachment_id, r.referrer_kind, r.referrer_id,
 EXISTS(SELECT 1 FROM attachment_referrer_edges e WHERE e.attachment_id = r.attachment_id AND e.referrer_kind = r.referrer_kind AND e.referrer_id = r.referrer_id),
 EXISTS(SELECT 1 FROM attachment_uploads u WHERE u.attachment_id = r.attachment_id),
 (SELECT COUNT(*) FROM attachment_pending_writes p WHERE p.attachment_id = r.attachment_id AND p.referrer_kind = r.referrer_kind AND p.referrer_id = r.referrer_id)
 FROM (SELECT attachment_id, referrer_kind, referrer_id FROM attachment_referrer_edges
 UNION SELECT attachment_id, referrer_kind, referrer_id FROM attachment_pending_writes) r
 WHERE (r.referrer_kind = 'session' AND r.referrer_id = ?1)
 OR (r.referrer_kind = 'process_record' AND r.referrer_id = ?2)
 ORDER BY r.attachment_id, r.referrer_kind, r.referrer_id").expect("prepare attachment roots");
        statement
            .query_map(
                rusqlite::params![
                    session_id.as_str(),
                    lash_sansio::ProcessId::fixture(session_id.as_str()).as_str()
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .expect("read attachment roots")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode runs")
            .into_iter()
            .map(
                |(id, referrer_kind, referrer_id, edge, written, pending_writes)| {
                    AttachmentReferrerObservation {
                        attachment_id: AttachmentId::parse(id).expect("digest"),
                        referrer_kind,
                        referrer_id,
                        edge,
                        written,
                        pending_writes,
                    }
                },
            )
            .collect()
    };
    let revisions = {
        let mut statement = connection
            .prepare(
                "SELECT head_revision, leaf_node_id, checkpoint_ref
                 FROM session_revisions
                 WHERE session_id = ?1
                 ORDER BY head_revision ASC",
            )
            .expect("prepare SQLite session-revision read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok(RevisionObservation {
                    head_revision: row.get(0)?,
                    leaf_node_id: row.get(1)?,
                    checkpoint_ref: row.get::<_, Option<String>>(2)?.map(BlobRef),
                })
            })
            .expect("read SQLite session revisions")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite session revisions")
    };
    let pins = {
        let mut statement = connection
            .prepare(
                "SELECT target_kind, target_id
                 FROM pins
                 WHERE session_id = ?1
                 ORDER BY target_kind ASC, target_id ASC",
            )
            .expect("prepare SQLite pin read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok(PinObservation {
                    target_kind: row.get(0)?,
                    target_id: row.get(1)?,
                })
            })
            .expect("read SQLite pins")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite pins")
    };
    let session_meta = store
        .load_session_meta(session_id)
        .await
        .expect("read SQLite session metadata")
        .map(session_meta_observation);
    let pending_turn_inputs = {
        let mut statement = connection
            .prepare(
                "SELECT input_id, state, admitted_run, admitted_by
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
                |(input_id, state, admitted_run, admitted_by)| PendingTurnInputObservation {
                    input_id,
                    state: TurnInputStateKind::from_wire_str(&state)
                        .expect("decode SQLite pending-input state"),
                    admitted_run,
                    admitted_by,
                },
            )
            .collect()
    };
    let queued_work_batches = {
        let mut statement = connection
            .prepare(
                "SELECT enqueue_seq, batch_id, source_key, delivery_policy,
                        authority_json, merge_key, admitted_run, admitted_by, payload_json
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
    let queued_work = queued_work_observations_from_sql_rows(queued_work_batches);
    let terminal_rows: Vec<RunTerminalRow> = {
        let mut statement = connection
            .prepare(
                "SELECT run, terminal_kind, terminal_at_ms
                 FROM session_runs
                 WHERE session_id = ?1 AND terminal_kind IS NOT NULL
                 ORDER BY run ASC",
            )
            .expect("prepare SQLite run terminal read");
        statement
            .query_map([session_id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("read SQLite run terminals")
            .collect::<Result<Vec<_>, _>>()
            .expect("decode SQLite run terminals")
    };
    let run_terminals = run_terminals(terminal_rows);
    RawDurableState {
        head_revision,
        leaf_node_id,
        checkpoint,
        durable_nodes,
        runtime_turn_commits,
        attachment_referrers,
        revisions,
        pins,
        session_meta,
        pending_turn_inputs,
        queued_work,
        run_terminals,
    }
}
