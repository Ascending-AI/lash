//! One session's execution state: its bindings (`K-SES-001`), as pages of
//! the persistent heap (FIG-6193).
//!
//! What a session carries from one cell to the next is the carry its last
//! cell's machine sealed: the heap, the bindings `main` left at its top
//! level and the functions it saved (S8), each a page root. The parent
//! never copies the state out: it holds the session's descriptor
//! ([`graph`]), a page naming those roots, and reads a binding's value only
//! where a host asks for it, through the [`SessionEditor`].
//!
//! The cells of a turn stage what they seal in the turn's one spool. A
//! cell's end is staged there under a new descriptor; a capture is that
//! descriptor, the commit that names it publishes every page the spool
//! holds under it as its candidate (FIG-6418), and its acknowledgement
//! settles them. A binding a cell left that the session
//! does not carry (`K-SES-003`) is listed by name with why, until a later
//! cell binds the name again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::SessionError;
use lash_core::plugin::SessionPages;
use lash_core_store::checkpoint_graph::stage::{ManifestEntry, StageFault, StageSpool};
use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_core_store::execution_state::{
    ExecutionGraphCapture, ExecutionGraphPurpose, HydratedExecutionGraph,
};
use lash_durable::domain::pages::RootLease;
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, FunctionId, KERNEL_VERSION, Name, NumberPolicy};
use lash_kernel_state::paged::{KeyedRoot, PageAnswer, PageBody, PageSource, SessionRoot};
use lash_kernel_vm::{SessionEditor, SessionValue};
use lash_vm_broker::kernel::pages::{ParentSeal, SharedSpool, SpoolRoot, answer_page};
use lash_vm_client::service::LowerFunction;

use super::pages::{PageAccessError, SessionPageAccess};
use super::snapshot::SessionOpenError;

mod graph;
mod notes;

/// The reserved binding the history projection answers; no cell binds it.
pub(crate) const HISTORY_BINDING: &str = "history";

/// The most manifest entries one read of a capture's candidate takes.
const MANIFEST_BATCH: usize = 1024;

/// One version of the session: its descriptor and what the descriptor
/// states.
#[derive(Clone, Debug)]
struct Version {
    /// The descriptor page; none for a session that holds nothing.
    descriptor: Option<NodeId>,
    /// Its bytes, which a host that hydrates the state is handed.
    bytes: Option<Arc<[u8]>>,
    session: Option<SessionRoot>,
    cells: u64,
    /// The session's notes tree, keyed by name beside the bindings
    /// (`notes`): unbounded data lives on pages, never in the descriptor.
    notes: KeyedRoot,
    /// The library functions the session's saved functions pin, together
    /// (FIG-5799): bounded by the build's library, never by the session.
    pins: BTreeSet<FunctionId>,
}

impl Default for Version {
    fn default() -> Self {
        Self {
            notes: KeyedRoot::Empty,
            ..Self::zeroed()
        }
    }
}

impl Version {
    fn zeroed() -> Self {
        Self {
            descriptor: None,
            bytes: None,
            session: None,
            cells: 0,
            notes: KeyedRoot::Empty,
            pins: BTreeSet::new(),
        }
    }
}

/// What the parent read of a version, kept until a cell changes it.
#[derive(Clone, Debug, Default)]
struct View {
    /// Each binding's value, as the prompt lists it.
    values: BTreeMap<Name, SessionValue>,
    /// Each saved function's description; none until a cell or a prompt
    /// needs them.
    functions: Option<BTreeMap<Name, LowerFunction>>,
    /// The saved functions this build's kernel does not carry, with why:
    /// listed as not carried and never lowered.
    refused: BTreeMap<Name, String>,
    /// Every name the session binds, loaded from the binding tree: `Some`
    /// once read or known (a fresh session binds nothing), `None` after a
    /// restore until a call loads it.
    names: Option<BTreeSet<Name>>,
    /// Each name's notes, loaded with the names.
    notes: Option<BTreeMap<Name, notes::Note>>,
}

/// The session as it stood before the active cell.
struct Active {
    current: Version,
    defaults: BTreeMap<Name, Datum>,
    view: View,
}

/// A capture staged and not yet acknowledged.
#[derive(Clone, Debug)]
struct Pending {
    root: NodeId,
    /// The spool candidate built for the capture's commit, once one was.
    commit: Option<CommitId>,
    purpose: ExecutionGraphPurpose,
    /// The helper releases the capture's commit pins.
    releases: BTreeSet<NodeId>,
    /// The held runs whose carries the capture owns once committed.
    runs: usize,
}

/// What a finished cell changed in the session's bindings.
pub(super) struct CellChanges {
    pub bindings: lash_core::BindingChanges,
}

