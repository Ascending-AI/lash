//! The attachment write-ahead manifest and its garbage-collection fence.
//!
//! The SQLite owner of the attachment family: `attachment_manifest` (one row
//! per session/digest intent) and `attachment_condemnations` (the CAS fence
//! that decides whether a digest's bytes may be deleted).
//!
//! Every atom runs inside `SqliteConnection::write`/`write_flow`
//! (`BEGIN IMMEDIATE`) or an explicit read, so the condemnation check, the
//! root predicate and the write they guard cannot interleave with a competing
//! writer. That is the whole of SQLite's half of the fence: PostgreSQL needs a
//! per-digest advisory lock to buy the same thing under `READ COMMITTED`.
//!
//! Every DB body is a synchronous rusqlite closure handed to `conn.call`
//! (reads) or `conn.write` (read-then-write); only the wrapper call is awaited.

use std::sync::LazyLock;

use crate::scope_fence::Schema;
use lash_sansio::SessionId;
use lash_store_sql::attachment::condemnation::CondemnationStatements;
use lash_store_sql::attachment::manifest::{ManifestProcessOwnerStatements, ManifestStatements};
use lash_store_sql::{SchemaTables, TableLayout, Vocabulary, VocabularyTerm};

use super::*;

lash_store_sql::statements! {
    /// `attachment_manifest` statements only SQLite issues.
    pub(crate) struct ManifestSqliteStatements @ "attachment_manifest" {
        /// Reclaim every attachment root a deleted session left behind.
        ///
        /// FIG-653: graph retention is a prune precondition for committed
        /// roots, and owner-level retention deliberately includes suffix
        /// attachments, because the manifest has no node edge — forks and
        /// pins keep these rows until their final prefix dies.
        ///
        /// Forks on the tombstone literal: `graph_nodes.tombstoned` is
        /// INTEGER 0/1 on SQLite and BOOLEAN on PostgreSQL.
        delete_deleted_session_roots = "DELETE FROM attachment_manifest AS manifest
             WHERE EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                           WHERE deleted.session_id = manifest.session_id)
               AND (manifest.committed_at_ms IS NULL OR NOT EXISTS (
                   SELECT 1 FROM graph_nodes AS node
                   WHERE node.session_id = manifest.session_id AND node.tombstoned = 0
               ))";

        /// Forget `?2` in session `?1` unless a live node still roots it.
        /// Same tombstone-literal fork as
        /// [`ManifestSqliteStatements::delete_deleted_session_roots`].
        forget_for_session = "DELETE FROM attachment_manifest
             WHERE session_id = ?1 AND attachment_id = ?2 AND (
                 committed_at_ms IS NULL OR NOT EXISTS (
                     SELECT 1 FROM graph_nodes AS node
                     WHERE node.session_id = attachment_manifest.session_id
                       AND node.tombstoned = 0
                 ))";

        /// Every uncommitted intent older than `?1`.
        ///
        /// The ordering is the fork: SQLite reports oldest intent first,
        /// PostgreSQL reports digest order. Both are total and neither caller
        /// depends on the other's, so the two orders are left exactly as they
        /// stand rather than unified inside a refactor.
        select_uncommitted = "SELECT attachment_id, session_id, canonical_uri, intent_at_ms,
                 committed_at_ms, owner_kind, owner_id, owner_incarnation, written_at_ms
             FROM attachment_manifest
             WHERE committed_at_ms IS NULL AND intent_at_ms <= ?1
             ORDER BY intent_at_ms ASC";
    }
}

lash_store_sql::statements! {
    /// `attachment_condemnations` statements only SQLite issues.
    pub(crate) struct CondemnationSqliteStatements @ "attachment_condemnation" {
        /// Whether a physical delete is already in flight for `?1`.
        ///
        /// SQLite answers with the row's presence; PostgreSQL wraps the same
        /// predicate in `SELECT EXISTS(…)` because its driver reads a scalar
        /// rather than an optional row.
        select_deleting = "SELECT 1 FROM attachment_condemnations
             WHERE attachment_id = ?1 AND phase = 'deleting'";

        /// Whether `?1` is condemned at all.
        ///
        /// SQLite alone asks this: it reads the absence and inserts under one
        /// `BEGIN IMMEDIATE` lock, so the read is the contention check.
        /// PostgreSQL cannot hold that across statements and detects a peer
        /// sweeper through the insert's `ON CONFLICT` instead.
        select_exists = "SELECT 1 FROM attachment_condemnations WHERE attachment_id = ?1";

        /// Condemn `?1`.
        ///
        /// No `ON CONFLICT`: the absence of the row was read under the same
        /// write lock this insert commits under, so a conflict here is a
        /// defect and the constraint error is kept rather than swallowed.
        insert_condemned = "INSERT INTO attachment_condemnations (attachment_id, phase)
             VALUES (?1, 'condemned')";
    }
}

