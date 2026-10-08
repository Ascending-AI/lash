//! Once-only incorporation of logical tool facts.
//!
//! One operation — [`RuntimeExecutionContext::incorporate_tool_facts`] —
//! applies every logical semantic channel exactly once per
//! [`SettlementSource`]: possession is granted. It never executes a
//! declaration, never emits a delivery, never re-runs a projector, and never
//! meters spend: hosts meter provider attempts (ADR 0127).
//!
//! Idempotence is carried, not hoped for: [`IncorporationLedger`] records the
//! incorporated sources and travels with the execution context wherever
//! `started_process_ids` does, so a redrive or a segment handover cannot
//! incorporate the same settlement twice.

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::execution_context::RuntimeExecutionContext;
use crate::ProcessId;
use crate::runtime::effect::executor::RuntimeEffectControllerError;

pub use lash_core_store::effect_opener::{IncorporationLedger, SettlementSource};

/// What one incorporation applied, in counts. A source already in the ledger
/// returns every count as zero — the second call is the no-op the ledger
/// exists to make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Incorporated {
    pub source: Option<SettlementSource>,
    pub possession: Vec<ProcessId>,
}

impl<'run> RuntimeExecutionContext<'run> {
    /// ADR 0099 §6: applies a settlement's recorded semantic deltas
    /// exactly once. Never executes a declaration, never emits a delivery,
    /// never re-runs a projector. A `source` already in the ledger returns an
    /// [`Incorporated`] whose counts are all zero.
    pub fn incorporate_tool_facts(
        &self,
        source: SettlementSource,
        possession: &[ProcessId],
    ) -> Result<Incorporated, RuntimeEffectControllerError> {
        let mut ledger = self.incorporation_ledger().lock_recover();
        if ledger.incorporated.contains(&source) {
            return Ok(Incorporated {
                source: Some(source),
                ..Incorporated::default()
            });
        }
        // The caller supplies the identities realized by recorded declarations.
        self.restore_started_process_ids(possession);
        ledger.incorporated.insert(source.clone());
        Ok(Incorporated {
            source: Some(source),
            possession: possession.to_vec(),
        })
    }

    /// The once-only ledger this context incorporates against. Behind a
    /// method so `incorporate_tool_facts` and the handover carriage both
    /// reach the same `Arc`.
    pub(crate) fn incorporation_ledger(&self) -> &Arc<std::sync::Mutex<IncorporationLedger>> {
        &self.incorporation_ledger
    }
}