/// One code mode session's execution state: its pages, the dialect its
/// cells are written in, and the bookkeeping its captures are built from.
pub struct CodeModeExecutionState {
    dialect: Arc<str>,
    numbers: NumberPolicy,
    current: Version,
    /// The descriptor the store holds as of the last acknowledged capture
    /// or restore.
    installed: Option<NodeId>,
    /// The helper releases an acknowledged capture installed: a saved
    /// function's record names the release its code calls into, and a
    /// capture sends that release's closure until a commit installs it.
    releases: BTreeSet<NodeId>,
    pending: Option<Pending>,
    /// Whether the state as it stands is a turn's working state no session
    /// revision publishes yet: the turn's final capture owes it even when
    /// nothing changed since a working capture was acknowledged.
    final_owed: bool,
    active: Option<Active>,
    response_returned: bool,
    /// Host bindings set by default (`set_default`) and not yet sealed
    /// into the session: each binds its name only where nothing does.
    defaults: BTreeMap<Name, Datum>,
    /// Names a read-only projected binding shadows: never listed as the
    /// session's.
    protected: BTreeSet<Name>,
    view: View,
    pages: SessionPageAccess,
    spools: Option<Result<Arc<SpoolRoot>, StageFault>>,
    kernel: super::KernelCarry,
}

impl std::fmt::Debug for CodeModeExecutionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeModeExecutionState")
            .field("dialect", &self.dialect)
            .field("current", &self.current.descriptor)
            .field("installed", &self.installed)
            .field("pending", &self.pending)
            .field("pages", &self.pages)
            .finish_non_exhaustive()
    }
}

fn protocol(error: impl std::fmt::Display) -> SessionError {
    SessionError::Protocol(format!("code mode session state: {error}"))
}

impl CodeModeExecutionState {
    /// The state of a session that has run no cell, whose cells are written
    /// in `dialect` and decode a host's bare numbers by `numbers`.
    pub(crate) fn new(dialect: impl Into<Arc<str>>, numbers: NumberPolicy) -> Self {
        Self {
            dialect: dialect.into(),
            numbers,
            current: Version::default(),
            installed: None,
            releases: BTreeSet::new(),
            pending: None,
            final_owed: false,
            active: None,
            response_returned: false,
            defaults: BTreeMap::new(),
            protected: BTreeSet::from([Name::new(HISTORY_BINDING)]),
            view: View {
                names: Some(BTreeSet::new()),
                notes: Some(BTreeMap::new()),
                ..View::default()
            },
            pages: SessionPageAccess::default(),
            spools: None,
            kernel: super::KernelCarry::default(),
        }
    }

    /// This state, carrying saved functions forward with `kernel`.
    #[must_use]
    pub(crate) fn carrying(mut self, kernel: super::KernelCarry) -> Self {
        self.kernel = kernel;
        self
    }

    /// This state, staging its turns' pages in spools `spools` opens.
    #[must_use]
    pub(crate) fn staging(mut self, spools: Result<Arc<SpoolRoot>, StageFault>) -> Self {
        self.spools = Some(spools);
        self
    }

    /// Reads the session's committed pages through `cache`, the process's.
    pub(crate) fn caching(
        mut self,
        cache: lash_vm_broker::kernel::pages::cache::PageCache,
    ) -> Self {
        self.pages.caching(cache);
        self
    }

    /// The dialect the session recorded at its creation.
    pub(crate) fn dialect(&self) -> &str {
        &self.dialect
    }

    /// Takes `pages` as where the session's committed state stands.
    pub(crate) fn note_pages(&mut self, pages: Option<&SessionPages>) {
        self.pages.note(pages);
    }

    /// Lease writes the session's reads made: one per acquisition or
    /// renewal.
    #[cfg(test)]
    pub(crate) fn lease_writes(&self) -> u64 {
        self.pages.lease_writes()
    }

    /// Pages the session's reads fetched from the store.
    #[cfg(test)]
    pub(crate) fn page_fetches(&self) -> u64 {
        self.pages.fetches()
    }

    /// The turn's spool, opened on first use.
    pub(super) fn turn_spool(&mut self) -> Result<SharedSpool, PageAccessError> {
        let spools = match &self.spools {
            Some(Ok(spools)) => Arc::clone(spools),
            Some(Err(fault)) => return Err(PageAccessError::Staging(fault.clone())),
            None => return Err(PageAccessError::NoStaging),
        };
        Ok(self.pages.spool(&spools)?)
    }

    /// The lease a cell's run reads the session's committed pages under:
    /// none when the store holds none of them.
    pub(super) async fn lease(&mut self) -> Result<Option<RootLease>, PageAccessError> {
        self.pages.lease().await
    }

    /// The actor whose committed state the session's pages are.
    pub(super) fn actor(&self) -> Option<&lash_durable::ActorKey> {
        self.pages.actor()
    }

    /// The carried session a cell starts from.
    pub(super) fn session(&self) -> Option<SessionRoot> {
        self.current.session
    }

    /// The pages a cell's run may name without reading them: its carried
    /// session's roots.
    pub(super) fn session_references(&self) -> Vec<NodeId> {
        let Some(session) = &self.current.session else {
            return Vec::new();
        };
        std::iter::once(session.heap)
            .chain(session.bindings.page())
            .chain(session.functions.page())
            .map(|page| NodeId(page.id.0))
            .collect()
    }

    /// Every name the session binds, data or function, a host default
    /// included: what a cell is lowered against. Reads the loaded names
    /// cache: callers that may follow a restore load it first
    /// ([`Self::load_names`]).
    pub(crate) fn names(&self) -> BTreeSet<Name> {
        self.view
            .names
            .iter()
            .flatten()
            .chain(self.defaults.keys())
            .filter(|name| !self.protected.contains(*name))
            .cloned()
            .collect()
    }