/// The attachment owner classes, spelled once in `lash-core` and named as
/// tokens by the GC predicates that compare against them.
const ATTACHMENT_OWNER: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new(
        "turn_attachment_owner",
        lash_core::store_backend_support::turn_attachment_owner_predicate_sql,
    ),
    VocabularyTerm::new(
        "process_attachment_owner",
        lash_core::store_backend_support::process_attachment_owner_predicate_sql,
    ),
]);

/// The tables the attachment family's statements name that live in the
/// session catalog: its own two, and the three root sets its GC predicates
/// consult. Every one of them is in the catalog's own file, so they are
/// addressed through `main` on the connection that owns it.
const CATALOG_TABLES: &[&str] = &[
    lash_store_sql::attachment::manifest::TABLE,
    lash_store_sql::attachment::condemnation::TABLE,
    "deleted_sessions",
    "graph_nodes",
    "runtime_turn_commits",
];

/// The session catalog alone: no process registry is bound, so `processes` is
/// not placed and the statements that prove a process owner dead cannot be
/// rendered for this layout at all.
const CATALOG: TableLayout =
    TableLayout::new(&[SchemaTables::new(Schema::Main.qualifier(), CATALOG_TABLES)]);

/// The session catalog beside a bound process registry.
///
/// This is the layout FIG-3406 exists for: one statement addressing
/// `main.attachment_manifest` and `process_registry.processes`, so the
/// owner-death proof is part of the same SQLite statement and transaction as
/// the forget it guards rather than a read-then-forget pair racing a
/// registration.
const CATALOG_BESIDE_REGISTRY: TableLayout = TableLayout::new(&[
    SchemaTables::new(Schema::Main.qualifier(), CATALOG_TABLES),
    SchemaTables::new(Schema::ProcessRegistry.qualifier(), &["processes"]),
]);

/// Every attachment-family statement, rendered once.
pub(crate) struct AttachmentSql {
    /// `attachment_manifest` statements both backends issue verbatim.
    pub(crate) manifest: ManifestStatements,
    /// `attachment_manifest` statements only SQLite issues.
    pub(crate) manifest_sqlite: ManifestSqliteStatements,
    /// The GC probes that prove a process owner dead, rendered for the layout
    /// that reaches a bound registry. A store with none never reads them.
    pub(crate) manifest_process_owner: ManifestProcessOwnerStatements,
    /// `attachment_condemnations` statements both backends issue verbatim.
    pub(crate) condemnation: CondemnationStatements,
    /// `attachment_condemnations` statements only SQLite issues.
    pub(crate) condemnation_sqlite: CondemnationSqliteStatements,
}

/// The family's own tables live in the session catalog and are never reached
/// through an `ATTACH`ed name; the process registry its GC consults is. Two
/// layouts, both rendered once here, and the call site picks by whether a
/// registry is bound.
static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let catalog = lash_store_sql::Dialect::sqlite(CATALOG).with_vocabulary(ATTACHMENT_OWNER);
    let beside_registry =
        lash_store_sql::Dialect::sqlite(CATALOG_BESIDE_REGISTRY).with_vocabulary(ATTACHMENT_OWNER);
    AttachmentSql {
        manifest: ManifestStatements::render(catalog),
        manifest_sqlite: ManifestSqliteStatements::render(catalog),
        manifest_process_owner: ManifestProcessOwnerStatements::render(beside_registry),
        condemnation: CondemnationStatements::render(catalog),
        condemnation_sqlite: CondemnationSqliteStatements::render(catalog),
    }
});

/// The attachment-family statements, rendered once at first use.
pub(crate) fn attachment_sql() -> &'static AttachmentSql {
    &ATTACHMENT_SQL
}

/// The live-root probe this store may issue: the one that proves a process
/// owner dead only when a registry is attached to read it from.
fn live_root_sql(process_registry_attached: bool) -> &'static str {
    if process_registry_attached {
        attachment_sql()
            .manifest_process_owner
            .select_live_root_proving_process_death
            .sql()
    } else {
        attachment_sql().manifest.select_live_root.sql()
    }
}

