//! Test-only code and affine state doubles. Neither can construct RegionCode,
//! CallPlan, StateLease or any other production proof authority.
use super::*;
use std::cell::{Cell, RefCell};

pub(super) struct State {
    shared: Rc<RefCell<Storage>>,
    leased: Rc<Cell<bool>>,
    lineage: Rc<()>,
}
struct Storage {
    version: u64,
    dependency: u64,
    root_transactions: u64,
    fail_prepare: bool,
    fail_commit: bool,
    image: Option<Reconstruction>,
    graph: Option<storage_fixture::FixtureGraph>,
}
#[derive(Clone)]
pub(super) struct View {
    shared: Rc<RefCell<Storage>>,
    leased: Rc<Cell<bool>>,
}
pub(super) struct StateLease<'m> {
    state: &'m mut State,
    version: u64,
    dependency: u64,
    lineage: Rc<()>,
    view_id: Option<u64>,
}
pub(super) struct PreparedStateExit<'m> {
    authority: StateLease<'m>,
    next_version: u64,
    graph: storage_fixture::FixtureGraph,
}
impl State {
    pub(super) fn new() -> Self {
        Self {
            shared: Rc::new(RefCell::new(Storage {
                version: 0,
                dependency: 0,
                root_transactions: 0,
                fail_prepare: false,
                fail_commit: false,
                image: None,
                graph: None,
            })),
            leased: Rc::new(Cell::new(false)),
            lineage: Rc::new(()),
        }
    }
    pub(super) fn view(&self) -> View {
        View {
            shared: Rc::clone(&self.shared),
            leased: Rc::clone(&self.leased),
        }
    }
    pub(super) fn fail_prepare(&mut self) {
        self.shared.borrow_mut().fail_prepare = true;
    }
    pub(super) fn fail_commit(&mut self) {
        self.shared.borrow_mut().fail_commit = true;
    }
}
impl View {
    pub(super) fn inspect<T>(
        &self,
        inspect: impl FnOnce(Option<&Reconstruction>, u64, u64) -> T,
    ) -> T {
        let state = self.shared.borrow();
        inspect(state.image.as_ref(), state.version, state.root_transactions)
    }
    pub(super) fn publish_alias(&self) -> Result<(), RegionError> {
        if self.leased.get() {
            return Err(invalid(Refused::Facts));
        }
        self.shared.borrow_mut().version += 1;
        Ok(())
    }
    pub(super) fn corrupt_dependency(&self) {
        self.shared.borrow_mut().dependency += 1;
    }
}
impl<'m> StateLease<'m> {
    fn acquire(state: &'m mut State) -> Result<Self, RegionError> {
        if state.leased.replace(true) {
            return Err(invalid(Refused::Facts));
        }
        let (version, dependency) = {
            let storage = state.shared.borrow();
            (storage.version, storage.dependency)
        };
        let lineage = Rc::clone(&state.lineage);
        Ok(Self {
            state,
            version,
            dependency,
            lineage,
            view_id: None,
        })
    }
    fn current(&self, storage: &Storage) -> bool {
        Rc::ptr_eq(&self.lineage, &self.state.lineage)
            && self.state.leased.get()
            && storage.version == self.version
            && storage.dependency == self.dependency
    }
    pub(super) fn prepare(
        self,
        _image: &Reconstruction,
        graph: storage_fixture::FixtureGraph,
    ) -> Result<PreparedStateExit<'m>, RegionError> {
        if self.view_id != Some(graph.lease) {
            return Err(invalid(Refused::Facts));
        }
        let next_version = {
            let mut storage = self.state.shared.borrow_mut();
            if !self.current(&storage) || storage.fail_prepare {
                return Err(invalid(Refused::Facts));
            }
            let next = storage
                .version
                .checked_add(1)
                .ok_or_else(|| invalid(Refused::Facts))?;
            storage.root_transactions += 1;
            next
        };
        Ok(PreparedStateExit {
            authority: self,
            next_version,
            graph,
        })
    }
}
impl Drop for StateLease<'_> {
    fn drop(&mut self) {
        self.state.leased.set(false);
    }
}
impl PreparedStateExit<'_> {
    pub(super) fn commit(self, image: Reconstruction) -> Result<(), RegionError> {
        let old = {
            let mut storage = self.authority.state.shared.borrow_mut();
            if !self.authority.current(&storage) || storage.fail_commit {
                return Err(invalid(Refused::Facts));
            }
            let old = (
                storage.image.replace(image),
                storage.graph.replace(self.graph),
            );
            storage.version = self.next_version;
            old
        };
        drop(old);
        Ok(())
    }
}
impl<'m> RegionLease<'m> {
    pub(super) fn acquire_fixture(
        state: &'m mut State,
        code: Arc<Code>,
        limits: Limits,
    ) -> Result<Self, RegionError> {
        let mut authority = StateLease::acquire(state)?;
        let programs = programs::Programs::bind(Box::new([Arc::clone(&code)]))?;
        let view = storage_fixture::FixtureView::bind(&programs, &code)?;
        authority.view_id = Some(view.lease);
        let first = code
            .invocation
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let invocations = super::invocations::Invocations::fixture(first, u64::MAX - first)?;
        let execution = Executing::Fixture(Execution::acquire(code, limits, invocations, view)?);
        Ok(Self {
            authority: Authority::Fixture(authority),
            execution,
        })
    }
}

impl<'m> RegionLease<'m> {
    pub(super) fn acquire_fixture_programs(
        state: &'m mut State,
        programs: Arc<programs::Programs>,
        entry: CodeId,
        limits: Limits,
    ) -> Result<Self, RegionError> {
        let mut authority = StateLease::acquire(state)?;
        let view = storage_fixture::FixtureView::bind(&programs, programs.code(entry)?)?;
        authority.view_id = Some(view.lease);
        let first = programs
            .code(entry)?
            .invocation
            .checked_add(1)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let invocations = super::invocations::Invocations::fixture(first, u64::MAX - first)?;
        let entry = super::entry::Entry::fixture(programs.code(entry)?);
        let execution = Executing::Fixture(Execution::acquire_programs(
            programs,
            entry,
            limits,
            invocations,
            view,
        )?);
        Ok(Self {
            authority: Authority::Fixture(authority),
            execution,
        })
    }
}

impl View {
    pub(super) fn inspect_graph<T>(
        &self,
        inspect: impl FnOnce(Option<&storage_fixture::FixtureGraph>) -> T,
    ) -> T {
        inspect(self.shared.borrow().graph.as_ref())
    }
}

impl<'m> RegionLease<'m> {
    pub(super) fn acquire_fixture_reserved(
        state: &'m mut State,
        code: Arc<Code>,
        limits: Limits,
        first: u64,
        count: u64,
    ) -> Result<Self, RegionError> {
        let mut authority = StateLease::acquire(state)?;
        let programs = programs::Programs::bind(Box::new([Arc::clone(&code)]))?;
        let view = storage_fixture::FixtureView::bind(&programs, &code)?;
        authority.view_id = Some(view.lease);
        let invocations = super::invocations::Invocations::fixture(first, count)?;
        let execution = Executing::Fixture(Execution::acquire(code, limits, invocations, view)?);
        Ok(Self {
            authority: Authority::Fixture(authority),
            execution,
        })
    }
}