    /// Every name the session binds, loading the names cache first: the
    /// live catalog check reads it before any turn (FIG-5824).
    ///
    /// # Errors
    ///
    /// The store's faults reading the session's pages.
    pub(crate) async fn session_names(&mut self) -> Result<BTreeSet<Name>, PageAccessError> {
        self.load_names().await?;
        Ok(self.names())
    }

    /// The bindings not carried, each with why.
    pub(crate) fn not_carried(&self) -> BTreeMap<Name, String> {
        let mut not_carried: BTreeMap<Name, String> = self
            .view
            .notes
            .iter()
            .flatten()
            .filter_map(|(name, note)| {
                note.not_carried
                    .as_ref()
                    .map(|why| (name.clone(), why.clone()))
            })
            .collect();
        not_carried.extend(self.view.refused.clone());
        not_carried
    }

    /// Whether the session binds `name` in its stored state: the names
    /// cache, loaded or not.
    fn bound(&self, name: &Name) -> bool {
        self.view
            .names
            .as_ref()
            .is_some_and(|names| names.contains(name))
    }

    /// The binding tree's index pages breadth-first into the read cache,
    /// in [`FETCH_BATCH`] batches: loading `names` then costs O(tree
    /// depth) store round trips, never one per page.
    async fn prefetch_binding_tree(&mut self) -> Result<(), PageAccessError> {
        let Some(page) = self
            .current
            .session
            .as_ref()
            .and_then(|session| session.bindings.page())
        else {
            return Ok(());
        };
        self.pages.prefetch_tree(NodeId(page.id.0)).await
    }

    /// Loads the names and notes caches from the session's pages: a row
    /// per name an earlier cell's not-carried reason or a saved function's
    /// freezing cell is recorded for.
    async fn load_names(&mut self) -> Result<(), PageAccessError> {
        if self.view.names.is_some() && self.view.notes.is_some() {
            return Ok(());
        }
        self.prefetch_binding_tree().await?;
        let session = self.current.session;
        let notes_root = self.current.notes;
        let names = self
            .pages
            .read(move |source| SessionEditor::open(session.as_ref(), source)?.names())
            .await?;
        let notes = self
            .pages
            .read(move |source| notes::read(&notes_root, source))
            .await?;
        self.view.names = Some(names);
        self.view.notes = Some(notes);
        Ok(())
    }

    /// Seals the host's pending defaults into the session, each only where
    /// nothing binds its name. Reads the bindings' pages on each name's
    /// path.
    async fn seal_defaults(&mut self) -> Result<(), PageAccessError> {
        if self.defaults.is_empty() {
            return Ok(());
        }
        let spool = self.turn_spool()?;
        let defaults = self.defaults.clone();
        let session = self.current.session;
        let (sealed, bound) = self
            .pages
            .read(move |source| {
                let mut editor = SessionEditor::open(session.as_ref(), source)?;
                let mut bound = Vec::new();
                for (name, value) in &defaults {
                    if editor.get(name)?.is_none() {
                        editor.set(name, value)?;
                        bound.push(name.clone());
                    }
                }
                Ok((
                    if editor.edited() {
                        Some(editor.seal()?)
                    } else {
                        None
                    },
                    bound,
                ))
            })
            .await?;
        self.defaults.clear();
        if let Some(sealed) = sealed {
            let mut next = self.current.clone();
            next.session = Some(sealed);
            if let Some(names) = &mut self.view.names {
                names.extend(bound.iter().cloned());
            }
            // A name a default binds is carried now: its not-carried note
            // dies.
            if !bound.is_empty() {
                let root = self.current.notes;
                let carried = bound.clone();
                next.notes = self
                    .pages
                    .read(move |source| {
                        notes::edit(&root, source, |edit| {
                            for name in &carried {
                                edit.note(name, |record| {
                                    record.not_carried = None;
                                })?;
                            }
                            Ok(())
                        })
                    })
                    .await?;
                if let Some(notes) = &mut self.view.notes {
                    for name in &bound {
                        if let Some(note) = notes.get_mut(name) {
                            note.not_carried = None;
                            if note.saved.is_none() {
                                notes.remove(name);
                            }
                        }
                    }
                }
            }
            self.current = self.seal_descriptor(next, spool)?;
        }
        Ok(())
    }

    fn seal_descriptor(
        &self,
        mut version: Version,
        mut spool: SharedSpool,
    ) -> Result<Version, PageAccessError> {
        let Some(session) = version.session else {
            version.descriptor = None;
            version.bytes = None;
            return Ok(version);
        };
        let page = graph::seal(
            &mut ParentSeal(&mut spool),
            &graph::SessionDescriptor {
                dialect: self.dialect.to_string(),
                numbers: self.numbers,
                kernel: KERNEL_VERSION,
                cells: version.cells,
                session,
                notes: version.notes,
                pins: version.pins.clone(),
            },
        )?;
        let id = NodeId(page.id.0);
        version.bytes = Some(spool.read(&id)?);
        version.descriptor = Some(id);
        Ok(version)
    }