/// Adopt stored references under the boundary transaction.
///
/// Validate every digest for upload evidence first, so a batch containing one
/// unknown digest writes nothing at all, then acquire this session's roots.
pub(crate) fn commit_attachment_refs_conn(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    attachment_ids: &[AttachmentId],
    now: i64,
) -> Result<(), StoreError> {
    let mut evidence = std::collections::BTreeMap::new();
    for id in attachment_ids {
        let deleting = tx
            .query_row(
                attachment_sql().condemnation_sqlite.select_deleting.sql(),
                params![id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        if deleting {
            return Err(StoreError::UnknownAttachment { digest: id.clone() });
        }
        // Evidence from any session: the uploader and the adopter need not be
        // the same, and the earliest proven upload is the one that is copied.
        let written_at_ms = tx
            .query_row(
                attachment_sql().manifest.select_earliest_written_at.sql(),
                params![id.as_str()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(sqlite_error)?
            .flatten();
        let Some(written_at_ms) = written_at_ms else {
            return Err(StoreError::UnknownAttachment { digest: id.clone() });
        };
        evidence.insert(id.clone(), written_at_ms);
    }
    for id in attachment_ids {
        // The fresh committed root supersedes an unarmed, unclaimed
        // condemnation. A restoring writer's claim is left for that writer to
        // settle.
        tx.execute(
            attachment_sql()
                .condemnation
                .delete_unclaimed_condemned
                .sql(),
            params![id.as_str()],
        )
        .map_err(sqlite_error)?;
        // Copy the evidence onto the adopter's row so it outlives the
        // uploader's intent being forgotten.
        tx.execute(
            attachment_sql().manifest.upsert_adopted.sql(),
            params![
                now,
                id.as_str(),
                session_id.as_str(),
                format!("lash-attachment://blake3/{id}"),
                evidence.get(id).copied(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

impl Store {
    /// Enumerate the durable condemnation authority without exposing write
    /// tokens. Persisted phase/provenance combinations are decoded strictly so
    /// a corrupt row cannot be mistaken for sweep-owned maintenance work.
    pub(crate) async fn list_attachment_condemnations(
        &self,
    ) -> Result<Vec<lash_core::AttachmentCondemnationRecord>, StoreError> {
        let rows = self
            .conn
            .call(|conn| {
                let mut statement = conn.prepare(attachment_sql().condemnation.select_all.sql())?;
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        let mut condemnations = rows
            .into_iter()
            .map(|(digest, phase, write_token, write_session_id)| {
                let digest = AttachmentId::parse(&digest).map_err(|error| {
                    stored_data_corrupt(
                        "attachment condemnation",
                        format!("attachment_id is not a valid attachment id: {error}"),
                    )
                })?;
                lash_core::store::decode_attachment_condemnation_record(
                    digest,
                    &phase,
                    write_token.is_some(),
                    write_session_id.map(SessionId::from),
                )
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        condemnations.sort_by(|left, right| left.digest.cmp(&right.digest));
        Ok(condemnations)
    }

    /// `Free -> Condemned` for one digest, conditional on there being no live
    /// root. The root predicate, the existing-condemnation check, and the insert
    /// share one SQLite transaction, so this is one CAS against every concurrent
    /// [`AttachmentManifest::begin_attachment_write`].
    pub(crate) async fn condemn_attachment(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<lash_core::AttachmentCondemnation, StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
        let live_ref_sql = live_root_sql(self.process_registry_attached);
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::AttachmentCondemnation, StoreError> = (|| {
                    let rooted = tx
                        .query_row(live_ref_sql, params![attachment_id, cutoff], |_| Ok(()))
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    if rooted {
                        return Ok(lash_core::AttachmentCondemnation::RootPresent);
                    }
                    let condemned = tx
                        .query_row(
                            attachment_sql().condemnation_sqlite.select_exists.sql(),
                            params![attachment_id],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    if condemned {
                        return Ok(lash_core::AttachmentCondemnation::AlreadyCondemned);
                    }
                    tx.execute(
                        attachment_sql().condemnation_sqlite.insert_condemned.sql(),
                        params![attachment_id],
                    )
                    .map_err(sqlite_error)?;
                    // The digest is proven unrooted, so every remaining manifest
                    // row for it is stale evidence of an upload whose bytes this
                    // sweep is about to delete. Clearing them here is what makes
                    // a negative byte-absence tombstone unnecessary.
                    tx.execute(
                        attachment_sql().manifest.delete_by_id.sql(),
                        params![attachment_id],
                    )
                    .map_err(sqlite_error)?;
                    Ok(lash_core::AttachmentCondemnation::Condemned)
                })(
                );
                Ok(match outcome {
                    Ok(condemnation) => TxOutcome::Commit(Ok(condemnation)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    /// `Condemned -> Deleting`: the CAS that authorizes the physical delete. A
    /// writer that revoked the condemnation removed the row, so the conditional
    /// UPDATE matches nothing and the delete is never issued.
    pub(crate) async fn arm_attachment_delete(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<lash_core::AttachmentDeleteArming, StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        let armed = self
            .conn
            .write(move |tx| {
                tx.execute(
                    attachment_sql().condemnation.arm_delete.sql(),
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(if armed == 1 {
            lash_core::AttachmentDeleteArming::Armed
        } else {
            lash_core::AttachmentDeleteArming::Revoked
        })
    }

    /// Return an abandoned sweep's un-tokened `Condemned` or `Deleting` digest
    /// to `Free`. A stale sweep cannot clear a restoring writer's token.
    pub(crate) async fn release_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    attachment_sql().condemnation.delete_sweep_owned.sql(),
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }

    /// Clear an abandoned restoring writer under explicit host quiescence.
    /// Retire `Condemned` only when its associated intent became committed,
    /// otherwise preserve it after removing that unstamped intent.
    pub(crate) async fn recover_abandoned_attachment_write(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<(), StoreError> = (|| {
                    let claim = tx
                        .query_row(
                            attachment_sql().condemnation.select_claim.sql(),
                            params![attachment_id],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    let Some((token, session_id)) = claim else {
                        return Ok(());
                    };
                    tx.execute(
                        attachment_sql().manifest.delete_unproven_for_session.sql(),
                        params![attachment_id, session_id],
                    )
                    .map_err(sqlite_error)?;
                    let condemned_superseded = tx
                        .execute(
                            attachment_sql().condemnation.delete_superseded_claim.sql(),
                            params![attachment_id, token, session_id],
                        )
                        .map_err(sqlite_error)?;
                    if condemned_superseded == 0 {
                        tx.execute(
                            attachment_sql().condemnation.clear_write_claim.sql(),
                            params![attachment_id, token],
                        )
                        .map_err(sqlite_error)?;
                    }
                    Ok(())
                })();
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    /// Delete the condemnation row after the physical delete succeeds: the
    /// digest returns to `Free` holding no upload evidence, because the
    /// condemnation already cleared every manifest row for it.
    pub(crate) async fn retire_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let attachment_id = attachment_id.as_str().to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    attachment_sql().condemnation.delete_armed.sql(),
                    params![attachment_id],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl AttachmentManifest for Store {
    /// The writer half of the GC fence: the condemnation check, the claim, and
    /// the intent upsert are one SQLite transaction, so a sweeper's condemn CAS
    /// either precedes this whole mutation or fails against the intent it wrote.
    async fn begin_attachment_write(
        &self,
        intent: AttachmentIntent,
    ) -> Result<lash_core::AttachmentWriteFence, StoreError> {
        {
            let attachment_id = intent.attachment_id.as_str().to_string();
            let session_id = intent.session_id.clone();
            let canonical_uri = intent.canonical_uri.as_str().to_string();
            let intent_at_ms = intent.intent_at_epoch_ms as i64;
            let owner_kind = intent.owner.as_ref().map(|owner| owner.kind().as_str());
            let owner_id = intent.owner.as_ref().map(|owner| owner.id().to_string());
            let owner_incarnation = intent
                .owner
                .as_ref()
                .and_then(lash_core::AttachmentOwner::incarnation)
                .map(|incarnation| i64::try_from(incarnation.registration_sequence()))
                .transpose()
                .map_err(|_| {
                    StoreError::Backend("attachment owner incarnation exceeds i64".to_string())
                })?;
            let write_id = lash_core::AttachmentWriteToken::new();
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<lash_core::AttachmentWriteFence, StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        let condemnation = tx
                            .query_row(
                                attachment_sql().condemnation.select_phase_and_claim.sql(),
                                params![attachment_id],
                                |row| {
                                    Ok((
                                        row.get::<_, String>(0)?,
                                        row.get::<_, Option<String>>(1)?,
                                    ))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        match condemnation
                            .as_ref()
                            .map(|(phase, token)| (phase.as_str(), token.is_some()))
                        {
                            // The physical delete is already in flight: record
                            // nothing, so these bytes cannot land inside it.
                            Some(("deleting", _)) | Some(("condemned", true)) => {
                                return Ok(lash_core::AttachmentWriteFence::ReclamationInFlight);
                            }
                            // Keep the condemnation present and own it with this
                            // attempt's identity until the backend put settles.
                            Some(("condemned", false)) => {
                                let claimed = tx
                                    .execute(
                                        attachment_sql().condemnation.claim_write.sql(),
                                        params![
                                            attachment_id,
                                            write_id.as_hex(),
                                            session_id.as_str()
                                        ],
                                    )
                                    .map_err(sqlite_error)?;
                                if claimed == 0 {
                                    return Ok(
                                        lash_core::AttachmentWriteFence::ReclamationInFlight,
                                    );
                                }
                            }
                            None => {}
                            Some((phase, _)) => {
                                return Err(StoreError::Backend(format!(
                                    "attachment `{attachment_id}` has unknown condemnation phase `{phase}`"
                                )));
                            }
                        }
                        // A fresh attempt has proven nothing, so it takes the row
                        // with no upload stamp. Evidence and commitment already on
                        // the row were earned by earlier attempts and are kept.
                        tx.execute(
                            attachment_sql().manifest.insert_intent.sql(),
                            params![
                                attachment_id,
                                session_id.as_str(),
                                canonical_uri,
                                intent_at_ms,
                                owner_kind,
                                owner_id,
                                owner_incarnation,
                                write_id.as_hex()
                            ],
                        )
                        .map_err(sqlite_error)?;
                        Ok(lash_core::AttachmentWriteFence::Granted(
                            lash_core::AttachmentWritePermit::new(write_id),
                        ))
                    })(
                    );
                    Ok(match outcome {
                        Ok(fence) => TxOutcome::Commit(Ok(fence)),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn complete_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let digest = intent.attachment_id.clone();
        let attachment_id = intent.attachment_id.as_str().to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        let written_at_ms = crate::clamp_epoch_ms(self.clock.timestamp_ms());
        {
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        // Id-matched: only the row this attempt still owns is
                        // stamped, and the first proven upload is kept.
                        let stamped = tx
                            .execute(
                                attachment_sql().manifest.stamp_written.sql(),
                                params![
                                    attachment_id,
                                    session_id.as_str(),
                                    write_id,
                                    written_at_ms
                                ],
                            )
                            .map_err(sqlite_error)?;
                        if stamped == 0 {
                            return Err(StoreError::StaleWritePermit { digest });
                        }
                        // The bytes exist now, so this attempt's claim on the
                        // condemnation is released with the condemnation itself.
                        tx.execute(
                            attachment_sql().condemnation.delete_by_write_token.sql(),
                            params![attachment_id, write_id],
                        )
                        .map_err(sqlite_error)?;
                        Ok(())
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn abort_attachment_write(
        &self,
        intent: &AttachmentIntent,
        permit: lash_core::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let attachment_id = intent.attachment_id.as_str().to_string();
        let session_id = intent.session_id.clone();
        let write_id = permit.write_id().as_hex();
        {
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        // Only this attempt's own unstamped, uncommitted row. A
                        // superseded permit matches nothing and deletes nothing.
                        tx.execute(
                            attachment_sql().manifest.delete_unproven_for_write.sql(),
                            params![attachment_id, session_id.as_str(), write_id],
                        )
                        .map_err(sqlite_error)?;
                        let condemned_superseded = tx
                            .execute(
                                attachment_sql().condemnation.delete_superseded_claim.sql(),
                                params![attachment_id, write_id, session_id.as_str()],
                            )
                            .map_err(sqlite_error)?;
                        if condemned_superseded == 0 {
                            tx.execute(
                                attachment_sql().condemnation.clear_write_claim.sql(),
                                params![attachment_id, write_id],
                            )
                            .map_err(sqlite_error)?;
                        }
                        Ok(())
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn commit_refs(
        &self,
        session_id: &SessionId,
        attachment_ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        if attachment_ids.is_empty() {
            return Ok(());
        }
        {
            let session_id = SessionId::from(session_id.to_string());
            let attachment_ids = attachment_ids.to_vec();
            let now = self.clock.timestamp_ms() as i64;
            self.conn
                .write_flow(move |tx| {
                    let outcome: Result<(), StoreError> = (|| {
                        crate::persistence::ensure_session_not_deleted_conn(tx, &session_id)?;
                        commit_attachment_refs_conn(tx, &session_id, &attachment_ids, now)
                    })();
                    Ok(match outcome {
                        Ok(()) => TxOutcome::Commit(Ok(())),
                        Err(err) => TxOutcome::Rollback(Err(err)),
                    })
                })
                .await
                .map_err(sqlite_error)?
        }
    }

    async fn list_uncommitted(
        &self,
        older_than_epoch_ms: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        {
            let older_than = crate::clamp_epoch_ms(older_than_epoch_ms);
            self.conn
                .call(move |conn| {
                    let mut stmt =
                        conn.prepare(attachment_sql().manifest_sqlite.select_uncommitted.sql())?;
                    let rows = stmt.query_map(params![older_than], |row| {
                        let id: String = row.get(0)?;
                        let session_id: SessionId = SessionId::from(row.get::<_, String>(1)?);
                        let canonical_uri: String = row.get(2)?;
                        let intent_at_ms: i64 = row.get(3)?;
                        let committed_at_ms: Option<i64> = row.get(4)?;
                        let owner_kind: Option<String> = row.get(5)?;
                        let owner_id: Option<String> = row.get(6)?;
                        let owner_incarnation = row
                            .get::<_, Option<i64>>(7)?
                            .map(|value| {
                                u64_from_sql("AttachmentManifest", "owner_incarnation", value)
                            })
                            .transpose()?;
                        let written_at_ms: Option<i64> = row.get(8)?;
                        let owner = lash_core::store::decode_attachment_owner(
                            owner_kind.as_deref(),
                            owner_id,
                            owner_incarnation,
                        )
                        .map_err(sqlite_conversion_error)?;
                        Ok(AttachmentManifestEntry {
                            attachment_id: crate::attachment_id_from_sql(
                                "AttachmentManifest",
                                "attachment_id",
                                id,
                            )?,
                            session_id,
                            canonical_uri,
                            intent_at_epoch_ms: u64_from_sql(
                                "AttachmentManifest",
                                "intent_at_ms",
                                intent_at_ms,
                            )?,
                            written_at_epoch_ms: written_at_ms
                                .map(|value| {
                                    u64_from_sql("AttachmentManifest", "written_at_ms", value)
                                })
                                .transpose()?,
                            committed_at_epoch_ms: committed_at_ms
                                .map(|value| {
                                    u64_from_sql("AttachmentManifest", "committed_at_ms", value)
                                })
                                .transpose()?,
                            owner,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        }
    }

    async fn forget_aged_uncommitted_intents(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<(), StoreError> {
        {
            let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
            // One conditional DELETE composes age with owner-death proof. The
            // attached process registry makes the NOT EXISTS predicate part of
            // this same SQLite statement and transaction, avoiding a
            // read-process-then-forget race across the per-session topology —
            // which is why the two shapes are two statements rendered for two
            // layouts. Without a registry the owner-death statement has no
            // layout to render for, so process-owned rows are conservatively
            // retained rather than guessed at.
            let forget = if self.process_registry_attached {
                attachment_sql()
                    .manifest_process_owner
                    .delete_aged_uncommitted_proving_process_death
                    .sql()
            } else {
                attachment_sql().manifest.delete_aged_uncommitted.sql()
            };
            self.conn
                .write(move |tx| {
                    tx.execute(
                        attachment_sql()
                            .manifest_sqlite
                            .delete_deleted_session_roots
                            .sql(),
                        [],
                    )?;
                    tx.execute(forget, params![cutoff])?;
                    Ok(())
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        }
    }

    async fn has_live_ref_for_id(
        &self,
        attachment_id: &AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, StoreError> {
        {
            let attachment_id = attachment_id.as_str().to_string();
            let cutoff = crate::clamp_epoch_ms(intent_grace_cutoff_epoch_ms);
            let sql = live_root_sql(self.process_registry_attached);
            self.conn
                .call(move |conn| {
                    conn.query_row(sql, params![attachment_id, cutoff], |_| Ok(()))
                        .optional()
                        .map(|found| found.is_some())
                })
                .await
                .map_err(sqlite_error)
        }
    }

    async fn forget(
        &self,
        session_id: &SessionId,
        attachment_id: &AttachmentId,
    ) -> Result<(), StoreError> {
        {
            let session_id = SessionId::from(session_id.to_string());
            let attachment_id = attachment_id.as_str().to_string();
            self.conn
                .call(move |conn| {
                    conn.execute(
                        attachment_sql().manifest_sqlite.forget_for_session.sql(),
                        params![session_id.as_str(), attachment_id.as_str()],
                    )
                })
                .await
                .map_err(sqlite_error)?;
            Ok(())
        }
    }

    async fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        {
            self.conn
                .call(move |conn| {
                    let mut stmt =
                        conn.prepare(attachment_sql().manifest.select_rooted_ids.sql())?;
                    let rows = stmt.query_map([], |row| {
                        let id: String = row.get(0)?;
                        crate::attachment_id_from_sql("AttachmentManifest", "attachment_id", id)
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()
                })
                .await
                .map_err(sqlite_error)
        }
    }
}

#[cfg(test)]
mod cross_database_plan_tests {
    use super::*;

    /// The text the live-root probe was built with per call before FIG-3406,
    /// reproduced verbatim, including the `format!` site's indentation.
    ///
    /// It is the oracle: the named statement is meant to be the *same query*,
    /// not merely a similar one, so the two must plan identically. Whitespace
    /// is deliberately not matched — an authored statement is indented like
    /// the block it lives in — which is exactly what makes the comparison a
    /// test of the plan rather than of the bytes.
    fn historical_live_ref_sql() -> String {
        let turn_owner_kind = AttachmentOwnerKind::Turn.as_str();
        let process_owner_kind = AttachmentOwnerKind::Process.as_str();
        let process_dead = format!(
            "OR (
            manifest.owner_kind = '{process_owner_kind}'
            AND NOT EXISTS (
                SELECT 1 FROM process_registry.processes AS process
                WHERE process.process_id = manifest.owner_id
                  AND process.incarnation = manifest.owner_incarnation
            )
        )"
        );
        format!(
            "SELECT 1 FROM attachment_manifest AS manifest
         WHERE manifest.attachment_id = ?1
           AND NOT (
                manifest.committed_at_ms IS NULL
                AND manifest.intent_at_ms <= ?2
                AND (
                    manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                    OR (
                        manifest.owner_kind = '{turn_owner_kind}'
                        AND EXISTS (
                            SELECT 1 FROM runtime_turn_commits AS turn_commit
                            WHERE turn_commit.session_id = manifest.session_id
                              AND turn_commit.turn_id <> manifest.owner_id
                              AND turn_commit.committed_at_ms > manifest.intent_at_ms
                        )
                    )
                    {process_dead}
                )
           )
         LIMIT 1"
        )
    }

    /// The historical aged-intent forget, same provenance.
    fn historical_forget_sql() -> String {
        let turn_owner_kind = AttachmentOwnerKind::Turn.as_str();
        let process_owner_kind = AttachmentOwnerKind::Process.as_str();
        let process_dead = format!(
            "OR (
                            manifest.owner_kind = '{process_owner_kind}'
                            AND NOT EXISTS (
                                SELECT 1 FROM process_registry.processes AS process
                                WHERE process.process_id = manifest.owner_id
                                  AND process.incarnation = manifest.owner_incarnation
                            )
                        )"
        );
        format!(
            "DELETE FROM attachment_manifest AS manifest
                         WHERE manifest.committed_at_ms IS NULL
                           AND manifest.intent_at_ms <= ?1
                           AND (
                                manifest.owner_kind IS NULL
                    OR EXISTS (SELECT 1 FROM deleted_sessions AS deleted
                               WHERE deleted.session_id = manifest.session_id)
                                OR (
                                    manifest.owner_kind = '{turn_owner_kind}'
                                    AND EXISTS (
                                        SELECT 1 FROM runtime_turn_commits AS turn_commit
                                        WHERE turn_commit.session_id = manifest.session_id
                                          AND turn_commit.turn_id <> manifest.owner_id
                                          AND turn_commit.committed_at_ms > manifest.intent_at_ms
                                    )
                                )
                                {process_dead}
                           )"
        )
    }

    /// A session catalog with a real process registry attached under the name
    /// production attaches it by, both provisioned from this crate's own
    /// schema so the planner sees the real indexes.
    fn catalog_with_registry() -> (tempfile::TempDir, rusqlite::Connection) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let registry_path = directory.path().join("registry.sqlite3");
        {
            let registry =
                rusqlite::Connection::open(&registry_path).expect("open process registry");
            registry
                .execute_batch(crate::schema::PROCESS_SCHEMA)
                .expect("apply the process registry schema");
            registry
                .execute_batch(crate::schema_fragments::SCOPE_RETIREMENT_TABLE)
                .expect("apply the shared fence fragment");
        }
        let connection = rusqlite::Connection::open(directory.path().join("catalog.sqlite3"))
            .expect("open session catalog");
        connection
            .execute_batch(crate::schema::SCHEMA)
            .expect("apply the durable-core schema");
        connection
            .execute_batch(crate::schema_fragments::AWAIT_EVENT_TABLES)
            .expect("apply the shared await-event fragment");
        connection
            .execute(
                "ATTACH DATABASE ?1 AS process_registry",
                params![registry_path.to_string_lossy().into_owned()],
            )
            .expect("attach the process registry");
        (directory, connection)
    }

    fn seed(connection: &rusqlite::Connection) {
        connection
            .execute_batch(
                "WITH RECURSIVE n(i) AS (
                     SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000
                 )
                 INSERT INTO attachment_manifest (
                     attachment_id, session_id, canonical_uri, intent_at_ms,
                     owner_kind, owner_id, owner_incarnation
                 )
                 SELECT printf('blake3:%064d', i), printf('session-%04d', i), 'uri', i,
                        CASE WHEN i % 2 = 0 THEN 'turn' ELSE 'process' END,
                        printf('owner-%04d', i),
                        CASE WHEN i % 2 = 0 THEN NULL ELSE i END
                 FROM n;
                 ANALYZE;",
            )
            .expect("seed the manifest");
    }

    fn plan(connection: &rusqlite::Connection, sql: &str, parameters: usize) -> String {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|error| panic!("prepare `{sql}`: {error}"));
        let bindings: Vec<rusqlite::types::Value> = (0..parameters)
            .map(|_| rusqlite::types::Value::Integer(0))
            .collect();
        statement
            .query_map(rusqlite::params_from_iter(bindings), |row| {
                row.get::<_, String>(3)
            })
            .expect("explain")
            .collect::<Result<Vec<_>, _>>()
            .expect("explain rows")
            .join(" | ")
    }

    /// FIG-3406's load-bearing claim on the hot path: making the probe a
    /// named, per-table-rendered statement did not change what SQLite does
    /// with it.
    #[test]
    fn the_live_root_probe_and_the_aged_forget_plan_exactly_as_they_did() {
        let (_directory, connection) = catalog_with_registry();
        seed(&connection);

        let probe = plan(&connection, live_root_sql(true), 2);
        assert_eq!(
            probe,
            plan(&connection, &historical_live_ref_sql(), 2),
            "the named live-root probe must plan exactly as the format!ed one did"
        );
        let forget = plan(
            &connection,
            attachment_sql()
                .manifest_process_owner
                .delete_aged_uncommitted_proving_process_death
                .sql(),
            1,
        );
        assert_eq!(
            forget,
            plan(&connection, &historical_forget_sql(), 1),
            "the named aged-intent forget must plan exactly as the format!ed one did"
        );

        // And the plan is the one worth keeping: the probe reaches its digest
        // through an index rather than reading the whole manifest, and the
        // owner-death proof reaches the attached registry through its primary
        // key.
        assert!(
            probe.contains("idx_attachment_manifest_written")
                || probe.contains("USING INDEX")
                || probe.contains("USING COVERING INDEX"),
            "the live-root probe must find its digest through an index: {probe}"
        );
        assert!(
            !probe.contains("SCAN manifest"),
            "the live-root probe must not scan the manifest: {probe}"
        );
        assert!(
            probe.contains("process") && probe.contains("INDEX"),
            "the owner-death proof must reach the registry through an index: {probe}"
        );
    }

    /// The shape a store with no registry issues is the same query minus the
    /// clause it cannot answer — and it plans without ever naming the
    /// registry, which is the observable half of "unrenderable for that
    /// layout".
    #[test]
    fn the_registryless_probe_never_reaches_the_process_registry() {
        let (_directory, connection) = catalog_with_registry();
        seed(&connection);

        let without = live_root_sql(false);
        assert!(
            !without.contains("processes"),
            "a store with no registry must not name the registry's table: {without}"
        );
        assert!(!plan(&connection, without, 2).contains("process "));
        assert!(
            !attachment_sql()
                .manifest
                .delete_aged_uncommitted
                .sql()
                .contains("processes")
        );
    }

    /// The live-root probe and the reconciliation forget are the same
    /// predicate asked two ways; a digest that the sweep would forget must be
    /// exactly a digest the probe reports unrooted, or the GC can delete
    /// bytes something still roots.
    #[test]
    fn the_probe_and_the_forget_agree_on_every_seeded_row() {
        let (_directory, connection) = catalog_with_registry();
        connection
            .execute_batch(
                "INSERT INTO attachment_manifest
                     (attachment_id, session_id, canonical_uri, intent_at_ms, owner_kind, owner_id,
                      owner_incarnation)
                 VALUES
                     ('blake3:aged-host', 's1', 'uri', 10, NULL, NULL, NULL),
                     ('blake3:live-turn', 's2', 'uri', 10, 'turn', 't2', NULL),
                     ('blake3:dead-process', 's3', 'uri', 10, 'process', 'p3', 7);",
            )
            .expect("seed the three owner classes");

        let mut unrooted = Vec::new();
        for digest in [
            "blake3:aged-host",
            "blake3:live-turn",
            "blake3:dead-process",
        ] {
            let rooted = connection
                .query_row(live_root_sql(true), params![digest, 100_i64], |_| Ok(()))
                .optional()
                .expect("probe")
                .is_some();
            if !rooted {
                unrooted.push(digest.to_string());
            }
        }

        connection
            .execute(
                attachment_sql()
                    .manifest_process_owner
                    .delete_aged_uncommitted_proving_process_death
                    .sql(),
                params![100_i64],
            )
            .expect("forget");
        let survivors: Vec<String> = {
            let mut statement = connection
                .prepare("SELECT attachment_id FROM attachment_manifest ORDER BY attachment_id")
                .expect("read survivors");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("rows");
            rows.collect::<Result<_, _>>().expect("survivors")
        };

        unrooted.sort();
        assert_eq!(
            unrooted,
            vec![
                "blake3:aged-host".to_string(),
                "blake3:dead-process".to_string()
            ],
            "an unscoped aged put and a dead process owner are unrooted; a live turn owner is not"
        );
        assert_eq!(
            survivors,
            vec!["blake3:live-turn".to_string()],
            "the sweep forgets exactly the digests the probe reported unrooted"
        );
    }
}
