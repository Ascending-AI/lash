//! Attachment edges and external-byte write state in the durable-core catalog.
use super::*;
use crate::schema_layout::Schema;
use lash_core_execution::{ArtifactReferrer, AttachmentWrite, ReferrerClaim, SessionReferrerState};
use lash_sansio::sync::MutexExt;
use lash_store_sql::attachment::{
    condemnation::CondemnationStatements, edges::AttachmentEdgeStatements,
    pending_writes::PendingWriteStatements, sweep_clock::SweepClockStatements,
    uploads::UploadStatements,
};
use std::sync::LazyLock;

lash_store_sql::statements! {
    pub(crate) struct AttachmentSqliteStatements @ "attachment_referrer_edge" {
        select_deleting = "SELECT 1 FROM attachment_condemnations WHERE attachment_id = ?1 AND phase = 'deleting'";
        select_condemned = "SELECT 1 FROM attachment_condemnations WHERE attachment_id = ?1";
        insert_condemned = "INSERT INTO attachment_condemnations (attachment_id, phase, sweep_generation) VALUES (?1, 'condemned', ?2)";
        session_state = "SELECT EXISTS (SELECT 1 FROM session_meta WHERE session_id = ?1), EXISTS (SELECT 1 FROM deleted_sessions WHERE session_id = ?1), EXISTS (SELECT 1 FROM graph_nodes WHERE session_id = ?1 AND tombstoned = 0)";
    }
}
pub(crate) struct AttachmentSql {
    pub(crate) edges: AttachmentEdgeStatements,
    pub(crate) pending: PendingWriteStatements,
    pub(crate) uploads: UploadStatements,
    pub(crate) condemnation: CondemnationStatements,
    pub(crate) sqlite: AttachmentSqliteStatements,
    pub(crate) sweep_clock: SweepClockStatements,
}
static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let dialect = Schema::Main.dialect();
    AttachmentSql {
        edges: AttachmentEdgeStatements::render(dialect),
        pending: PendingWriteStatements::render(dialect),
        uploads: UploadStatements::render(dialect),
        condemnation: CondemnationStatements::render(dialect),
        sqlite: AttachmentSqliteStatements::render(dialect),
        sweep_clock: SweepClockStatements::render(dialect),
    }
});
pub(crate) fn attachment_sql() -> &'static AttachmentSql {
    &ATTACHMENT_SQL
}
fn check_kind(referrer: &ArtifactReferrer) -> Result<(), StoreError> {
    if !referrer.kind().holds_attachments() {
        return Err(StoreError::ReferrerKindRefused {
            kind: referrer.kind(),
            store: "attachment",
        });
    }
    ArtifactReferrer::decode(referrer.kind().as_str(), &referrer.canonical_id())
        .map_err(|error| error.into_store_error("attachment referrer"))?;
    Ok(())
}
fn check_fence(tx: &rusqlite::Connection, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
    check_kind(referrer)?;
    if crate::artifact_store::artifact_fenced_tx(tx, referrer).map_err(sqlite_error)? {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: referrer.clone(),
        });
    }
    Ok(())
}
fn insert_edge(
    tx: &rusqlite::Connection,
    referrer: &ArtifactReferrer,
    id: &AttachmentId,
) -> Result<(), StoreError> {
    crate::conn::cached_execute(
        tx,
        attachment_sql().edges.insert.sql(),
        params![
            id.as_str(),
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
pub(crate) fn acquire_attachment_refs_conn(
    tx: &rusqlite::Connection,
    claim: &ReferrerClaim,
    ids: &[AttachmentId],
    now: u64,
) -> Result<(), StoreError> {
    let referrer = claim.referrer();
    check_fence(tx, referrer)?;
    let ids = ids.iter().collect::<std::collections::BTreeSet<_>>();
    for id in &ids {
        let deleting = tx
            .query_row(
                attachment_sql().sqlite.select_deleting.sql(),
                params![id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        let evidenced = tx
            .query_row(
                attachment_sql().uploads.select_evidence.sql(),
                params![id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sqlite_error)?
            .is_some();
        if deleting || !evidenced {
            return Err(StoreError::UnknownAttachment {
                digest: (*id).clone(),
            });
        }
    }
    for id in &ids {
        crate::conn::cached_execute(
            tx,
            attachment_sql()
                .condemnation
                .delete_unclaimed_condemned
                .sql(),
            params![id.as_str()],
        )
        .map_err(sqlite_error)?;
        insert_edge(tx, referrer, id)?;
    }
    if let Some(cleanup) = claim.guard_cleanup() {
        crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now, "core")?;
    }
    Ok(())
}
fn has_permit(
    tx: &rusqlite::Connection,
    write: &AttachmentWrite,
    token: &str,
) -> Result<bool, StoreError> {
    tx.query_row(
        attachment_sql().pending.select_permit.sql(),
        params![
            token,
            write.attachment_id.as_str(),
            write.claim.referrer().kind().as_str(),
            write.claim.referrer().canonical_id()
        ],
        |_| Ok(()),
    )
    .optional()
    .map_err(sqlite_error)
    .map(|row| row.is_some())
}
fn abort_write_conn(
    tx: &rusqlite::Connection,
    write: &AttachmentWrite,
    token: &str,
) -> Result<(), StoreError> {
    if !has_permit(tx, write, token)? {
        return Ok(());
    }
    let referrer = write.claim.referrer();
    crate::conn::cached_execute(
        tx,
        attachment_sql().condemnation.delete_superseded_claim.sql(),
        params![
            write.attachment_id.as_str(),
            token,
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )
    .map_err(sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        attachment_sql().pending.delete_permit.sql(),
        params![
            token,
            write.attachment_id.as_str(),
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )
    .map_err(sqlite_error)?;
    crate::conn::cached_execute(
        tx,
        attachment_sql().edges.delete_unproven_ref.sql(),
        params![
            write.attachment_id.as_str(),
            referrer.kind().as_str(),
            referrer.canonical_id()
        ],
    )
    .map_err(sqlite_error)?;
    Ok(())
}
fn outcome<T>(value: Result<T, StoreError>) -> rusqlite::Result<TxOutcome<Result<T, StoreError>>> {
    Ok(match value {
        Ok(value) => TxOutcome::Commit(Ok(value)),
        Err(error) => TxOutcome::Rollback(Err(error)),
    })
}
impl SqliteStore {
    pub(crate) async fn rooted_attachment_ids(
        &self,
    ) -> Result<std::collections::BTreeSet<AttachmentId>, StoreError> {
        let ids = self
            .conn
            .call(|conn| {
                let mut statement =
                    conn.prepare_cached(attachment_sql().edges.select_rooted_ids.sql())?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        ids.into_iter()
            .map(|id| {
                AttachmentId::parse(&id)
                    .map_err(|error| stored_data_corrupt("attachment root", error))
            })
            .collect()
    }
    pub(crate) async fn has_attachment_root(&self, id: &AttachmentId) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.conn
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        attachment_sql().edges.select_live_root.sql(),
                        params![id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some())
            })
            .await
            .map_err(sqlite_error)
    }
    pub(crate) async fn list_attachment_condemnations(
        &self,
    ) -> Result<Vec<lash_core_execution::AttachmentCondemnationRecord>, StoreError> {
        let rows = self
            .conn
            .call(|conn| {
                let mut statement =
                    conn.prepare_cached(attachment_sql().condemnation.select_all.sql())?;
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        let mut rows = rows
            .into_iter()
            .map(
                |(id, phase, token, kind, referrer, attempts, error, stall)| {
                    let digest = AttachmentId::parse(&id)
                        .map_err(|error| stored_data_corrupt("attachment condemnation", error))?;
                    let write_referrer = match (kind, referrer) {
                        (None, None) => None,
                        (Some(kind), Some(id)) => Some((kind, id)),
                        _ => {
                            return Err(stored_data_corrupt(
                                "attachment condemnation",
                                "incomplete writer referrer",
                            ));
                        }
                    };
                    lash_core_execution::store::decode_attachment_condemnation_record(
                        lash_core_execution::store::StoredAttachmentCondemnation {
                            digest,
                            phase,
                            write_token_present: token.is_some(),
                            write_referrer,
                            delete_attempts: attempts,
                            last_delete_error: error,
                            stall_reason: stall,
                        },
                    )
                },
            )
            .collect::<Result<Vec<_>, StoreError>>()?;
        rows.sort_by(|a, b| a.digest.cmp(&b.digest));
        Ok(rows)
    }
    /// Open one sweep pass: mint the next generation and register it live in
    /// this process. SQLite runs in one process (ADR 0106), so the registry
    /// is the whole liveness proof: a pass that ended, was cancelled, or died
    /// with its process is absent from it.
    pub(crate) async fn begin_attachment_sweep(
        &self,
    ) -> Result<lash_core_execution::AttachmentSweepGeneration, StoreError> {
        let generation = self
            .conn
            .write(|tx| {
                tx.query_row(
                    attachment_sql().sweep_clock.mint_generation.sql(),
                    params![true],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .map_err(sqlite_error)?;
        let generation = u64::try_from(generation).map_err(|_| {
            stored_data_corrupt(
                "attachment sweep clock",
                format!("minted generation {generation} is negative"),
            )
        })?;
        let liveness = LiveSweep::register(self.sweep_catalog_key(), generation);
        Ok(lash_core_execution::AttachmentSweepGeneration::new(
            generation,
            Box::new(liveness),
        ))
    }

    /// Adopt every sweep-owned row an older generation left whose pass is no
    /// longer live, in one `BEGIN IMMEDIATE` transaction.
    pub(crate) async fn adopt_attachment_condemnations(
        &self,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnationAdoption, StoreError> {
        let mine = sweep_generation_sql(generation)?;
        let catalog = self.sweep_catalog_key();
        let now = crate::clamp_epoch_ms(self.clock.timestamp_ms());
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<_, StoreError> = (|| {
                    let rows = {
                        let mut statement = tx
                            .prepare_cached(attachment_sql().condemnation.select_adoptable.sql())
                            .map_err(sqlite_error)?;
                        statement
                            .query_map(params![mine], |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, i64>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, i64>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                    row.get::<_, i64>(5)?,
                                ))
                            })
                            .map_err(sqlite_error)?
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map_err(sqlite_error)?
                    };
                    let mut adoption =
                        lash_core_execution::AttachmentCondemnationAdoption::default();
                    for (digest, owner, phase, delete_attempts, stall_reason, next_delete_at_ms)
                        in rows
                    {
                        let id = AttachmentId::parse(&digest).map_err(|error| {
                            stored_data_corrupt(
                                "attachment condemnation",
                                format!("attachment_id is not a valid attachment id: {error}"),
                            )
                        })?;
                        let stalled = stall_reason
                            .map(|label| {
                                lash_core_execution::AttachmentDeleteStallReason::from_label(&label)
                                    .ok_or_else(|| stored_data_corrupt(
                                        "attachment condemnation",
                                        format!("attachment `{id}` has unknown stall reason `{label}`"),
                                    ))
                            })
                            .transpose()?;
                        let live = LiveSweep::is_live(&catalog, owner);
                        let backing_off = phase != "deleting" && next_delete_at_ms > now;
                        if live || backing_off {
                            if stalled.is_some() {
                                adoption.stalled.push(id.clone());
                            }
                            if live {
                                adoption.held_by_live_pass.push(id);
                            } else if stalled.is_none() {
                                adoption.backing_off.push(id);
                            }
                            continue;
                        }
                        let adopted = crate::conn::cached_execute(
                            tx,
                            attachment_sql().condemnation.adopt.sql(),
                            params![digest, mine, owner, now],
                        )
                        .map_err(sqlite_error)?;
                        if adopted == 1 {
                            adoption.adopted.push(adopted_condemnation(
                                id,
                                &phase,
                                delete_attempts,
                                stalled,
                            )?);
                        }
                    }
                    Ok(adoption)
                })();
                Ok(match outcome {
                    Ok(adoption) => TxOutcome::Commit(Ok(adoption)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    /// The key this catalog's live sweep passes are registered under: the
    /// database target the host opened it at.
    fn sweep_catalog_key(&self) -> String {
        self.location.target().to_string()
    }

    /// `Condemned -> Deleting` under `generation`: the CAS that authorizes the
    /// physical delete. A writer that revoked or claimed the condemnation, or
    /// a pass that adopted it, leaves nothing to match, and the delete is never
    /// issued.
    pub(crate) async fn arm_attachment_delete(
        &self,
        attachment_id: &AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentDeleteArming, StoreError> {
        let generation = sweep_generation_sql(generation)?;
        let attachment_id = attachment_id.as_str().to_string();
        let armed = self
            .conn
            .write(move |tx| {
                crate::conn::cached_execute(
                    tx,
                    attachment_sql().condemnation.arm_delete.sql(),
                    params![attachment_id, generation],
                )
            })
            .await
            .map_err(sqlite_error)?;
        Ok(if armed == 1 {
            lash_core_execution::AttachmentDeleteArming::Armed
        } else {
            lash_core_execution::AttachmentDeleteArming::Revoked
        })
    }

    /// Settle one condemnation `generation` owns. A restoring writer's token is
    /// never cleared here.
    pub(crate) async fn settle_attachment_condemnation(
        &self,
        attachment_id: &AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
        settlement: lash_core_execution::AttachmentCondemnationSettlement,
    ) -> Result<lash_core_execution::AttachmentSettlementOutcome, StoreError> {
        let generation = sweep_generation_sql(generation)?;
        let attachment_id = attachment_id.as_str().to_string();
        let now = crate::clamp_epoch_ms(self.clock.timestamp_ms()).min(i64::MAX - 900_000);
        let settled = self
            .conn
            .write(move |tx| match settlement {
                lash_core_execution::AttachmentCondemnationSettlement::Deleted => {
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().condemnation.delete_armed.sql(),
                        params![attachment_id, generation],
                    )
                }
                lash_core_execution::AttachmentCondemnationSettlement::Spared => {
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().condemnation.delete_spared.sql(),
                        params![attachment_id, generation],
                    )
                }
                lash_core_execution::AttachmentCondemnationSettlement::Failed { stall, error } => {
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().condemnation.record_failed_delete.sql(),
                        params![
                            attachment_id,
                            generation,
                            error,
                            stall.map(|reason| reason.as_str()),
                            now
                        ],
                    )
                }
            })
            .await
            .map_err(sqlite_error)?;
        Ok(if settled == 1 {
            lash_core_execution::AttachmentSettlementOutcome::Applied
        } else {
            lash_core_execution::AttachmentSettlementOutcome::NotOwned
        })
    }

    pub(crate) async fn condemn_attachment(
        &self,
        id: &AttachmentId,
        generation: &lash_core_execution::AttachmentSweepGeneration,
    ) -> Result<lash_core_execution::AttachmentCondemnation, StoreError> {
        let generation = sweep_generation_sql(generation)?;
        let id = id.to_string();
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    if tx
                        .query_row(
                            attachment_sql().edges.select_live_root.sql(),
                            params![id],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some()
                    {
                        return Ok(lash_core_execution::AttachmentCondemnation::RootPresent);
                    }
                    if tx
                        .query_row(
                            attachment_sql().sqlite.select_condemned.sql(),
                            params![id],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some()
                    {
                        return Ok(lash_core_execution::AttachmentCondemnation::AlreadyCondemned);
                    }
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().sqlite.insert_condemned.sql(),
                        params![id, generation],
                    )
                    .map_err(sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().uploads.delete_by_id.sql(),
                        params![id],
                    )
                    .map_err(sqlite_error)?;
                    Ok(lash_core_execution::AttachmentCondemnation::Condemned)
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
    pub(crate) async fn recover_abandoned_attachment_write(
        &self,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        let id = id.clone();
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    let claim = tx
                        .query_row(
                            attachment_sql().condemnation.select_claim.sql(),
                            params![id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    if let Some((token, kind, referrer)) = claim {
                        let referrer = ArtifactReferrer::decode(&kind, &referrer)
                            .map_err(|error| error.into_store_error("attachment pending write"))?;
                        // Abort needs only the referrer; the claim's guard has already been armed.
                        let cleanup = match &referrer {
                            ArtifactReferrer::Execution(_) => {
                                Some(lash_core_execution::ArtifactCleanupPlan::AwaitJournal)
                            }
                            ArtifactReferrer::Upload(_) => Some(
                                lash_core_execution::ArtifactCleanupPlan::AwaitUploadExpiry {
                                    expires_at_ms: 0,
                                },
                            ),
                            _ => None,
                        };
                        let claim = match cleanup {
                            Some(guard) => ReferrerClaim::guarded(referrer, guard),
                            None => ReferrerClaim::unguarded(referrer),
                        }
                        .map_err(|error| error.into_store_error("attachment pending write"))?;
                        abort_write_conn(
                            tx,
                            &AttachmentWrite {
                                attachment_id: id,
                                claim,
                            },
                            &token,
                        )?;
                    }
                    Ok(())
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}
/// The sweep passes running in this process, by catalog and generation.
static LIVE_SWEEPS: LazyLock<std::sync::Mutex<std::collections::HashSet<(String, u64)>>> =
    LazyLock::new(Default::default);

/// One registered live sweep pass. Dropping it — the pass returning or its
/// task being cancelled — removes the registration; a process that dies takes
/// the whole registry with it.
struct LiveSweep {
    catalog: String,
    generation: u64,
}

impl LiveSweep {
    fn register(catalog: String, generation: u64) -> Self {
        LIVE_SWEEPS
            .lock_recover()
            .insert((catalog.clone(), generation));
        Self {
            catalog,
            generation,
        }
    }

    fn is_live(catalog: &str, generation: i64) -> bool {
        u64::try_from(generation).is_ok_and(|generation| {
            LIVE_SWEEPS
                .lock_recover()
                .contains(&(catalog.to_owned(), generation))
        })
    }
}

impl Drop for LiveSweep {
    fn drop(&mut self) {
        LIVE_SWEEPS
            .lock_recover()
            .remove(&(std::mem::take(&mut self.catalog), self.generation));
    }
}

fn sweep_generation_sql(
    generation: &lash_core_execution::AttachmentSweepGeneration,
) -> Result<i64, StoreError> {
    i64::try_from(generation.generation()).map_err(|_| {
        StoreError::Backend(format!(
            "attachment sweep generation {} exceeds the stored range",
            generation.generation()
        ))
    })
}

fn adopted_condemnation(
    digest: AttachmentId,
    phase: &str,
    delete_attempts: i64,
    stalled: Option<lash_core_execution::AttachmentDeleteStallReason>,
) -> Result<lash_core_execution::AdoptedAttachmentCondemnation, StoreError> {
    let phase = match phase {
        "condemned" => lash_core_execution::AttachmentCondemnationPhase::Condemned,
        "deleting" => lash_core_execution::AttachmentCondemnationPhase::Deleting,
        unknown => {
            return Err(stored_data_corrupt(
                "attachment condemnation",
                format!("attachment `{digest}` has unknown phase `{unknown}`"),
            ));
        }
    };
    let delete_attempts = u32::try_from(delete_attempts).map_err(|_| {
        stored_data_corrupt(
            "attachment condemnation",
            format!("attachment `{digest}` has delete attempt count {delete_attempts}"),
        )
    })?;
    Ok(lash_core_execution::AdoptedAttachmentCondemnation {
        digest,
        phase,
        delete_attempts,
        stalled,
    })
}

#[async_trait::async_trait]
impl AttachmentManifest for SqliteStore {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<lash_core_execution::AttachmentWriteFence, StoreError> {
        let write = write.clone();
        let now = self.clock.timestamp_ms();
        let token = lash_core_execution::AttachmentWriteToken::new();
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    let referrer = write.claim.referrer();
                    check_fence(tx, referrer)?;
                    let condemnation = tx
                        .query_row(
                            attachment_sql().condemnation.select_phase_and_claim.sql(),
                            params![write.attachment_id.as_str()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                        )
                        .optional()
                        .map_err(sqlite_error)?;
                    match condemnation
                        .as_ref()
                        .map(|(phase, claim)| (phase.as_str(), claim.is_some()))
                    {
                        Some(("deleting", _)) | Some(("condemned", true)) => {
                            return Ok(
                                lash_core_execution::AttachmentWriteFence::ReclamationInFlight,
                            );
                        }
                        None | Some(("condemned", false)) => {}
                        Some((phase, _)) => {
                            return Err(stored_data_corrupt(
                                "attachment condemnation",
                                format!("unknown phase {phase}"),
                            ));
                        }
                    }
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().pending.insert.sql(),
                        params![
                            token.as_hex(),
                            write.attachment_id.as_str(),
                            referrer.kind().as_str(),
                            referrer.canonical_id(),
                            crate::clamp_epoch_ms(now)
                        ],
                    )
                    .map_err(sqlite_error)?;
                    if condemnation.is_some() {
                        crate::conn::cached_execute(
                            tx,
                            attachment_sql().condemnation.claim_write.sql(),
                            params![write.attachment_id.as_str(), token.as_hex()],
                        )
                        .map_err(sqlite_error)?;
                    }
                    insert_edge(tx, referrer, &write.attachment_id)?;
                    if let Some(cleanup) = write.claim.guard_cleanup() {
                        crate::obligation_ledger::arm_cleanup_tx(tx, &cleanup, now, "core")?;
                    }
                    Ok(lash_core_execution::AttachmentWriteFence::Granted(
                        lash_core_execution::AttachmentWritePermit::new(token),
                    ))
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: lash_core_execution::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let write = write.clone();
        let token = permit.write_id().as_hex();
        let now = crate::clamp_epoch_ms(self.clock.timestamp_ms());
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    if !has_permit(tx, &write, &token)? {
                        return Err(StoreError::StaleWritePermit {
                            digest: write.attachment_id,
                        });
                    }
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().uploads.insert.sql(),
                        params![write.attachment_id.as_str(), now],
                    )
                    .map_err(sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().condemnation.delete_by_write_token.sql(),
                        params![write.attachment_id.as_str(), token],
                    )
                    .map_err(sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().pending.delete_permit.sql(),
                        params![
                            token,
                            write.attachment_id.as_str(),
                            write.claim.referrer().kind().as_str(),
                            write.claim.referrer().canonical_id()
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: lash_core_execution::AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        let write = write.clone();
        let token = permit.write_id().as_hex();
        self.conn
            .write_flow(move |tx| outcome(abort_write_conn(tx, &write, &token)))
            .await
            .map_err(sqlite_error)?
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        let claim = claim.clone();
        let ids = ids.to_vec();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| outcome(acquire_attachment_refs_conn(tx, &claim, &ids, now)))
            .await
            .map_err(sqlite_error)?
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        check_kind(referrer)?;
        let referrer = referrer.clone();
        let id = id.clone();
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    if crate::artifact_store::artifact_fenced_tx(tx, &referrer)
                        .map_err(sqlite_error)?
                    {
                        return Ok(());
                    }
                    if let ArtifactReferrer::Session(session) = &referrer {
                        let retained: bool = tx
                            .query_row(
                                attachment_sql().sqlite.session_state.sql(),
                                params![session.as_str()],
                                |row| row.get(2),
                            )
                            .map_err(sqlite_error)?;
                        if retained {
                            return Ok(());
                        }
                    }
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().edges.delete_ref.sql(),
                        params![
                            id.as_str(),
                            referrer.kind().as_str(),
                            referrer.canonical_id()
                        ],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn end_attachment_referrer(&self, referrer: &ArtifactReferrer) -> Result<(), StoreError> {
        check_kind(referrer)?;
        let referrer = referrer.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                outcome((|| {
                    crate::artifact_store::fence_artifact_referrer_tx(tx, &referrer, now)
                        .map_err(sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().pending.delete_referrer.sql(),
                        params![referrer.kind().as_str(), referrer.canonical_id()],
                    )
                    .map_err(sqlite_error)?;
                    crate::conn::cached_execute(
                        tx,
                        attachment_sql().edges.delete_referrer.sql(),
                        params![referrer.kind().as_str(), referrer.canonical_id()],
                    )
                    .map_err(sqlite_error)?;
                    Ok(())
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
    async fn session_referrer_state(
        &self,
        session: &SessionId,
    ) -> Result<SessionReferrerState, StoreError> {
        let session = session.clone();
        self.conn
            .call(move |conn| {
                conn.query_row(
                    attachment_sql().sqlite.session_state.sql(),
                    params![session.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, bool>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                        ))
                    },
                )
            })
            .await
            .map_err(sqlite_error)
            .map(|(metadata, deleted, retained)| {
                if !metadata && !deleted {
                    SessionReferrerState::Absent
                } else if metadata && !deleted {
                    SessionReferrerState::Live
                } else if retained {
                    SessionReferrerState::DeletedRetained
                } else {
                    SessionReferrerState::DeletedRetired
                }
            })
    }
    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        let id = id.clone();
        let rows = self
            .conn
            .call(move |conn| {
                let mut statement =
                    conn.prepare_cached(attachment_sql().edges.select_referrers.sql())?;
                statement
                    .query_map(params![id.as_str()], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .map_err(sqlite_error)?;
        rows.into_iter()
            .map(|(kind, id)| {
                ArtifactReferrer::decode(&kind, &id)
                    .map_err(|error| error.into_store_error("attachment referrer edge"))
            })
            .collect()
    }
}
