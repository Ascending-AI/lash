//! Projections of committed register facts. Observation owns no guest state.

use std::cell::{Cell, RefCell};

use lash_kernel_doc::TaskId;
use lash_vocabulary::sites::{EffectIdentity, EffectLocation, LoopIteration, Site, TaskIdentity};

use super::*;
use crate::compile::{BlockId, LibId};
use crate::observe::{Event, GuestPhase, Observer};
use lash_kernel_doc::heap::HeapFault;

pub(super) type Coordinates = (
    Result<TaskIdentity, HeapFault>,
    Result<Vec<LoopIteration>, HeapFault>,
);

pub(super) struct StatementFact {
    pub(super) code: CodeId,
    pub(super) stmt: StmtId,
    pub(super) block: BlockId,
    pub(super) index: usize,
    pub(super) charging: bool,
}

/// One entry lends its semantic observer, task coordinates and count accumulator
/// to the common loop. Physical frame installation never replays these facts.
pub(crate) struct Watching<'a> {
    observer: Option<RefCell<&'a mut Observer>>,
    ran: RefCell<&'a mut [u64]>,
    task: TaskId,
    identity: Option<Result<TaskIdentity, HeapFault>>,
    loops: Result<Vec<LoopIteration>, HeapFault>,
    native: Cell<bool>,
    active_loops: RefCell<Vec<LoopIteration>>,
    host: Option<RefCell<&'a mut dyn crate::Host>>,
    sites: bool,
    lib: Cell<LibId>,
    fast: Cell<bool>,
    formulas: Cell<u64>,
    guard: Cell<u64>,
}

impl<'a> Watching<'a> {
    pub(super) fn new(
        observer: Option<&'a mut Observer>,
        ran: &'a mut [u64],
        task: TaskId,
        coordinates: Option<Coordinates>,
        host: Option<&'a mut dyn crate::Host>,
        sites: bool,
    ) -> Option<Box<Self>> {
        if ran.is_empty() && !sites && observer.as_ref().is_none_or(|o| o.only_charges()) {
            return None;
        }
        let (identity, loops) =
            coordinates.map_or((None, Ok(Vec::new())), |(id, loops)| (Some(id), loops));
        Some(Box::new(Self {
            observer: observer.map(RefCell::new),
            ran: RefCell::new(ran),
            task,
            identity,
            loops,
            host: host.map(RefCell::new),
            sites,
            native: Cell::new(false),
            active_loops: RefCell::new(Vec::new()),
            lib: Cell::new(LibId(0)),
            fast: Cell::new(false),
            formulas: Cell::new(0),
            guard: Cell::new(0),
        }))
    }

    pub(super) fn on(&self, event: Event, charged: u64, allocated: u64) {
        if let Some(observer) = &self.observer {
            observer.borrow_mut().on(event, charged, allocated);
        }
    }

    pub(super) fn statement(&self, exe: &Executable, fact: StatementFact, meters: (u64, u64)) {
        let StatementFact {
            code,
            stmt,
            block,
            index,
            charging,
        } = fact;
        if let Some(ordinal) = exe.stmt(stmt).ordinal {
            self.count_site(ordinal.get());
            self.path(exe.block(block).site.child(index as u32));
        }
        self.on(
            Event::Statement {
                task: self.task,
                code,
                stmt,
                charging,
            },
            meters.0,
            meters.1,
        );
    }

    fn count_site(&self, ordinal: u32) {
        let mut ran = self.ran.borrow_mut();
        if let Some(count) = ran.get_mut(ordinal as usize) {
            *count += 1;
        }
    }

    fn path(&self, site: Site) {
        if let Some(observer) = &self.observer
            && let Some(path) = &mut observer.borrow_mut().path
            && let Some(identity) = &self.identity
        {
            path.push(self.task, || identity.clone(), site);
        }
    }

    pub(super) fn empty(&self, exe: &Executable, block: BlockId) {
        if let Some(ordinal) = exe.block_ordinal(block) {
            self.count_site(ordinal);
            self.path(exe.block(block).site.clone());
        }
    }

    pub(super) fn expressions(&self, stream: &Stream, close: &Close, charging: bool) {
        if let Some(observer) = &self.observer {
            let mut observer = observer.borrow_mut();
            for event in stream.regions.events(close.region) {
                if let Weight::Units(units) = event.weight {
                    for _ in 0..units {
                        observer.expression(charging);
                    }
                }
            }
        }
    }

