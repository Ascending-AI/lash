//! What a session holds in an earlier kernel version, carried to this
//! build's (kernel spec §6 "Upgrades", ADR 0106 §1).
//!
//! A session holds kernel definitions in two places: the functions it
//! saved, each a document of its own, and the cell its open turn stopped
//! in, whose execution root holds the cell's document and the run parked
//! under it. A saved function is carried by the kernel migration the
//! process engine carries a document with, when a cell is lowered against
//! it. A parked cell's run is not: its heap pages are carried by no
//! migrator this build ships, so the cell is refused, typed, and stays
//! parked (FIG-6193).

use std::sync::Arc;

use lash_core::plugin::StoredCell;
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{GraphDecoder, NodeKind};
use lash_kernel_dialect::SavedFunction;
use lash_kernel_doc::{FunctionRegistry, KERNEL_VERSION, KernelVersion};
use lash_kernel_package::HeldPackages;
use lash_vm_runtime::{
    KernelMigrationRefusal, ParkedRefusal, migrate_saved_function, plan_migration,
};

use super::envelope::CellEnvelope;

/// What a session's plugin carries a stored kernel definition forward
/// with: the library its workers hold and the kernel version it writes.
#[derive(Clone)]
pub(crate) struct KernelCarry {
    library: Held,
    writes: KernelVersion,
}

/// What a carry reads library functions from.
#[derive(Clone)]
enum Held {
    /// The packages the host registered: what its workers were assembled
    /// with, and whose sets a cell runs on.
    Packages(Arc<HeldPackages>),
    /// A registry alone: what a survey of stored cells reads.
    Registry(Arc<FunctionRegistry>),
    /// lash's own packages.
    Lash,
}

impl Default for KernelCarry {
    fn default() -> Self {
        Self::holding(None, None)
    }
}

impl KernelCarry {
    /// A carry to `writes`, the newest version when `None`, against the
    /// host's `packages`, lash's own when `None`.
    pub(crate) fn holding(
        packages: Option<Arc<HeldPackages>>,
        writes: Option<KernelVersion>,
    ) -> Self {
        Self {
            library: packages.map_or(Held::Lash, Held::Packages),
            writes: writes.unwrap_or(KernelVersion::NEWEST),
        }
    }

    pub(crate) fn library(&self) -> Result<Arc<FunctionRegistry>, KernelMigrationRefusal> {
        match &self.library {
            Held::Registry(functions) => Ok(Arc::clone(functions)),
            Held::Packages(_) | Held::Lash => self
                .packages()
                .map(|packages| Arc::clone(packages.functions()))
                .map_err(|message| KernelMigrationRefusal::State { message }),
        }
    }

    /// The packages a cell's set is viewed in.
    ///
    /// # Errors
    ///
    /// lash's packages do not assemble, or the carry reads a registry
    /// alone.
    pub(crate) fn packages(&self) -> Result<Arc<HeldPackages>, String> {
        match &self.library {
            Held::Packages(packages) => Ok(Arc::clone(packages)),
            Held::Lash => lash_vm_runtime::lash_packages()
                .map_err(|error| format!("the build's library does not assemble: {error}")),
            Held::Registry(_) => Err("a survey's carry holds no packages".to_owned()),
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

    /// Whether `cell`, a cell's stored root, runs on under the kernel
    /// version this plugin writes: an ended cell, or one whose document is
    /// in that version already, does. A parked cell an earlier version
    /// wrote does not: no migrator this build ships carries its heap pages
    /// (FIG-6193). It stays parked, its rows intact, and resumes on a build
    /// that runs its kernel.
    ///
    /// # Errors
    ///
    /// The typed refusal of a cell the migration does not carry.
    pub(crate) fn cell_snapshot(&self, cell: &StoredCell) -> Result<(), KernelMigrationRefusal> {
        let state = |message: String| KernelMigrationRefusal::State { message };
        let graph =
            |error: lash_core_store::checkpoint_graph::GraphRefusal| state(error.to_string());
        let mut decoder = GraphDecoder::new(&cell.graph, cell.read_limits);
        let body: ExecRoot = decoder
            .decode(cell.row.root, NodeKind::EXEC_ROOT)
            .map_err(graph)?;
        if body.status != ExecStatus::Parked {
            return Ok(());
        }
        let Some(envelope) = body.envelope else {
            return Err(state("a parked cell root lacks its envelope".to_owned()));
        };
        let carried_cell = CellEnvelope::load(&mut decoder, envelope).map_err(graph)?;
        let base = carried_cell.document;
        if base.manifest.kernel >= self.writes.number() {
            return Ok(());
        }
        match plan_migration(&base, self.library()?.as_ref())? {
            None => Ok(()),
            Some(plan) => Err(KernelMigrationRefusal::Parked {
                document: plan.from,
                refusal: ParkedRefusal::StateNotCarried {
                    detail: "a parked cell's heap pages are carried by no migrator this \
                             build ships; it stays parked and resumes on a build that runs \
                             its kernel"
                        .to_owned(),
                },
            }),
        }
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
    KernelCarry {
        library: Held::Registry(functions),
        writes: KernelVersion::NEWEST,
    }
    .cell_snapshot(cell)
    .err()
}
