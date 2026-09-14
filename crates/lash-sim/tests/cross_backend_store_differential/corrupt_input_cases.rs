//! Corrupt-input refusal cases for the FIG-2841 durability law.
//!
//! These are the two shapes the motivating defects needed: an undecodable
//! prior record on a read-modify-write path (FIG-2836's corrupt prior
//! checkpoint manifest, read inside the commit that replaces it), and a
//! corrupt row on every list path (FIG-2838's class disagreement between
//! PostgreSQL and SQLite over one undecodable persisted record).
//!
//! **Why the in-memory backend is not compared here.** The reference store
//! holds typed records, never encoded bytes, so an undecodable persisted
//! record is not a state it can reach — the same reason
//! `lash-conformance`'s in-memory registrations omit
//! `append_receipt_identity_corruption_tests!`. Excluding it is a structural
//! fact about the backend, not a comparison weakened to make it agree: the
//! two backends that *can* hold the bytes are still held to identical error
//! classes and identical residue. The refusal cases, which every backend can
//! reach, are compared across all three.
//!
//! **Why these cases compare raw residue instead of the decoded digest.**
//! [`RawDurableState`] decodes every row it reads, so it cannot observe a row
//! that is deliberately undecodable. These cases therefore compare the error
//! class, the mutated-or-not verdict, and the exact set of durable tables that
//! moved, all read without decoding.

use super::*;

/// A persisted record this harness can make undecodable on a SQL backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CorruptTarget {
    /// `graph_nodes.node_json` for the session's root node.
    GraphNodeJson,
    /// `pending_turn_inputs.input_json`.
    PendingTurnInputJson,
    /// `queued_work_items.payload_json`.
    QueuedWorkItemPayloadJson,
    /// The bytes of the checkpoint manifest blob the session head names.
    CheckpointManifestBlob,
}

impl CorruptTarget {
    pub(super) fn seed_label(self) -> &'static str {
        match self {
            Self::GraphNodeJson => "seed_corrupt_graph_node_json",
            Self::PendingTurnInputJson => "seed_corrupt_pending_turn_input_json",
            Self::QueuedWorkItemPayloadJson => "seed_corrupt_queued_work_item_payload_json",
            Self::CheckpointManifestBlob => "seed_corrupt_checkpoint_manifest_blob",
        }
    }

    pub(super) fn restore_label(self) -> &'static str {
        match self {
            Self::GraphNodeJson => "restore_graph_node_json",
            Self::PendingTurnInputJson => "restore_pending_turn_input_json",
            Self::QueuedWorkItemPayloadJson => "restore_queued_work_item_payload_json",
            Self::CheckpointManifestBlob => "restore_checkpoint_manifest_blob",
        }
    }
}

/// Bytes that are not a valid encoding of any persisted record.
const CORRUPT_TEXT: &str = "{\"fig-2841\": not-json";
const CORRUPT_BYTES: &[u8] = &[0x00, 0xff, 0x00, 0xff];

/// Original record contents, restored after the case so a content-addressed
/// blob shared with a later case is never left corrupt in the shared
/// PostgreSQL database.
#[derive(Default)]
pub(super) struct CorruptBackup {
    pub(super) text: Option<String>,
    pub(super) bytes: Option<Vec<u8>>,
}

fn seed(target: CorruptTarget) -> StoreOperation {
    StoreOperation::SeedCorruptRecord { target }
}

fn restore(target: CorruptTarget) -> StoreOperation {
    StoreOperation::RestoreCorruptRecord { target }
}

fn drive(method: SurfaceMethod) -> StoreOperation {
    StoreOperation::DriveSurface { method }
}

fn seed_graph() -> StoreOperation {
    commit(
        "seed_corrupt_input_graph",
        0,
        append(
            vec![
                NodeSpec::new("root", None, "root"),
                NodeSpec::new("active-frame", Some("root"), "active"),
            ],
            Some("active-frame"),
        ),
    )
}

/// A corrupt graph-node row must refuse the whole-graph read, the single-node
/// read, and the commit that appends beside it, with no residue.
pub(super) fn corrupt_graph_node_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CorruptGraphNodeRefusals,
        operations: vec![
            seed_graph(),
            seed(CorruptTarget::GraphNodeJson),
            drive(SurfaceMethod::LoadSession),
            drive(SurfaceMethod::LoadKnownNode),
            commit(
                "append_over_corrupt_graph_node",
                1,
                append(
                    vec![NodeSpec::new("appended", Some("active-frame"), "appended")],
                    Some("appended"),
                ),
            ),
            restore(CorruptTarget::GraphNodeJson),
        ],
    }
}