    pub(super) fn can_call(&self) -> bool {
        !self.sites || self.host.is_some()
    }

    /// A helper call starts its authored observation without advancing a
    /// guest occurrence: ordinary calls in an uncharged body stay uncounted.
    pub(super) fn action_start(&self, exe: &Executable, site: &Site) {
        if let Some(ordinal) = exe.action_ordinals.get(site) {
            self.count_site(*ordinal);
        }
    }

    pub(super) fn root_call(&self, exe: &Executable, id: StmtId, arguments: usize) {
        let crate::compile::Form::Action(action) = &exe.stmt(id).form else {
            unreachable!("a root call belongs to its source action");
        };
        self.action_start(exe, &action.site);
        if let Some(observer) = &self.observer {
            for _ in 0..arguments {
                observer.borrow_mut().expression(false);
            }
        }
    }

    pub(super) fn action(
        &self,
        exe: &Executable,
        index: usize,
        occurrence: u64,
    ) -> Result<(), Halt> {
        // The executable's semantic occurrence census is authoritative, including
        // long site keys. Never decode an index or physical pc into an identity.
        let site = exe
            .occurrence_site(index)
            .ok_or_else(|| fault("a counted occurrence has no authored site"))?;
        self.action_start(exe, site);
        if self.sites
            && let (Some(host), Some(task)) = (&self.host, &self.identity)
        {
            let mut loops = self.loops.clone()?;
            loops.extend(self.active_loops.borrow().iter().cloned());
            let task = task.clone()?;
            let at = lash_kernel_doc::kernel_internals::effect_id(EffectIdentity {
                task,
                site: site.clone(),
                occurrence,
                loops,
            });
            let native = self.native.replace(false);
            if native {
                self.phase(None);
            }
            host.borrow_mut().enter_site(&EffectLocation::authored(at));
            if native {
                self.native.set(true);
                self.phase(Some(GuestPhase::Native(self.lib.get().0)));
            }
        }
        Ok(())
    }

    /// Context phases are live during the semantic operation; counters commit
    /// only with its answer, so a precise slow exit records no speculative call.
    pub(crate) fn reset_facts(&self) {
        self.fast.set(false);
        self.formulas.set(0);
        self.guard.set(0);
    }

    pub(crate) fn native_begin(&self, lib: LibId) {
        self.lib.set(lib);
        self.native.set(true);
        self.reset_facts();
        self.phase(Some(GuestPhase::Native(lib.0)));
    }

    pub(crate) fn prepared(&self) {
        self.fast.set(true);
    }
    pub(crate) fn guard_work(&self, units: u64) {
        self.guard.set(units);
    }

    fn phase(&self, phase: Option<GuestPhase>) {
        if let Some(observer) = &self.observer
            && let Some(context) = &mut observer.borrow_mut().context
        {
            match phase {
                Some(phase) => context.enter(phase),
                None => context.leave(),
            }
        }
    }

    pub(super) fn native_end(&self, committed: bool, before: (u64, u64), after: (u64, u64)) {
        self.phase(None);
        self.native.set(false);
        if committed
            && let Some(observer) = &self.observer
            && let Some(work) = &mut observer.borrow_mut().work
        {
            work.flush(before.0, before.1);
            work.native_enter(self.task.0 as usize, self.lib.get().0);
            if self.fast.get() {
                work.native_fast();
            }
            work.guard_work(self.guard.get());
            for _ in 0..self.formulas.get() {
                work.formula(self.lib.get().0);
            }
            work.flush(after.0, after.1);
            work.native_exit();
        }
    }

    pub(super) fn push(&self, code: CodeId, charged: u64, allocated: u64) {
        self.on(
            Event::Push {
                task: self.task,
                code,
            },
            charged,
            allocated,
        );
    }
    pub(super) fn pop(&self, charged: u64, allocated: u64) {
        self.on(Event::Pop { task: self.task }, charged, allocated);
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    #[inline(always)]
    pub(super) fn observed_native(
        &mut self,
        lib: LibId,
        operation: impl FnOnce(&mut Self) -> Answer,
    ) -> Answer {
        if self.projection().is_none() {
            return operation(self);
        }
        let before = (self.charged, self.arena.allocations());
        if let Some(watching) = &self.projection() {
            watching.native_begin(lib);
        }
        let answer = operation(self);
        if let Some(watching) = &self.projection() {
            watching.native_end(
                answer.is_ok(),
                before,
                (self.charged, self.arena.allocations()),
            );
        }
        answer
    }
}

impl crate::observe::NativeObservation for Watching<'_> {
    fn guard_work(&self, units: u64) {
        Watching::guard_work(self, units);
    }
    fn formula_enter(&self) {
        self.formulas.set(self.formulas.get() + 1);
        self.phase(Some(GuestPhase::Formula(self.lib.get().0)));
    }
    fn formula_leave(&self) {
        self.phase(None);
    }
}

