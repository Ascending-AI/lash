//! What a session holds in an earlier kernel version, carried to this
//! build's (kernel spec §6 "Upgrades", ADR 0106 §1).
//!
//! A session holds kernel definitions in two places: the functions it
//! saved, each a document of its own, and the cell its open turn stopped
//! in, whose execution root holds the cell's document and the run parked
//! under it. Hosts explicitly authorize their proved successor; the same
//! linked checkpoint preparation carries a cell's coordinates, Written and
//! admissions. Saved session functions migrate their own code and heap
//! representation, preserving frozen captures; each fresh cell then relinks
//! them to its current packages (FIG-6926). Parked-frame values retain code.

use std::sync::Arc;

use lash_core::plugin::StoredCell;
use lash_core_store::checkpoint_graph::bodies::{ExecRoot, ExecStatus};
use lash_core_store::checkpoint_graph::{GraphDecoder, NodeKind};
use lash_kernel_doc::{FunctionRegistry, KernelVersion};

use lash_vm_client::packages::WorkerLibrary;
use lash_vm_runtime::KernelMigrationRefusal;

use super::envelope::CellEnvelope;

/// What a session's plugin carries a stored kernel definition forward
/// with: the library its workers hold and the kernel version it writes.
#[derive(Clone)]
pub(crate) struct KernelCarry {
    pub(super) succession: Option<Arc<SessionKernelSuccession>>,
    library: Held,
    writes: KernelVersion,
}

/// A host's explicit approval of a pinned document's format successor.
/// Return the exact rewrite and coordinate proof; no namespace is consulted.
pub type SessionKernelSuccession = dyn Fn(
        &lash_kernel_doc::Document,
        &FunctionRegistry,
    ) -> Result<Option<lash_vm_runtime::PlannedMigration>, KernelMigrationRefusal>
    + Send
    + Sync;

/// What a carry reads library functions from.
#[derive(Clone)]
enum Held {
    /// The packages the host registered: what its workers were assembled
    /// with, and whose sets a cell runs on.
    Release(Arc<WorkerLibrary>),
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
    pub(crate) fn with_succession(
        mut self,
        succession: Option<Arc<SessionKernelSuccession>>,
    ) -> Self {
        self.succession = succession;
        self
    }

    pub(super) fn writes(&self) -> KernelVersion {
        self.writes
    }

    /// A carry to `writes`, the newest version when `None`, against the
    /// host's `packages`, lash's own when `None`.
    pub(crate) fn holding(
        release: Option<Arc<WorkerLibrary>>,
        writes: Option<KernelVersion>,
    ) -> Self {
        Self {
            succession: None,
            library: release.map_or(Held::Lash, Held::Release),
            writes: writes.unwrap_or(KernelVersion::NEWEST),
        }
    }

    pub(crate) fn library(&self) -> Result<Arc<FunctionRegistry>, KernelMigrationRefusal> {
        match &self.library {
            Held::Registry(functions) => Ok(Arc::clone(functions)),
            Held::Release(release) => Ok(Arc::new(release.registry().clone())),
            Held::Lash => Ok(Arc::new(
                lash_vm_releases::helper_release()
                    .map_err(|error| KernelMigrationRefusal::State {
                        message: error.to_string(),
                    })?
                    .functions
                    .clone(),
            )),
        }
    }

    pub(super) fn successor(
        &self,
        document: &lash_kernel_doc::Document,
    ) -> Result<lash_vm_runtime::PlannedMigration, KernelMigrationRefusal> {
        let refused = |reason: &str| KernelMigrationRefusal::NotAdmitted {
            document: document.identity(),
            reason: reason.into(),
        };
        let capability = self
            .succession
            .as_ref()
            .ok_or_else(|| refused("session kernel succession is unavailable"))?;
        let plan = capability(document, self.library()?.as_ref())?
            .ok_or_else(|| refused("session kernel succession has no correspondence"))?;
        let proved = lash_vm_runtime::plan_migration(document, self.library()?.as_ref())?
            .ok_or_else(|| refused("no kernel format correspondence"))?;
        if plan != proved {
            return Err(KernelMigrationRefusal::Document {
                document: document.identity(),
                refusal: lash_kernel_migrate::DocumentRefusal::Correspondence,
            });
        }
        Ok(plan)
    }

    /// Whether `cell`, a cell's stored root, runs on under the kernel
    /// version this plugin writes: an ended cell, or one whose document is
    /// in that version already, does. An earlier parked cell requires a known migration; its stored code
    /// and page continuations are admitted at resume.
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
        if base
            .requires
            .kernels
            .iter()
            .copied()
            .max()
            .unwrap_or(self.writes.number())
            >= self.writes.number()
        {
            return Ok(());
        }
        self.successor(&base).map(|_| ())
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
        succession: None,
        library: Held::Registry(functions),
        writes: KernelVersion::NEWEST,
    }
    .cell_snapshot(cell)
    .err()
}