/// A corrupt pending-turn-input row must refuse both its list paths and the
/// read-modify-write claim, with no residue.
pub(super) fn corrupt_pending_turn_input_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CorruptPendingTurnInputRefusals,
        operations: vec![
            seed_graph(),
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "corrupt-input-owner",
            },
            seed(CorruptTarget::PendingTurnInputJson),
            drive(SurfaceMethod::ListPendingTurnInputs),
            drive(SurfaceMethod::ListTurnInputApplications),
            drive(SurfaceMethod::ClaimNextTurnInputs),
            restore(CorruptTarget::PendingTurnInputJson),
        ],
    }
}

/// A corrupt queued-work payload must refuse every queued-work list path and
/// the read-modify-write claim, with no residue.
pub(super) fn corrupt_queued_work_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CorruptQueuedWorkRefusals,
        operations: vec![
            seed_graph(),
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "corrupt-queued-work-owner",
            },
            seed(CorruptTarget::QueuedWorkItemPayloadJson),
            drive(SurfaceMethod::ListQueuedWork),
            drive(SurfaceMethod::ListPendingQueuedWork),
            drive(SurfaceMethod::PendingSessionWorkOrdering),
            drive(SurfaceMethod::ClaimReadyQueuedWork),
            restore(CorruptTarget::QueuedWorkItemPayloadJson),
        ],
    }
}

/// FIG-2836's shape: the prior checkpoint manifest is undecodable, and the
/// commit that would replace it reads it inside its own transaction. The
/// replacement must not become durable before the refusal is returned.
pub(super) fn corrupt_prior_checkpoint_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CorruptPriorCheckpointRefusals,
        operations: vec![
            StoreOperation::Commit {
                label: "seed_checkpoint_bodies",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "checkpoint-prefix")],
                    Some("active-frame"),
                ),
                turn_commit: None,
                checkpoint: CheckpointSpec::Bodies,
                usage: false,
                adopt_attachment: false,
            },
            seed(CorruptTarget::CheckpointManifestBlob),
            drive(SurfaceMethod::LoadSession),
            StoreOperation::Commit {
                label: "replace_corrupt_prior_checkpoint_manifest",
                expected_head_revision: 1,
                graph: append(
                    vec![NodeSpec::new("appended", Some("active-frame"), "appended")],
                    Some("appended"),
                ),
                turn_commit: None,
                checkpoint: CheckpointSpec::PriorRefs,
                usage: false,
                adopt_attachment: false,
            },
            restore(CorruptTarget::CheckpointManifestBlob),
        ],
    }
}

pub(super) fn corrupt_input_cases() -> Vec<GeneratedCase> {
    vec![
        corrupt_graph_node_case(),
        corrupt_pending_turn_input_case(),
        corrupt_queued_work_case(),
        corrupt_prior_checkpoint_case(),
    ]
}

