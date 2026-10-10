//! What a session holds in an earlier kernel version, carried to this
//! build's (kernel spec §6 "Upgrades", ADR 0106 §1).
//!
//! A session holds kernel definitions in two places: the functions it
//! saved, each a document of its own, and the cell its open turn stopped
//! in, whose snapshot holds the cell's document, the run parked under it
//! and the ledger of the waits the run stands on. Both are carried by the
//! kernel migration the process engine carries a process with: a saved
//! function when its session is restored or seeded, and a parked cell when
//! its turn is restored, in the commit that moves the session to this
//! build's format set.

use std::sync::Arc;

use std::collections::BTreeSet;

use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::{Annotations, FunctionId, FunctionRegistry, KERNEL_VERSION, KernelVersion};
use lash_vm_protocol::EncodedPayload;
use lash_vm_runtime::{
    KernelMigrationRefusal, migrate_run, migrate_saved_function, plan_migration,
};

use super::envelope::{envelope_of, stored_checkpoint};

/// What a session's plugin carries a stored kernel definition forward
/// with: the library its workers hold and the kernel version it writes.
#[derive(Clone)]
pub(crate) struct KernelCarry {
    /// The library the workers were assembled with; `None` is lash's own.
    functions: Option<Arc<FunctionRegistry>>,
    writes: KernelVersion,
}

impl Default for KernelCarry {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl KernelCarry {
    /// A carry to `writes`, the newest version when `None`, against
    /// `functions`.
    pub(crate) fn new(
        functions: Option<Arc<FunctionRegistry>>,
        writes: Option<KernelVersion>,
    ) -> Self {
        Self {
            functions,
            writes: writes.unwrap_or(KernelVersion::NEWEST),
        }
    }

    fn library(&self) -> Result<Arc<FunctionRegistry>, KernelMigrationRefusal> {
        match &self.functions {
            Some(functions) => Ok(Arc::clone(functions)),
            None => lash_vm_runtime::standard_functions().map_err(|error| {
                KernelMigrationRefusal::State {
                    message: format!("the build's library does not assemble: {error}"),
                }
            }),
        }
    }

    /// `function` in the kernel version this build's dialects lower a cell
    /// in, or `None` when it is stored in it already. A saved function is
    /// declared in the document of each cell that uses it, so that version
    /// is the one it is held in: [`KERNEL_VERSION`], which the synthetic
    /// successor, changing no dialect, shares with the build before it.
    ///
    /// # Errors
    ///
    /// The typed refusal of a function the migration does not carry.
    pub(crate) fn saved_function(
        &self,
        function: &SavedFunction,
    ) -> Result<Option<SavedFunction>, KernelMigrationRefusal> {
        let found = function.document.manifest.kernel;
        if found == KERNEL_VERSION {
            return Ok(None);
        }
        let Some(lowered) = KernelVersion::of(KERNEL_VERSION) else {
            return Err(KernelMigrationRefusal::State {
                message: format!("this build has no kernel version {KERNEL_VERSION}"),
            });
        };
        migrate_saved_function(function, self.library()?.as_ref(), lowered)
    }

    /// `snapshot`, a cell's stored checkpoint, carried to the kernel
    /// version this plugin writes: its document rewritten, its parked run
    /// carried onto the rewrite and sealed under the same owner, and each
    /// wait of its ledger identified in the rewritten document. `None`
    /// when the cell's document is in that version already, or the
    /// checkpoint holds no cell.
    ///
    /// # Errors
    ///
    /// The typed refusal of a cell the migration does not carry.
    pub(crate) fn cell_snapshot(
        &self,
        snapshot: &str,
    ) -> Result<Option<lash_core::plugin::CarriedCellSnapshot>, KernelMigrationRefusal> {
        let state = |message: String| KernelMigrationRefusal::State { message };
        let mut checkpoint = stored_checkpoint(snapshot).map_err(state)?;
        let Some(mut envelope) = envelope_of(&checkpoint).map_err(state)? else {
            return Ok(None);
        };
        let base = envelope.cell.document().map_err(state)?;
        if base.manifest.kernel >= self.writes.number() {
            return Ok(None);
        }
        let Some(plan) = plan_migration(&base, self.library()?.as_ref())? else {
            return Ok(None);
        };
        if let lash_vm_broker::kernel::CheckpointPhase::Parked {
            state: parked,
            ledger,
        } = &mut checkpoint.phase
        {
            *parked = migrate_run(parked, &base, &plan)?;
            *ledger = ledger
                .identified(|identity| plan.rewritten.effect_identity(&base, identity))
                .map_err(|refusal| KernelMigrationRefusal::Parked {
                    document: plan.from,
                    refusal,
                })?;
        }
        envelope.cell.document = plan
            .rewritten
            .document
            .to_json()
            .map_err(|error| state(error.to_string()))?;
        // Where each statement came from in the source follows its node; a
        // node the rewrite did not keep has no statement to name.
        if let Ok(mut annotations) = serde_json::from_str::<Annotations>(&envelope.cell.annotations)
        {
            annotations.document = plan.rewritten.identity();
            annotations.nodes = annotations
                .nodes
                .into_iter()
                .filter_map(|mut node| {
                    node.site = plan.rewritten.correspondence.successor(&node.site)?.clone();
                    Some(node)
                })
                .collect();
            annotations.nodes.sort_by(|a, b| a.site.cmp(&b.site));
            envelope.cell.annotations =
                serde_json::to_string(&annotations).map_err(|error| state(error.to_string()))?;
        }
        checkpoint.host = Some(EncodedPayload(envelope.encode().map_err(state)?));
        Ok(Some(lash_core::plugin::CarriedCellSnapshot {
            snapshot: serde_json::to_string(&checkpoint)
                .map_err(|error| state(error.to_string()))?,
            executable_identity: plan.rewritten.identity().to_string(),
            format_version: plan.to.number(),
        }))
    }
}

/// Why this build would not carry `snapshot`, a cell's stored checkpoint,
/// to the kernel version it writes, with `functions` as its library; `None`
/// when it would, or the cell needs no carrying. What `lashctl
/// kernel-migration list` asks of the cell a session's open turn stopped in
/// (FIG-5787); it writes nothing.
pub fn cell_migration_refusal(
    snapshot: &str,
    functions: Arc<FunctionRegistry>,
) -> Option<KernelMigrationRefusal> {
    KernelCarry::new(Some(functions), None)
        .cell_snapshot(snapshot)
        .err()
}

/// The library functions the cell `snapshot`, a cell's stored checkpoint,
/// pins: those its document's manifest lists (FIG-5799). `None` when the
/// checkpoint holds no cell or does not decode. What a build that drops a
/// helper release asks of the cell a session's open turn stopped in.
pub fn cell_snapshot_functions(snapshot: &str) -> Option<BTreeSet<FunctionId>> {
    let checkpoint = stored_checkpoint(snapshot).ok()?;
    let envelope = envelope_of(&checkpoint).ok()??;
    let document = envelope.cell.document().ok()?;
    Some(document.manifest.functions.keys().copied().collect())
}