/// A borrowed projection sink keeps the register program's lifetime independent
/// of the machine fields lent to observation. It contains no execution policy.
pub(super) trait Projection: crate::observe::NativeObservation {
    fn reset_facts(&self);
    fn general_formula(&self, charging: bool);
    fn prepared(&self);
    fn statement(&self, exe: &Executable, fact: StatementFact, meters: (u64, u64));
    fn expressions(&self, stream: &Stream, close: &Close, charging: bool);
    fn empty(&self, exe: &Executable, block: BlockId);
    fn can_call(&self) -> bool;
    fn action_start(&self, exe: &Executable, site: &Site);
    fn root_call(&self, exe: &Executable, id: StmtId, arguments: usize);
    fn condition(&self, exe: &Executable, id: StmtId);
    fn sites(&self) -> bool;
    fn loops(&self, loops: Vec<LoopIteration>);
    fn action(&self, exe: &Executable, index: usize, occurrence: u64) -> Result<(), Halt>;
    fn native_begin(&self, lib: LibId);
    fn native_end(&self, committed: bool, before: (u64, u64), after: (u64, u64));
    fn push(&self, code: CodeId, charged: u64, allocated: u64);
    fn pop(&self, charged: u64, allocated: u64);
}
impl Projection for Watching<'_> {
    fn reset_facts(&self) {
        Watching::reset_facts(self);
    }
    fn general_formula(&self, charging: bool) {
        self.fast.set(false);
        self.formulas.set(u64::from(charging));
    }
    fn prepared(&self) {
        Watching::prepared(self);
    }
    fn statement(&self, exe: &Executable, fact: StatementFact, meters: (u64, u64)) {
        Watching::statement(self, exe, fact, meters);
    }
    fn expressions(&self, stream: &Stream, close: &Close, charging: bool) {
        Watching::expressions(self, stream, close, charging);
    }
    fn empty(&self, exe: &Executable, block: BlockId) {
        Watching::empty(self, exe, block);
    }
    fn action_start(&self, exe: &Executable, site: &Site) {
        Watching::action_start(self, exe, site);
    }
    fn root_call(&self, exe: &Executable, id: StmtId, arguments: usize) {
        Watching::root_call(self, exe, id, arguments);
    }
    fn condition(&self, exe: &Executable, id: StmtId) {
        if let Some(stmt) = exe.stmt(id).ordinal
            && let Some(ordinal) = exe.condition_ordinals.get(&stmt.get())
        {
            self.count_site(*ordinal);
        }
    }
    fn sites(&self) -> bool {
        self.sites
    }
    fn loops(&self, loops: Vec<LoopIteration>) {
        *self.active_loops.borrow_mut() = loops;
    }
    fn can_call(&self) -> bool {
        Watching::can_call(self)
    }
    fn action(&self, exe: &Executable, index: usize, occurrence: u64) -> Result<(), Halt> {
        Watching::action(self, exe, index, occurrence)
    }
    fn native_begin(&self, lib: LibId) {
        Watching::native_begin(self, lib);
    }
    fn native_end(&self, committed: bool, before: (u64, u64), after: (u64, u64)) {
        Watching::native_end(self, committed, before, after);
    }
    fn push(&self, code: CodeId, charged: u64, allocated: u64) {
        Watching::push(self, code, charged, allocated);
    }
    fn pop(&self, charged: u64, allocated: u64) {
        Watching::pop(self, charged, allocated);
    }
}

impl Drop for Watching<'_> {
    fn drop(&mut self) {
        // The entry owns the published phase before any native or host callback.
        // Unwinding a callback releases that same obligation.
        if self.native.get() {
            self.native_end(false, (0, 0), (0, 0));
        }
    }
}