impl BackendRunner {
    /// Make one persisted record undecodable, stashing the original so the
    /// case can restore it.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub(super) async fn seed_corrupt_record(
        &mut self,
        target: CorruptTarget,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let session_id = self.session_id.clone();
        let mut backup = CorruptBackup::default();
        match &self.raw_reader {
            RawDurableReader::InMemory { .. } => panic!(
                "the in-memory reference store holds typed records and cannot carry an \
                 undecodable persisted record; corrupt-input cases must exclude it"
            ),
            RawDurableReader::Sqlite { path, .. } => {
                let connection =
                    rusqlite::Connection::open(path).expect("open SQLite corruption seam");
                connection
                    .busy_timeout(Duration::from_secs(15))
                    .expect("configure SQLite corruption seam busy timeout");
                match target {
                    CorruptTarget::GraphNodeJson => {
                        let node_id = scoped_node_id(&session_id, "root");
                        backup.text = connection
                            .query_row(
                                "SELECT node_json FROM graph_nodes
                                 WHERE session_id = ?1 AND node_id = ?2",
                                rusqlite::params![session_id.as_str(), node_id],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .expect("read SQLite graph node before corruption");
                        let changed = connection
                            .execute(
                                "UPDATE graph_nodes SET node_json = ?3
                                 WHERE session_id = ?1 AND node_id = ?2",
                                rusqlite::params![session_id.as_str(), node_id, CORRUPT_TEXT],
                            )
                            .expect("corrupt SQLite graph node");
                        assert_eq!(changed, 1, "SQLite corruption seam found no graph node");
                    }
                    CorruptTarget::PendingTurnInputJson => {
                        backup.text = connection
                            .query_row(
                                "SELECT input_json FROM pending_turn_inputs WHERE session_id = ?1",
                                [session_id.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .expect("read SQLite pending turn input before corruption");
                        let changed = connection
                            .execute(
                                "UPDATE pending_turn_inputs SET input_json = ?2
                                 WHERE session_id = ?1",
                                rusqlite::params![session_id.as_str(), CORRUPT_TEXT],
                            )
                            .expect("corrupt SQLite pending turn input");
                        assert_eq!(
                            changed, 1,
                            "SQLite corruption seam found no pending turn input"
                        );
                    }
                    CorruptTarget::QueuedWorkItemPayloadJson => {
                        backup.text = connection
                            .query_row(
                                "SELECT payload_json FROM queued_work_items
                                 WHERE batch_id IN
                                     (SELECT batch_id FROM queued_work_batches
                                      WHERE session_id = ?1)",
                                [session_id.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .expect("read SQLite queued-work payload before corruption");
                        let changed = connection
                            .execute(
                                "UPDATE queued_work_items SET payload_json = ?2
                                 WHERE batch_id IN
                                     (SELECT batch_id FROM queued_work_batches
                                      WHERE session_id = ?1)",
                                rusqlite::params![session_id.as_str(), CORRUPT_TEXT],
                            )
                            .expect("corrupt SQLite queued-work payload");
                        assert_eq!(
                            changed, 1,
                            "SQLite corruption seam found no queued-work item"
                        );
                    }
                    CorruptTarget::CheckpointManifestBlob => {
                        backup.bytes = connection
                            .query_row(
                                "SELECT content FROM blobs WHERE hash =
                                     (SELECT checkpoint_ref FROM session_head
                                      WHERE session_id = ?1)",
                                [session_id.as_str()],
                                |row| row.get::<_, Vec<u8>>(0),
                            )
                            .optional()
                            .expect("read SQLite checkpoint manifest before corruption");
                        let changed = connection
                            .execute(
                                "UPDATE blobs SET content = ?2 WHERE hash =
                                     (SELECT checkpoint_ref FROM session_head
                                      WHERE session_id = ?1)",
                                rusqlite::params![session_id.as_str(), CORRUPT_BYTES],
                            )
                            .expect("corrupt SQLite checkpoint manifest");
                        assert_eq!(
                            changed, 1,
                            "SQLite corruption seam found no checkpoint manifest blob"
                        );
                    }
                }
            }
            RawDurableReader::Postgres { pool, .. } => match target {
                CorruptTarget::GraphNodeJson => {
                    let node_id = scoped_node_id(&session_id, "root");
                    backup.text = sqlx::query_scalar(
                        "SELECT node_json FROM lash_graph_nodes
                         WHERE session_id = $1 AND node_id = $2",
                    )
                    .bind(session_id.as_str())
                    .bind(&node_id)
                    .fetch_optional(pool)
                    .await
                    .expect("read Postgres graph node before corruption");
                    let result = sqlx::query(
                        "UPDATE lash_graph_nodes SET node_json = $3
                         WHERE session_id = $1 AND node_id = $2",
                    )
                    .bind(session_id.as_str())
                    .bind(&node_id)
                    .bind(CORRUPT_TEXT)
                    .execute(pool)
                    .await
                    .expect("corrupt Postgres graph node");
                    assert_eq!(
                        result.rows_affected(),
                        1,
                        "Postgres corruption seam found no graph node"
                    );
                }
                CorruptTarget::PendingTurnInputJson => {
                    backup.text = sqlx::query_scalar(
                        "SELECT input_json FROM lash_pending_turn_inputs WHERE session_id = $1",
                    )
                    .bind(session_id.as_str())
                    .fetch_optional(pool)
                    .await
                    .expect("read Postgres pending turn input before corruption");
                    let result = sqlx::query(
                        "UPDATE lash_pending_turn_inputs SET input_json = $2
                         WHERE session_id = $1",
                    )
                    .bind(session_id.as_str())
                    .bind(CORRUPT_TEXT)
                    .execute(pool)
                    .await
                    .expect("corrupt Postgres pending turn input");
                    assert_eq!(
                        result.rows_affected(),
                        1,
                        "Postgres corruption seam found no pending turn input"
                    );
                }
                CorruptTarget::QueuedWorkItemPayloadJson => {
                    backup.text = sqlx::query_scalar(
                        "SELECT payload_json FROM lash_queued_work_items
                         WHERE batch_id IN
                             (SELECT batch_id FROM lash_queued_work_batches
                              WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .fetch_optional(pool)
                    .await
                    .expect("read Postgres queued-work payload before corruption");
                    let result = sqlx::query(
                        "UPDATE lash_queued_work_items SET payload_json = $2
                         WHERE batch_id IN
                             (SELECT batch_id FROM lash_queued_work_batches
                              WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .bind(CORRUPT_TEXT)
                    .execute(pool)
                    .await
                    .expect("corrupt Postgres queued-work payload");
                    assert_eq!(
                        result.rows_affected(),
                        1,
                        "Postgres corruption seam found no queued-work item"
                    );
                }
                CorruptTarget::CheckpointManifestBlob => {
                    backup.bytes = sqlx::query_scalar(
                        "SELECT content FROM lash_blobs WHERE hash =
                             (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .fetch_optional(pool)
                    .await
                    .expect("read Postgres checkpoint manifest before corruption");
                    let result = sqlx::query(
                        "UPDATE lash_blobs SET content = $2 WHERE hash =
                             (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .bind(CORRUPT_BYTES)
                    .execute(pool)
                    .await
                    .expect("corrupt Postgres checkpoint manifest");
                    assert_eq!(
                        result.rows_affected(),
                        1,
                        "Postgres corruption seam found no checkpoint manifest blob"
                    );
                }
            },
        }
        assert!(
            backup.text.is_some() || backup.bytes.is_some(),
            "{} corruption seam captured no original record for {target:?}",
            self.name
        );
        self.surface.corrupt_backup = Some(backup);
        self.surface.corrupt_target = Some(target);
        Ok(None)
    }

    /// Put the original bytes back. Content-addressed blobs are shared, and
    /// the PostgreSQL leg runs against one database for the whole
    /// differential, so a corruption left behind would be another case's
    /// unexplained failure.
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    pub(super) async fn restore_corrupt_record(
        &mut self,
        target: CorruptTarget,
    ) -> Result<Option<ComparableRuntimeCommitResult>, StoreError> {
        let session_id = self.session_id.clone();
        let backup = self
            .surface
            .corrupt_backup
            .take()
            .expect("generated sequence seeds corruption before restoring it");
        self.surface.corrupt_target = None;
        restore_corrupt_record_raw(&self.raw_reader, &session_id, target, backup).await;
        Ok(None)
    }
}

/// Put the original bytes back. Separate from the step so the RAII guard below
/// can run it on a path that never reaches the restore step.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
pub(super) async fn restore_corrupt_record_raw(
    raw_reader: &RawDurableReader,
    session_id: &SessionId,
    target: CorruptTarget,
    backup: CorruptBackup,
) {
    {
        match raw_reader {
            RawDurableReader::InMemory { .. } => unreachable!("in-memory is not corrupted"),
            RawDurableReader::Sqlite { path, .. } => {
                let connection =
                    rusqlite::Connection::open(path).expect("open SQLite restoration seam");
                connection
                    .busy_timeout(Duration::from_secs(15))
                    .expect("configure SQLite restoration seam busy timeout");
                match target {
                    CorruptTarget::GraphNodeJson => {
                        connection
                            .execute(
                                "UPDATE graph_nodes SET node_json = ?3
                                 WHERE session_id = ?1 AND node_id = ?2",
                                rusqlite::params![
                                    session_id.as_str(),
                                    scoped_node_id(session_id, "root"),
                                    backup.text.expect("graph-node backup is text")
                                ],
                            )
                            .expect("restore SQLite graph node");
                    }
                    CorruptTarget::PendingTurnInputJson => {
                        connection
                            .execute(
                                "UPDATE pending_turn_inputs SET input_json = ?2
                                 WHERE session_id = ?1",
                                rusqlite::params![
                                    session_id.as_str(),
                                    backup.text.expect("pending-input backup is text")
                                ],
                            )
                            .expect("restore SQLite pending turn input");
                    }
                    CorruptTarget::QueuedWorkItemPayloadJson => {
                        connection
                            .execute(
                                "UPDATE queued_work_items SET payload_json = ?2
                                 WHERE batch_id IN
                                     (SELECT batch_id FROM queued_work_batches
                                      WHERE session_id = ?1)",
                                rusqlite::params![
                                    session_id.as_str(),
                                    backup.text.expect("queued-work backup is text")
                                ],
                            )
                            .expect("restore SQLite queued-work payload");
                    }
                    CorruptTarget::CheckpointManifestBlob => {
                        connection
                            .execute(
                                "UPDATE blobs SET content = ?2 WHERE hash =
                                     (SELECT checkpoint_ref FROM session_head
                                      WHERE session_id = ?1)",
                                rusqlite::params![
                                    session_id.as_str(),
                                    backup.bytes.expect("checkpoint backup is bytes")
                                ],
                            )
                            .expect("restore SQLite checkpoint manifest");
                    }
                }
            }
            RawDurableReader::Postgres { pool, .. } => match target {
                CorruptTarget::GraphNodeJson => {
                    sqlx::query(
                        "UPDATE lash_graph_nodes SET node_json = $3
                         WHERE session_id = $1 AND node_id = $2",
                    )
                    .bind(session_id.as_str())
                    .bind(scoped_node_id(session_id, "root"))
                    .bind(backup.text.expect("graph-node backup is text"))
                    .execute(pool)
                    .await
                    .expect("restore Postgres graph node");
                }
                CorruptTarget::PendingTurnInputJson => {
                    sqlx::query(
                        "UPDATE lash_pending_turn_inputs SET input_json = $2
                         WHERE session_id = $1",
                    )
                    .bind(session_id.as_str())
                    .bind(backup.text.expect("pending-input backup is text"))
                    .execute(pool)
                    .await
                    .expect("restore Postgres pending turn input");
                }
                CorruptTarget::QueuedWorkItemPayloadJson => {
                    sqlx::query(
                        "UPDATE lash_queued_work_items SET payload_json = $2
                         WHERE batch_id IN
                             (SELECT batch_id FROM lash_queued_work_batches
                              WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .bind(backup.text.expect("queued-work backup is text"))
                    .execute(pool)
                    .await
                    .expect("restore Postgres queued-work payload");
                }
                CorruptTarget::CheckpointManifestBlob => {
                    sqlx::query(
                        "UPDATE lash_blobs SET content = $2 WHERE hash =
                             (SELECT checkpoint_ref FROM lash_sessions WHERE session_id = $1)",
                    )
                    .bind(session_id.as_str())
                    .bind(backup.bytes.expect("checkpoint backup is bytes"))
                    .execute(pool)
                    .await
                    .expect("restore Postgres checkpoint manifest");
                }
            },
        }
    }
}

/// Restore-on-drop for the corrupt-input cases.
///
/// Checkpoint manifest blobs are content-addressed and shared across sessions,
/// and every case in this suite runs against one PostgreSQL database. A case
/// that panics or breaks out of its step loop between the seed step and the
/// restore step would otherwise leave the shared blob corrupt, failing every
/// later run of the suite until the database is recreated. The restore step is
/// still the normal path -- it is a compared step, and the digest must show the
/// row coming back; this guard only covers the paths that never reach it.
impl Drop for BackendRunner {
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    fn drop(&mut self) {
        let (Some(backup), Some(target)) = (
            self.surface.corrupt_backup.take(),
            self.surface.corrupt_target.take(),
        ) else {
            return;
        };
        let reader = &self.raw_reader;
        let session_id = self.session_id.clone();
        // A dedicated runtime on its own thread: `Drop` cannot await, and this
        // may run while unwinding inside the harness's own runtime.
        let restored = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("build corrupt-record restoration runtime")
                        .block_on(restore_corrupt_record_raw(
                            reader,
                            &session_id,
                            target,
                            backup,
                        ));
                })
                .join()
        });
        if restored.is_err() && !std::thread::panicking() {
            panic!("failed to restore a corrupt record while dropping the backend runner");
        }
    }
}