    /// Readies the session for a cell: the host's defaults sealed in, the
    /// saved functions' descriptions read. `pages` is where the turn says
    /// the committed state stands.
    pub(super) async fn prepare_cell(
        &mut self,
        pages: Option<&SessionPages>,
    ) -> Result<(), PageAccessError> {
        self.note_pages(pages);
        self.seal_defaults().await?;
        self.load_names().await?;
        self.load_functions().await
    }

    /// The saved functions a cell is lowered against, each in this build's
    /// kernel version.
    pub(super) fn lowering_functions(&self) -> BTreeMap<Name, LowerFunction> {
        self.view.functions.clone().unwrap_or_default()
    }

    async fn load_functions(&mut self) -> Result<(), PageAccessError> {
        if self.view.functions.is_some() {
            return Ok(());
        }
        let Some(session) = self.current.session else {
            self.view.functions = Some(BTreeMap::new());
            return Ok(());
        };
        let metas = self
            .pages
            .read(move |source| SessionEditor::open(Some(&session), source)?.functions())
            .await?;
        let mut functions = BTreeMap::new();
        for (name, meta) in metas {
            match self.lower_function(&name, &meta) {
                Ok(function) => {
                    functions.insert(name, function);
                }
                Err(why) => {
                    self.view.refused.insert(name, why);
                }
            }
        }
        self.view.functions = Some(functions);
        Ok(())
    }

    /// A saved function's description as a cell is lowered against it,
    /// carried to this build's kernel version.
    fn lower_function(&self, name: &Name, meta: &[u8]) -> Result<LowerFunction, String> {
        let function =
            LowerFunction::from_description(meta, lash_kernel_doc::KernelDataLimits::standard())
                .map_err(|error| format!("the saved function `{name}` does not decode: {error}"))?;
        match self.kernel.saved_function(&function.clone().into_saved()) {
            Ok(None) => Ok(function),
            Ok(Some(carried)) => Ok(LowerFunction {
                document: lash_kernel_doc::IdentifiedDocument::new(carried.document),
                ..function
            }),
            Err(refusal) => Err(refusal.to_string()),
        }
    }

    /// Marks the start of a cell: the session as it stands is what a
    /// cancelled or refused cell goes back to.
    pub(super) fn begin_code_execution(&mut self) {
        debug_assert!(
            !self.response_returned,
            "a returned code execution must be settled before another cell starts"
        );
        self.withdraw_pending();
        self.active = Some(Active {
            current: self.current.clone(),
            defaults: self.defaults.clone(),
            view: self.view.clone(),
        });
        self.response_returned = false;
    }

