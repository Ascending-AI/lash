//! A process's engine state carried forward from an earlier build's format
//! at its claim (ADR 0106 §1, §2).
//!
//! An engine registered with an
//! [`EngineStateMigration`](crate::EngineStateMigration) decodes the formats
//! it carries beside its own, so its node claims a process the previous
//! build left in one. The claimer carries the state to the engine's own
//! format as its first commit, before any transition, once every live node
//! that serves the process decodes the newer set. Until then it advances
//! the process in the format it is in, so a node of the previous build can
//! still take it back.

use lash_durable::domain::{ExecKey, ProcessActorRow, ProcessWrite, SnapshotRev, SnapshotWrite};
use lash_durable::runner::Owned;
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, DurableReads, FormatSet, StoreFailure,
    StoreFailureKind,
};

use super::activation::{Pass, ProcessActivation, corrupt, hex_decode, hex_encode};
use super::driver::Driver;
use super::terminal::ProcessParkReason;
use crate::runtime::process::engine_state::{EngineState, EngineStateFormat};
use crate::{ProcessEngine, ProcessId};

/// What the claim-time step did with the pass's transaction.
pub(super) enum Carry {
    /// It committed the carried state, or parked the process: the pass is
    /// over.
    Ended(Pass),
    /// Nothing to carry now: the pass goes on over the state as written.
    AsWritten(Box<ActorTx>),
}

impl ProcessActivation {
    /// Carries `state`, written in a format `engine` carries forward, to
    /// the engine's own, when the fleet permits it.
    #[expect(
        clippy::too_many_arguments,
        reason = "one step of the pass, over what the pass has read"
    )]
    pub(super) async fn carry_forward(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        row: &ProcessActorRow,
        driver: &Driver,
        engine: &dyn ProcessEngine,
        state: &EngineState,
        snapshot_rev: Option<SnapshotRev>,
    ) -> Result<Carry, DurableError> {
        let written = state.format.clone();
        let declared = engine.state_format();
        if written == declared || owned.draining() {
            return Ok(Carry::AsWritten(Box::new(tx)));
        }
        let kind = engine.kind();
        let (Some(migration), Some(newer)) = (
            self.backend.state_migration(kind).cloned(),
            self.backend.formats().process(kind).cloned(),
        ) else {
            return Ok(Carry::AsWritten(Box::new(tx)));
        };
        if !self.fleet_decodes(owned, &written, &newer).await? {
            return Ok(Carry::AsWritten(Box::new(tx)));
        }
        let reason = match migration.migrate(process, state).await {
            Ok(Some(carried)) if carried.format == declared => {
                tx.write(DomainWrite::Process(ProcessWrite::Advance {
                    process: process.clone(),
                    expected_rev: row.state_rev,
                    driver_json: driver.encode(),
                }));
                tx.stamp_formats(newer);
                tx.write(DomainWrite::Snapshot(SnapshotWrite::Put {
                    exec: ExecKey::Process(process.clone()),
                    expected: snapshot_rev,
                    snapshot_ref: hex_encode(&carried.bytes),
                    executable_identity: carried.format.kind.clone(),
                    format_version: carried.format.version,
                }));
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Carry::Ended(Pass::Again));
            }
            Ok(Some(carried)) => ProcessParkReason::AdvanceRefused {
                message: format!(
                    "engine carried its state to format {:?}, declared {declared:?}",
                    carried.format
                ),
            },
            // Not at a point its engine carries it from: it goes on as
            // written, and is asked again at its next pass.
            Ok(None) => return Ok(Carry::AsWritten(Box::new(tx))),
            Err(refusal) if refusal.fatal => ProcessParkReason::MigrationRefused {
                kind: written.kind,
                version: written.version,
                refusal: refusal.refusal,
                message: refusal.message,
            },
            Err(refusal) => {
                return Err(DurableError::Store(StoreFailure {
                    kind: StoreFailureKind::Unavailable,
                    message: format!(
                        "carrying a process's engine state forward: {}",
                        refusal.message
                    ),
                }));
            }
        };
        self.park(owned, tx, &reason).await.map(Carry::Ended)
    }

    /// Whether every live node that decodes the set of a state in
    /// `written` also decodes `newer`: the fleet-format gate (ADR 0106 §2).
    /// While a node of the build that wrote the state is live, the state
    /// stays as that build reads it.
    async fn fleet_decodes(
        &self,
        owned: &Owned,
        written: &EngineStateFormat,
        newer: &FormatSet,
    ) -> Result<bool, DurableError> {
        let Some(older) = self.backend.formats().process_in(written).cloned() else {
            return Ok(false);
        };
        let live = owned.store().live_decodes().await?;
        let candidates = [newer.clone(), older];
        Ok(lash_durable::fleet_writable(&candidates, &live) == Some(newer))
    }
}

/// The engine state `process` holds, in the format it was written in, read
/// without a fence; `None` before its first transition and after its end.
/// For an operator's survey: an owner never decides anything from it.
///
/// # Errors
///
/// The store's, and a stored state that is not lowercase hex.
pub async fn stored_engine_state(
    reads: &dyn DurableReads,
    process: &ProcessId,
) -> Result<Option<EngineState>, DurableError> {
    let Some(snapshot) = reads.snapshot(&ExecKey::Process(process.clone())).await? else {
        return Ok(None);
    };
    let bytes = hex_decode(&snapshot.snapshot_ref)
        .ok_or_else(|| corrupt("a process's engine state", "it is not lowercase hex"))?;
    Ok(Some(EngineState {
        format: EngineStateFormat {
            kind: snapshot.executable_identity,
            version: snapshot.format_version,
        },
        bytes,
    }))
}
