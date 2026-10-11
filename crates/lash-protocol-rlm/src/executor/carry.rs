//! What a session holds in an earlier kernel version, carried to this
//! build's (kernel spec §6 "Upgrades", ADR 0106 §1).
//!
//! A session holds kernel definitions in two places: the functions it
//! saved, each a document of its own, and the cell its open turn stopped
//! in, whose execution root holds the cell's document, the run parked
//! under it and the admissions of the waits the run stands on. Both are carried by the
//! kernel migration the process engine carries a process with: a saved
//! function when its session is restored or seeded, and a parked cell when
//! its turn is restored, in the commit that moves the session to this
//! build's format set.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::plugin::{CarriedCell, StoredCell};
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{
    EncodeBudget, ExplicitLimit, GraphDecoder, GraphStaging, NodeId, NodeKind,
};
use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::{Annotations, FunctionId, FunctionRegistry, KERNEL_VERSION, KernelVersion};
use lash_vm_broker::kernel::graph::{EffectMembership, RootParts, reidentify, stage_exec_root};
use lash_vm_broker::kernel::state::WholeState;
use lash_vm_runtime::{
    KernelMigrationRefusal, migrate_run, migrate_saved_function, plan_migration,
};

use super::envelope::{CellEnvelope, EntryProjections, READ_LIMITS};
use super::host_graph::{CallStatus, HostState};

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

    /// `cell`, a cell's stored root, carried to the kernel version this
    /// plugin writes: its document rewritten, its parked run carried onto
    /// the rewrite under the build's helper release, and each wait it
    /// stands on re-admitted under the identity the rewrite gives it, in a
    /// new complete root. `None` when the cell's document is in that
    /// version already, or the cell has ended.
    ///
    /// # Errors
    ///
    /// The typed refusal of a cell the migration does not carry.
    pub(crate) fn cell_snapshot(
        &self,
        cell: &StoredCell,
    ) -> Result<Option<CarriedCell>, KernelMigrationRefusal> {
        let state = |message: String| KernelMigrationRefusal::State { message };
        let graph =
            |error: lash_core_store::checkpoint_graph::GraphRefusal| state(error.to_string());
        let mut decoder = GraphDecoder::new(&cell.graph, READ_LIMITS);
        let body: ExecRoot = decoder
            .decode(cell.row.root, NodeKind::EXEC_ROOT)
            .map_err(graph)?;
        if body.status != ExecStatus::Parked {
            return Ok(None);
        }
        let (Some(membership), Some(host), Some(envelope)) =
            (body.effect_membership, body.host, body.envelope)
        else {
            return Err(state(
                "a parked cell root lacks its membership, host or envelope".to_owned(),
            ));
        };
        let (mut carried_cell, _) = CellEnvelope::load(&mut decoder, envelope).map_err(graph)?;
        let base = carried_cell.document().map_err(state)?;
        if base.manifest.kernel >= self.writes.number() {
            return Ok(None);
        }
        let Some(plan) = plan_migration(&base, self.library()?.as_ref())? else {
            return Ok(None);
        };
        let entries = super::processes::CellEntries {
            identity: plan.from,
            document: base.clone(),
        };
        let (membership, resolved) = EffectMembership::load(
            &mut decoder,
            membership,
            &cell.exec,
            &EntryProjections(&entries),
        )
        .map_err(graph)?;
        let refused = |refusal| KernelMigrationRefusal::Parked {
            document: plan.from,
            refusal,
        };
        let mut admitted = Vec::new();
        let mut identities = BTreeMap::new();
        for entry in &membership.entries {
            if entry.status != lash_vm_broker::kernel::graph::MemberStatus::Pending {
                continue;
            }
            let resolved = resolved
                .get(&entry.effect)
                .ok_or_else(|| state(format!("no admission of wait {:?}", entry.effect)))?;
            admitted.push(lash_kernel_state::AdmittedWait {
                identity: entry.effect.clone(),
                request: resolved
                    .request
                    .admitted(resolved.admission.effect_name.as_ref())
                    .map_err(graph)?,
            });
            identities.insert(
                entry.effect.clone(),
                plan.rewritten
                    .effect_identity(&base, &entry.effect)
                    .map_err(refused)?,
            );
        }
        let parked = WholeState::parked_in(&cell.graph, body.state, body.format, &READ_LIMITS)
            .map_err(graph)?;
        let releases = lash_vm_broker::kernel::state::release_manifests()
            .map_err(|failure| state(failure.to_string()))?;
        let release = releases
            .manifests
            .current()
            .ok_or_else(|| state("the build holds no helper release".to_owned()))?;
        let whole = migrate_run(&parked, &admitted, &base, &plan, release)?;
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let reidentified = reidentify(
            &mut staging,
            &cell.exec,
            &membership,
            &resolved,
            &identities,
        )
        .map_err(graph)?;
        // Each open call's summary names its wait's new identity and
        // admission; a settled call keeps the admission its outcome names.
        let mut host_state = HostState::load(&mut decoder, host).map_err(graph)?;
        let summaries = host_state.calls.items().to_vec();
        for summary in summaries {
            if summary.status != CallStatus::Pending {
                continue;
            }
            if let Some((effect, admission)) = reidentified.admissions.get(&summary.effect) {
                let ordinal = summary.ordinal;
                host_state.calls.replace(
                    ordinal,
                    super::host_graph::CallSummary {
                        effect: effect.clone(),
                        admission: *admission,
                        ..summary
                    },
                );
            }
        }
        let host = host_state
            .stage(&mut staging, &mut BTreeSet::new())
            .map_err(graph)?;
        carried_cell.document = plan
            .rewritten
            .document
            .to_json()
            .map_err(|error| state(error.to_string()))?;
        // Where each statement came from in the source follows its node; a
        // node the rewrite did not keep has no statement to name.
        if let Ok(mut annotations) = serde_json::from_str::<Annotations>(&carried_cell.annotations)
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
            carried_cell.annotations =
                serde_json::to_string(&annotations).map_err(|error| state(error.to_string()))?;
        }
        let release_node = NodeId(release.0);
        let envelope = carried_cell
            .stage(&mut staging, release_node)
            .map_err(graph)?;
        let (_, root_write) = stage_exec_root(
            &mut staging,
            &cell.exec,
            Some(&reidentified.membership),
            RootParts {
                state: whole.root,
                status: ExecStatus::Parked,
                executable_identity: plan.rewritten.identity().to_string(),
                format: plan.to.number(),
                host: Some(host),
                envelope: Some(envelope),
            },
            (Some(cell.row.rev), Some(cell.row.root)),
            lash_core_store::checkpoint_graph::CommitId::mint(),
        )
        .map_err(graph)?;
        let (name, release_nodes) = releases
            .pinned(release_node)
            .ok_or_else(|| state("the build does not hold its own helper release".to_owned()))?;
        let mut nodes = whole.nodes;
        nodes.extend(release_nodes);
        nodes.extend(staging.into_nodes());
        let mut writes = reidentified.writes;
        writes.push(lash_core::durable_port::DomainWrite::ReleasePin(
            lash_core::durable_port::domain::graph::ReleasePinWrite::Install {
                release: name,
                root: release_node,
            },
        ));
        writes.push(root_write);
        Ok(Some(CarriedCell { nodes, writes }))
    }
}

/// Why this build would not carry `cell`, a cell's stored root, to the
/// kernel version it writes, with `functions` as its library; `None` when
/// it would, or the cell needs no carrying. What `lashctl
/// kernel-migration list` asks of the cell a session's open turn stopped in
/// (FIG-5787); it writes nothing.
pub fn cell_migration_refusal(
    cell: &StoredCell,
    functions: Arc<FunctionRegistry>,
) -> Option<KernelMigrationRefusal> {
    KernelCarry::new(Some(functions), None)
        .cell_snapshot(cell)
        .err()
}

/// The library functions the cell `cell`, a cell's stored root, pins:
/// those its document's manifest lists (FIG-5799). `None` when the root
/// does not decode. What a build that drops a helper release asks of the
/// cell a session's open turn stopped in.
pub fn cell_snapshot_functions(cell: &StoredCell) -> Option<BTreeSet<FunctionId>> {
    let loaded = super::envelope::load_cell(&cell.graph, cell.row.root).ok()?;
    let document = loaded.cell.document().ok()?;
    Some(document.manifest.functions.keys().copied().collect())
}