    pub(crate) fn prepare_runtime_code_execution(&mut self) -> Result<(), &'static str> {
        if self.active.is_none() {
            return Ok(());
        }
        if self.response_returned {
            return Err("the previous code execution response has not been settled");
        }
        self.rollback_code_execution();
        Ok(())
    }

    pub(crate) fn mark_code_execution_response_returned(&mut self) {
        self.response_returned = true;
    }

    pub(crate) fn accept_code_execution(&mut self) {
        self.active = None;
        self.response_returned = false;
    }

    /// Goes back to the session as it stood before the active cell. What
    /// the cell staged stays in the turn's spool, unreferenced.
    pub(crate) fn rollback_code_execution(&mut self) {
        self.response_returned = false;
        let Some(active) = self.active.take() else {
            return;
        };
        self.current = active.current;
        self.defaults = active.defaults;
        self.view = active.view;
    }

    /// Cancellation rolls the execution back; the cell's parked state is
    /// its execution's, not the session's.
    pub(crate) fn terminate_code_execution(&mut self) {
        self.rollback_code_execution();
    }

    /// Takes the carry a finished cell sealed into the turn's spool: the
    /// session's next version, under a descriptor staged beside it. Reads
    /// only the pages of the bindings and saved functions the cell
    /// changed. Functions remain the saved bundles the VM settled; no
    /// definition publication or binding conversion happens here.
    pub(super) async fn stage_cell(
        &mut self,
        carry: SessionRoot,
        not_carried: Vec<(Name, String)>,
        run: Option<super::pages::HeldRun>,
    ) -> Result<CellChanges, PageAccessError> {
        if let Some(run) = run {
            self.pages.hold(run);
        }
        self.stage_carry(carry, not_carried).await
    }

    async fn stage_carry(
        &mut self,
        carry: SessionRoot,
        not_carried: Vec<(Name, String)>,
    ) -> Result<CellChanges, PageAccessError> {
        let spool = self.turn_spool()?;
        let (old_bindings, old_functions) = match &self.current.session {
            Some(session) => (session.bindings, session.functions),
            None => (KeyedRoot::Empty, KeyedRoot::Empty),
        };
        let (bindings, functions) = self
            .pages
            .read(move |source| {
                let editor = SessionEditor::open(Some(&carry), source)?;
                let bindings = editor.changed(&old_bindings, &carry.bindings)?;
                let functions = editor.changed_functions(&old_functions, &carry.functions)?;
                Ok((bindings, functions))
            })
            .await?;
        let cells = self.current.cells.saturating_add(1);
        let mut next = Version {
            descriptor: None,
            bytes: None,
            session: Some(carry),
            cells,
            ..self.current.clone()
        };
        let mut changes = lash_core::BindingChanges::default();
        let names = &mut self.view.names;
        for (name, (was, is)) in &bindings {
            self.view.values.remove(name);
            match (was, is) {
                (false, true) => changes.added.push(name.to_string()),
                (true, true) => changes.changed.push(name.to_string()),
                (true, false) => changes.removed.push(name.to_string()),
                (false, false) => continue,
            }
            if let Some(names) = names {
                if *is {
                    names.insert(name.clone());
                } else {
                    names.remove(name);
                }
            }
        }
        let mut not_carried_notes = Vec::new();
        for (name, why) in not_carried {
            if !self.bound(&name) {
                changes.not_carried.push(name.to_string());
                // A name the session bound before and not now was not
                // carried, never removed.
                changes
                    .removed
                    .retain(|removed| removed.as_str() != name.as_str());
                not_carried_notes.push((name, why));
            }
        }
        let mut saved_notes = Vec::new();
        for (name, meta) in functions {
            self.view.refused.remove(&name);
            let lowered = meta.map(|meta| self.lower_function(&name, &meta));
            let view = self.view.functions.get_or_insert_with(BTreeMap::new);
            match lowered {
                Some(Ok(function)) => {
                    saved_notes.push((name.clone(), Some(cells)));
                    view.insert(name, function);
                }
                Some(Err(why)) => {
                    saved_notes.push((name.clone(), None));
                    view.remove(&name);
                    self.view.refused.insert(name, why);
                }
                None => {
                    saved_notes.push((name.clone(), None));
                    view.remove(&name);
                }
            }
        }
        // The notes tree, edited beside the bindings: a bound name's
        // not-carried note dies, a not-carried name records why, a saved
        // function records the cell that froze it.
        let notes_root = self.current.notes;
        let bound_names: Vec<Name> = bindings
            .iter()
            .filter(|(_, (_, is))| *is)
            .map(|(name, _)| name.clone())
            .collect();
        let edit_bound = bound_names.clone();
        let edit_not_carried = not_carried_notes.clone();
        let edit_saved = saved_notes.clone();
        next.notes = self
            .pages
            .read(move |source| {
                notes::edit(&notes_root, source, |edit| {
                    for name in &edit_bound {
                        edit.note(name, |record| {
                            record.not_carried = None;
                        })?;
                    }
                    for (name, why) in &edit_not_carried {
                        let why = edit.name_text(why)?;
                        edit.note(name, |record| {
                            record.not_carried = Some(why.clone());
                        })?;
                    }
                    for (name, saved) in &edit_saved {
                        edit.note(name, |record| {
                            record.saved = *saved;
                        })?;
                    }
                    Ok(())
                })
            })
            .await?;
        if let Some(notes) = &mut self.view.notes {
            for name in &bound_names {
                if let Some(note) = notes.get_mut(name) {
                    note.not_carried = None;
                    if note.saved.is_none() {
                        notes.remove(name);
                    }
                }
            }
            for (name, why) in &not_carried_notes {
                notes.entry(name.clone()).or_default().not_carried = Some(why.clone());
            }
            for (name, saved) in &saved_notes {
                let note = notes.entry(name.clone()).or_default();
                note.saved = *saved;
                if note.saved.is_none() && note.not_carried.is_none() {
                    notes.remove(name);
                }
            }
        }
        // The next descriptor's pins are the union over the lowered
        // functions' libraries (FIG-5799).
        if let Some(functions) = &self.view.functions {
            next.pins = functions
                .values()
                .flat_map(|function| function.document.manifest.functions.keys().copied())
                .collect();
        }
        self.current = self.seal_descriptor(next, spool)?;
        if self.current.descriptor.is_none() {
            return Err(PageAccessError::Heap(HeapFault::Malformed(
                "a staged session has no descriptor".into(),
            )));
        }
        Ok(CellChanges { bindings: changes })
    }

    /// The descriptor a cell's recorded end names, staged in the turn's
    /// spool so the end publishes it: the session as it stands, an empty
    /// one sealed for a session that holds nothing yet.
    ///
    /// # Errors
    ///
    /// The spool's faults.
    pub(super) fn end_descriptor(&mut self) -> Result<NodeId, PageAccessError> {
        let spool = self.turn_spool()?;
        let mut version = self.current.clone();
        if version.session.is_none() {
            let empty = SessionEditor::open(None, Box::new(SpoolSeal(spool.clone())))?;
            version.session = Some(empty.seal()?);
        }
        self.current = self.seal_descriptor(version, spool)?;
        self.current.descriptor.ok_or_else(|| {
            PageAccessError::Heap(HeapFault::Malformed(
                "a sealed session has no descriptor".into(),
            ))
        })
    }

    pub fn execution_state_dirty(&self) -> bool {
        self.pending.is_some()
            || self.final_owed
            || self.current.descriptor != self.installed
            || !self.defaults.is_empty()
    }

    /// The session's state as one capture (FIG-6193): its descriptor. The
    /// pages the turn's spool holds under it are the candidate of the
    /// commit that names it ([`Self::execution_candidate`]); they stay
    /// staged until the capture is acknowledged, and a refused commit
    /// publishes them with the next capture.
    pub async fn capture_execution_graph(
        &mut self,
        purpose: ExecutionGraphPurpose,
    ) -> Result<ExecutionGraphCapture, SessionError> {
        self.seal_defaults().await.map_err(protocol)?;
        self.withdraw_pending();
        let Some(graph) = self.descriptor() else {
            return Ok(ExecutionGraphCapture::Clear);
        };
        self.pending = Some(Pending {
            root: graph.root,
            commit: None,
            purpose,
            releases: BTreeSet::new(),
            runs: self.pages.held(),
        });
        Ok(ExecutionGraphCapture::Replace(graph))
    }

    /// The session's descriptor as it stands, if it holds one.
    fn descriptor(&self) -> Option<HydratedExecutionGraph> {
        match (self.current.descriptor, self.current.bytes.clone()) {
            (Some(root), Some(descriptor)) => Some(HydratedExecutionGraph { root, descriptor }),
            _ => None,
        }
    }

    /// The candidate that publishes the pending capture's pages under
    /// `commit` (FIG-6418), when the capture is `root`'s and the turn's
    /// spool holds it: a candidate of the same capture built before is
    /// withdrawn first. Every helper release the pages name that no
    /// acknowledged capture installed is pinned in `tx`, after `reads` of
    /// whether the store pins it already. None when the store holds
    /// `root`'s pages.
    ///
    /// # Errors
    ///
    /// The spool's faults, or a release the pages name that this build
    /// does not hold.
    pub(crate) async fn execution_candidate(
        &mut self,
        root: NodeId,
        commit: CommitId,
        tx: &mut lash_durable::ActorTx,
        reads: &dyn lash_durable::DurableReads,
    ) -> Result<Option<lash_core::plugin::ExecutionCandidate>, SessionError> {
        let Some(mut spool) = self.pages.staged().cloned() else {
            return Ok(None);
        };
        let Some(pending) = self.pending.as_mut().filter(|pending| pending.root == root) else {
            return Ok(None);
        };
        if !spool.contains(&root) {
            return Ok(None);
        }
        if let Some(previous) = pending.commit.take() {
            let _ = spool.withdraw(previous);
        }
        let mut manifest = spool.candidate(root, None, commit).map_err(protocol)?;
        pending.commit = Some(commit);
        let mut named = BTreeSet::new();
        let mut after = None;
        loop {
            let entries = manifest.entries(after, MANIFEST_BATCH).map_err(protocol)?;
            let Some(last) = entries.last() else {
                break;
            };
            after = Some(match last {
                ManifestEntry::External(id) => *id,
                ManifestEntry::Fresh(node) => node.id,
            });
            named.extend(entries.into_iter().filter_map(|entry| match entry {
                ManifestEntry::External(id) => Some(id),
                ManifestEntry::Fresh(_) => None,
            }));
        }
        let mut releases = BTreeSet::new();
        if let Some(held) = held_releases() {
            for release in named.difference(&self.releases) {
                if held.pinned(*release).is_none() {
                    continue;
                }
                held.pin(reads, tx, *release, true)
                    .await
                    .map_err(protocol)?;
                releases.insert(*release);
            }
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.releases = releases;
        }
        Ok(Some(lash_core::plugin::ExecutionCandidate { manifest }))
    }

    /// The session's descriptor, alone: its pages are the store's or the
    /// turn's, and read where a host asks for them.
    pub async fn hydrated_execution_graph(&self) -> Result<HydratedExecutionGraph, SessionError> {
        self.descriptor().ok_or_else(|| {
            SessionError::Protocol("the code mode session holds no execution state".to_owned())
        })
    }

    /// Settles the acknowledged capture: the pages it published leave the
    /// spool, and a final one ends the turn's staging and lease.
    pub(crate) async fn acknowledge_execution_state_capture(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.installed = Some(pending.root);
        self.releases.extend(pending.releases);
        self.pages.release_runs(pending.runs).await;
        if let (Some(commit), Some(spool)) = (pending.commit, self.pages.staged()) {
            let _ = spool.clone().settle(commit);
        }
        // A working capture's commit roots the state under its turn; only
        // a final one publishes it as the session's.
        self.final_owed = pending.purpose == ExecutionGraphPurpose::Working;
        if pending.purpose == ExecutionGraphPurpose::Final && self.active.is_none() {
            self.pages.end_turn();
            self.pages.release().await;
        }
    }

    pub(crate) fn abort_execution_state_capture(&mut self) {
        self.withdraw_pending();
    }

    /// Forgets the capture staged and not acknowledged, whose commit was
    /// not made: its pages stay staged, and its candidate leaves the turn's
    /// spool free for the next one.
    fn withdraw_pending(&mut self) {
        if let Some(Pending {
            commit: Some(commit),
            ..
        }) = self.pending.take()
            && let Some(spool) = self.pages.staged()
        {
            let _ = spool.clone().withdraw(commit);
        }
    }

    /// Installs a stored descriptor (FIG-6193): the working state a turn
    /// phase owns, or a session revision's, as `purpose` says, before any
    /// model continuation or cell compiles. A working state is one the
    /// turn's final capture still owes.
    ///
    /// # Errors
    ///
    /// A descriptor recorded under another dialect, number policy or
    /// kernel, or one that does not decode. Nothing is adopted then.
    pub fn restore_execution_graph(
        &mut self,
        graph: &HydratedExecutionGraph,
        purpose: ExecutionGraphPurpose,
    ) -> Result<(), SessionOpenError> {
        let read = stored_descriptor(graph)?;
        if read.dialect != self.dialect.as_ref() {
            return Err(SessionOpenError::Dialect {
                expected: self.dialect.to_string(),
                found: read.dialect,
            });
        }
        if read.numbers != self.numbers {
            return Err(SessionOpenError::Numbers {
                expected: self.numbers,
                found: read.numbers,
            });
        }
        if read.kernel != KERNEL_VERSION {
            return Err(SessionOpenError::Kernel {
                expected: KERNEL_VERSION,
                found: read.kernel,
            });
        }
        self.pages
            .preload([(graph.root, Arc::clone(&graph.descriptor))]);
        self.current = Version {
            descriptor: Some(graph.root),
            bytes: Some(Arc::clone(&graph.descriptor)),
            session: Some(read.session),
            cells: read.cells,
            notes: read.notes,
            pins: read.pins,
        };
        self.installed = Some(graph.root);
        self.final_owed = purpose == ExecutionGraphPurpose::Working;
        self.withdraw_pending();
        self.active = None;
        self.response_returned = false;
        self.defaults.clear();
        self.view = View::default();
        Ok(())
    }

    /// Takes `protected_names` as the read-only projected names: a binding
    /// one shadows is never listed as the session's, and the next cell,
    /// which binds the name to the host's value, carries none under it.
    pub async fn prune_protected_globals(
        &mut self,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        self.protected = protected_names
            .iter()
            .map(|name| Name::new(name.as_str()))
            .chain(std::iter::once(Name::new(HISTORY_BINDING)))
            .collect();
        self.defaults
            .retain(|name, _| !self.protected.contains(name));
        Ok(())
    }

    /// Applies a `set_default` patch as one transaction: every key is
    /// checked before any is held, and a name the session already binds
    /// keeps its value. The defaults are sealed into the session where its
    /// pages are next readable: the next cell, or the next capture.
    pub async fn patch_globals(
        &mut self,
        patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
        protected_names: &BTreeSet<String>,
    ) -> Result<(), SessionError> {
        for key in patch.set_default.keys() {
            if key == HISTORY_BINDING || protected_names.contains(key) {
                return Err(SessionError::Protocol(format!(
                    "`{key}` is a read-only projected host binding; choose a different variable name for `set_default`"
                )));
            }
        }
        for (key, value) in &patch.set_default {
            let name = Name::new(key.as_str());
            if self.bound(&name) || self.defaults.contains_key(&name) {
                continue;
            }
            let datum = crate::cell_value::json_datum(value, self.numbers);
            self.defaults.insert(name, datum);
        }
        Ok(())
    }

    /// The session's bound variables but `exclude` and its saved
    /// functions: a host resource handle as itself, which stays opaque,
    /// and every other value as the host reads it. Reads the value of each
    /// binding a cell changed since the last listing.
    pub(crate) async fn bound_variable_values(
        &mut self,
        exclude: &BTreeSet<String>,
        pages: Option<&SessionPages>,
    ) -> Result<Vec<(String, crate::session_globals::SessionGlobalKind)>, SessionError> {
        self.note_pages(pages);
        self.load_names().await.map_err(protocol)?;
        self.load_functions().await.map_err(protocol)?;
        let listed: Vec<Name> = self
            .names()
            .into_iter()
            .filter(|name| !exclude.contains(name.as_str()) && !self.saved_function(name))
            .collect();
        // A name the store binds is read: its own value lists before any
        // pending default of the name.
        let unread: Vec<Name> = listed
            .iter()
            .filter(|name| !self.view.values.contains_key(*name) && self.bound(name))
            .cloned()
            .collect();
        if !unread.is_empty()
            && let Some(session) = self.current.session
        {
            let read = self
                .pages
                .read(move |source| {
                    let mut editor = SessionEditor::open(Some(&session), source)?;
                    let mut read = Vec::with_capacity(unread.len());
                    for name in &unread {
                        if let Some(value) = editor.get(name)? {
                            read.push((name.clone(), value));
                        }
                    }
                    Ok(read)
                })
                .await
                .map_err(protocol)?;
            self.view.values.extend(read);
        }
        Ok(listed
            .into_iter()
            .filter_map(|name| {
                let value = match self.view.values.get(&name) {
                    Some(value) => value.clone(),
                    None => match self.defaults.get(&name) {
                        Some(datum) => SessionValue::Data(datum.clone()),
                        None => return None,
                    },
                };
                let kind = match value {
                    SessionValue::Data(Datum::Handle(handle)) => {
                        crate::session_globals::SessionGlobalKind::Handle {
                            kind: handle.kind.clone(),
                            id: handle.id.clone(),
                        }
                    }
                    SessionValue::Data(datum) => crate::session_globals::SessionGlobalKind::Data(
                        crate::cell_value::datum_json(&datum),
                    ),
                    SessionValue::Opaque => return None,
                };
                Some((name.to_string(), kind))
            })
            .collect())
    }

    /// Whether `name` is a saved function of the session's.
    fn saved_function(&self, name: &Name) -> bool {
        self.view
            .notes
            .as_ref()
            .and_then(|notes| notes.get(name))
            .is_some_and(|note| note.saved.is_some())
    }

    /// Each saved function the session holds, with its signature and the
    /// cell whose end froze what it reads.
    pub(crate) fn held_functions(&self) -> Vec<(String, String, Option<u64>)> {
        let described = self.view.functions.as_ref();
        self.view
            .notes
            .iter()
            .flatten()
            .filter(|(name, note)| note.saved.is_some() && !self.protected.contains(*name))
            .map(|(name, note)| {
                let signature = described
                    .and_then(|functions| functions.get(name))
                    .and_then(|function| function.written.as_ref())
                    .map(|written| written.signature.clone())
                    .unwrap_or_else(|| "(...)".to_owned());
                (name.to_string(), signature, note.saved)
            })
            .collect()
    }
}

