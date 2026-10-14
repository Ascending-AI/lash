//! Session ownership of paged cell roots. A speculative cell keeps its base
//! retained until the fenced publication settles. Cancellation drops only the
//! speculative root; it cannot replace the acknowledged session root.
use lash_core_store::checkpoint_graph::CommitId;
use lash_durable::domain::pages::CandidateReceipt;
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_doc::{KERNEL_VERSION, NumberPolicy};
use lash_kernel_state::paged::{PageSource, SessionRoot};
use std::sync::Arc;

mod graph;
/// The reserved history projection binding.
pub(crate) const HISTORY_BINDING: &str = "history";

/// A session owns roots, never exported objects or per-binding identity ranges.
#[derive(Clone, Debug)]
pub(crate) struct SessionBindings {
    pub(crate) root: SessionRoot,
}

/// The public session control descriptor, small regardless of retained state.
#[derive(Clone, Debug)]
pub struct CodeModeExecutionState {
    dialect: Arc<str>,
    numbers: NumberPolicy,
    bindings: Option<SessionBindings>,
    cells: u64,
    committed: Option<PageRef>,
    active: Option<CellVersion>,
}
#[derive(Clone, Debug)]
struct CellVersion {
    base: Option<SessionBindings>,
    base_page: Option<PageRef>,
    base_cells: u64,
    candidate: Option<(PageRef, CommitId)>,
    response_returned: bool,
}
impl CodeModeExecutionState {
    pub(crate) fn new(dialect: impl Into<Arc<str>>, numbers: NumberPolicy) -> Self {
        Self {
            dialect: dialect.into(),
            numbers,
            bindings: None,
            cells: 0,
            committed: None,
            active: None,
        }
    }
    pub fn open(
        page: PageRef,
        dialect: &str,
        numbers: NumberPolicy,
        source: &mut dyn PageSource,
    ) -> Result<Self, HeapFault> {
        let descriptor = graph::read(source, page)?;
        if descriptor.dialect != dialect
            || descriptor.numbers != numbers
            || descriptor.kernel != KERNEL_VERSION
        {
            return Err(HeapFault::Malformed(
                "session dialect, number policy or kernel mismatch".into(),
            ));
        }
        Ok(Self {
            dialect: descriptor.dialect.into(),
            numbers,
            bindings: Some(SessionBindings {
                root: descriptor.session,
            }),
            cells: descriptor.cells,
            committed: Some(page),
            active: None,
        })
    }
    pub(crate) fn dialect(&self) -> &str {
        &self.dialect
    }
    pub fn session(&self) -> Option<SessionRoot> {
        self.bindings.as_ref().map(|b| b.root)
    }
    pub fn committed_root(&self) -> Option<PageRef> {
        self.committed
    }
    /// Forks share immutable pages; each subsequent cell owns its own heap root.
    pub fn fork(&self) -> Self {
        let (bindings, cells, committed) = match &self.active {
            Some(active) => (active.base.clone(), active.base_cells, active.base_page),
            None => (self.bindings.clone(), self.cells, self.committed),
        };
        Self {
            dialect: Arc::clone(&self.dialect),
            numbers: self.numbers,
            bindings,
            cells,
            committed,
            active: None,
        }
    }
    pub(crate) fn begin_code_execution(&mut self) -> Result<(), HeapFault> {
        if self.active.is_some() {
            return Err(HeapFault::Malformed("a cell is already speculative".into()));
        }
        self.active = Some(CellVersion {
            base: self.bindings.clone(),
            base_page: self.committed,
            base_cells: self.cells,
            candidate: None,
            response_returned: false,
        });
        Ok(())
    }
    /// Accepts the worker's sealed carry without copying or changing identities.
    /// Publication must retain this session root in the same S6 transaction as
    /// its terminal and process ownership rows.
    pub fn stage_cell(
        &mut self,
        session: SessionRoot,
        commit: CommitId,
        source: &mut dyn PageSource,
    ) -> Result<PageRef, HeapFault> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| HeapFault::Malformed("no active cell".into()))?;
        if active.candidate.is_some() {
            return Err(HeapFault::Malformed(
                "the candidate is awaiting publication resolution".into(),
            ));
        }
        let cells = self
            .cells
            .checked_add(1)
            .ok_or_else(|| HeapFault::Malformed("cell count overflow".into()))?;
        let candidate = graph::seal(
            source,
            &graph::SessionDescriptor {
                dialect: self.dialect.to_string(),
                numbers: self.numbers,
                kernel: KERNEL_VERSION,
                cells,
                session,
            },
        )?;
        self.bindings = Some(SessionBindings { root: session });
        self.cells = cells;
        self.active
            .as_mut()
            .ok_or_else(|| HeapFault::Malformed("active cell disappeared".into()))?
            .candidate = Some((candidate, commit));
        Ok(candidate)
    }
    pub fn pending_root(&self) -> Option<PageRef> {
        self.active
            .as_ref()
            .and_then(|a| a.candidate.map(|(page, _)| page))
    }
    pub(crate) fn mark_code_execution_response_returned(&mut self) {
        if let Some(active) = &mut self.active {
            active.response_returned = true;
        }
    }
    /// The caller resolves the exact candidate under S6's commit identity.
    /// An ambiguous publication keeps both base and candidate retained.
    pub fn publication_committed(&mut self, receipt: &CandidateReceipt) -> Result<(), HeapFault> {
        let candidate = self
            .active
            .as_ref()
            .and_then(|a| a.candidate)
            .ok_or_else(|| HeapFault::Malformed("no candidate awaits publication".into()))?;
        if candidate.0.id.0 != receipt.root.0 || candidate.1 != receipt.commit {
            return Err(HeapFault::Malformed(
                "receipt names another cell candidate or commit".into(),
            ));
        }
        self.committed = Some(candidate.0);
        self.active = None;
        Ok(())
    }
    pub(crate) fn rollback_code_execution(&mut self) -> Result<(), HeapFault> {
        let Some(active) = self.active.take() else {
            return Ok(());
        };
        if active.candidate.is_some() {
            self.active = Some(active);
            return Err(HeapFault::Malformed(
                "resolve publication before dropping a candidate".into(),
            ));
        }
        self.bindings = active.base;
        self.committed = active.base_page;
        self.cells = active.base_cells;
        Ok(())
    }
    /// Authoritative refusal or cancellation before publication drops the
    /// speculative version and keeps the acknowledged pages owned.
    pub fn publication_refused(&mut self) {
        if let Some(active) = self.active.take() {
            self.bindings = active.base;
            self.committed = active.base_page;
            self.cells = active.base_cells;
        }
    }
    pub fn execution_state_dirty(&self) -> bool {
        self.active.is_some()
    }
}