/// A fresh session's pages: sealed into the turn's spool, and none to
/// read.
struct SpoolSeal(SharedSpool);

impl PageSource for SpoolSeal {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        Err(HeapFault::Missing {
            page: pages.first().map_or(PageId([0; 32]), |page| page.id),
        })
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        ParentSeal(&mut self.0).seal(page)
    }
}

/// The descriptor a stored state's root page holds.
fn stored_descriptor(
    graph: &HydratedExecutionGraph,
) -> Result<graph::SessionDescriptor, SessionOpenError> {
    let mut source = StoredPages(graph);
    Ok(graph::read(
        &mut source,
        PageRef {
            id: PageId(graph.root.0),
            grant: PageGrant::NONE,
        },
    )?)
}

/// The descriptor of a stored state the host was handed, read as it is.
struct StoredPages<'a>(&'a HydratedExecutionGraph);

impl PageSource for StoredPages<'_> {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        pages
            .iter()
            .map(|page| {
                if NodeId(page.id.0) != self.0.root {
                    return Err(HeapFault::Missing { page: page.id });
                }
                answer_page(*page, &self.0.descriptor, |_| Ok(PageGrant::NONE))
            })
            .collect()
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        Err(HeapFault::Malformed(format!(
            "a stored session state is read, never sealed into (a {:?} page)",
            page.kind
        )))
    }
}

/// The library functions the session's saved functions pin, together:
/// the union `state`, the stored execution state of the session, records
/// (FIG-5799): what a build that drops a helper release asks of an idle
/// session.
///
/// # Errors
///
/// The refusal of a state that is not one this build reads.
pub fn saved_function_pins(
    state: &HydratedExecutionGraph,
) -> Result<BTreeSet<FunctionId>, SessionOpenError> {
    Ok(stored_descriptor(state)?.pins)
}

/// The stored binding tree's root, of the execution state a stored
/// session's descriptor names: what a catalog check looks a tool's
/// namespace root up in (FIG-5824).
pub(crate) fn stored_session_descriptor(
    state: &HydratedExecutionGraph,
) -> Result<KeyedRoot, SessionOpenError> {
    Ok(stored_descriptor(state)?.session.bindings)
}

/// The helper releases this build holds, read once: a build's releases
/// never change while it runs.
fn held_releases() -> Option<&'static lash_vm_broker::kernel::state::ReleaseGraph> {
    static HELD: std::sync::OnceLock<Option<lash_vm_broker::kernel::state::ReleaseGraph>> =
        std::sync::OnceLock::new();
    HELD.get_or_init(|| lash_vm_broker::kernel::state::release_manifests().ok())
        .as_ref()
}

/// The stored check (FIG-5824): which of `roots` the stored binding tree
/// `bindings` binds, read through `pages`, the committed head's: a point
/// lookup per root, never a full listing. A page the store cannot answer
/// fails the check, never passes it.
///
/// # Errors
///
/// The store's faults leasing or reading the head's pages, or the heap's
/// reading the tree's.
pub(crate) async fn stored_binding_roots(
    pages: &SessionPages,
    bindings: &KeyedRoot,
    roots: &[String],
) -> Result<BTreeSet<String>, PageAccessError> {
    use lash_kernel_state::paged::metadata::metadata_key;
    use lash_kernel_state::paged::tree::{Access, Tree};

    let mut bound = BTreeSet::new();
    if roots.is_empty() || bindings.is_empty() {
        return Ok(bound);
    }
    let mut access = SessionPageAccess::default();
    access.note(Some(pages));
    let bindings = *bindings;
    let roots = roots.to_vec();
    let found = access
        .read(move |source| {
            let access = Access::new(source);
            let tree = Tree::<lash_kernel_state::paged::BindingRecord>::of_keyed(&bindings);
            let mut bound = BTreeSet::new();
            for root in &roots {
                if tree
                    .get(&metadata_key(&Name::new(root.as_str())), &access)?
                    .is_some()
                {
                    bound.insert(root.clone());
                }
            }
            Ok(bound)
        })
        .await;
    access.release().await;
    match found {
        Ok(found) => {
            bound.extend(found);
            Ok(bound)
        }
        Err(error) => Err(error),
    }
}
