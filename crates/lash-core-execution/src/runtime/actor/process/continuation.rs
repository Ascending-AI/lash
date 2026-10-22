//! What a process activation keeps of its own acknowledged commits between
//! passes (FIG-7046): the registry record, the driver and the engine state
//! it decoded, each with the stored identity it was decoded from, and the
//! facts of the runs it holds in flight with the waits their admissions
//! pinned.
//!
//! It is a postimage of durable rows, never a grant: every write stays
//! fenced, and nothing here outlives the activation. Each pass reads its
//! [`ProcessView`](lash_durable::domain::ProcessView) fresh, and a piece is
//! used only while that view proves it current: a decoded record or driver
//! while the stored text is the text it was decoded from, an engine state
//! while the root row names the root it was hydrated from or written as,
//! and the facts while the actor's revision is the one this activation's
//! own acknowledged commits left. Any other commit (a lost acknowledgement,
//! a write of another path) moves the revision past them, and the runs in
//! flight are read again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{DecodeLimits, NodeId};
use lash_durable::domain::{ExecRootRow, OwnerKey, ProcessRegistryRow, RunFacts, RunSeq};
use lash_durable::{DurableError, DurableReads, StateRevision};
use lash_execution_contracts::process::{EngineState, ProcessRecord};

use super::activation::{corrupt, registry_failure};
use super::driver::Driver;
use crate::runtime::actor::round::PinnedWaits;
use lash_vocabulary::identity::ProcessId;

/// One activation's acknowledged process state. See the module
/// documentation.
#[derive(Default)]
pub(super) struct Continuation {
    record: Option<(String, Arc<ProcessRecord>)>,
    driver: Option<(String, Driver)>,
    engine: Option<HeldEngine>,
    facts: Option<HeldFacts>,
}

/// The engine state the execution root `root` holds, with the engine that
/// wrote it.
struct HeldEngine {
    root: NodeId,
    written_by: String,
    state: Arc<EngineState>,
}

/// The facts of the runs in flight, each run's with the waits its
/// admissions pinned, current at actor revision `current`; `None` once a
/// commit this activation did not see acknowledged may have moved them.
struct HeldFacts {
    current: Option<StateRevision>,
    read_limits: DecodeLimits,
    runs: BTreeMap<RunSeq, (RunFacts, PinnedWaits)>,
}

impl Continuation {
    /// The live process record `stored` holds: the one decoded last while
    /// the stored text is the same.
    ///
    /// # Errors
    ///
    /// The registry's refusal of a pruned or undecodable record, and a
    /// record that is gone as corrupt: the process's actor row is live.
    pub(super) fn record(
        &mut self,
        process: &ProcessId,
        stored: &ProcessRegistryRow,
    ) -> Result<Arc<ProcessRecord>, DurableError> {
        if let (ProcessRegistryRow::Retained(text), Some((held, record))) = (stored, &self.record)
            && text == held
        {
            return Ok(Arc::clone(record));
        }
        let record =
            lash_execution_contracts::process::registry_transitions::stored_record(process, stored)
                .map_err(|error| registry_failure(&error))?
                .ok_or_else(|| corrupt("a live process actor", "its registry row is gone"))?;
        let record = Arc::new(record);
        if let ProcessRegistryRow::Retained(text) = stored {
            self.record = Some((text.clone(), Arc::clone(&record)));
        }
        Ok(record)
    }

    /// The driver `stored` encodes: the one decoded last while the stored
    /// text is the same.
    ///
    /// # Errors
    ///
    /// The decode error of a driver that does not decode.
    pub(super) fn driver(&mut self, stored: Option<&str>) -> Result<Driver, serde_json::Error> {
        if let (Some(text), Some((held, driver))) = (stored, &self.driver)
            && text == held
        {
            return Ok(driver.clone());
        }
        let driver = Driver::decode(stored)?;
        if let Some(text) = stored {
            self.driver = Some((text.to_owned(), driver.clone()));
        }
        Ok(driver)
    }

    /// The engine state `root` holds, with the engine that wrote it: the
    /// one this activation hydrated or wrote last while the row names that
    /// root, else hydrated in one read fenced on that root and checked as
    /// `process`'s own.
    ///
    /// # Errors
    ///
    /// A store failure, or [`DurableError::Graph`] for a root that does not
    /// load as this process's state.
    pub(super) async fn engine(
        &mut self,
        reads: &dyn DurableReads,
        process: &ProcessId,
        root: &ExecRootRow,
        limits: DecodeLimits,
    ) -> Result<(String, Arc<EngineState>), DurableError> {
        if let Some(held) = &self.engine
            && held.root == root.root
        {
            return Ok((held.written_by.clone(), Arc::clone(&held.state)));
        }
        let (written_by, state) =
            super::state_graph::hydrate_engine_state(reads, process, root, limits).await?;
        let state = Arc::new(state);
        self.engine = Some(HeldEngine {
            root: root.root,
            written_by: written_by.clone(),
            state: Arc::clone(&state),
        });
        Ok((written_by, state))
    }

    /// The facts the fold of a pass opened at actor revision `revision`
    /// reads, with the waits their admissions pinned: on `restore`, every
    /// fact `process` holds, read once to restore its history; else those
    /// of the runs `driver` holds in flight, from the store only when this
    /// activation's acknowledged commits did not leave them current at
    /// `revision`.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub(super) async fn facts(
        &mut self,
        reads: &dyn DurableReads,
        process: &ProcessId,
        driver: &Driver,
        revision: StateRevision,
        restore: bool,
    ) -> Result<(RunFacts, PinnedWaits), DurableError> {
        let owner = OwnerKey::Process(process.clone());
        let live = in_flight(driver);
        if restore {
            let facts = reads.run_facts(&owner).await?;
            let waits = PinnedWaits::read(reads, &facts).await?;
            self.facts = Some(HeldFacts::of(&facts, &live, reads, revision).await?);
            return Ok((facts, waits));
        }
        let current = self
            .facts
            .as_ref()
            .is_some_and(|held| held.current == Some(revision));
        if !current {
            let mut held = HeldFacts {
                current: Some(revision),
                read_limits: lash_durable::domain::graph::OWNER_READ_LIMITS,
                runs: BTreeMap::new(),
            };
            for run in &live {
                let facts = reads.run_facts_of(&owner, *run).await?;
                held.read_limits = facts.read_limits;
                let waits = PinnedWaits::read(reads, &facts).await?;
                held.runs.insert(*run, (facts, waits));
            }
            self.facts = Some(held);
        }
        let held = self.facts.get_or_insert_with(HeldFacts::empty);
        held.runs.retain(|run, _| live.contains(run));
        Ok(held.combined())
    }

    /// This activation's commit that left the actor at `revision` was
    /// acknowledged, appending `appended`; `driver` is the driver it left.
    /// Its facts join the runs in flight when the commit is the very next
    /// one after the revision they are current at; otherwise they are read
    /// again.
    ///
    /// # Errors
    ///
    /// A store failure reading the waits a new admission pinned.
    pub(super) async fn committed(
        &mut self,
        reads: &dyn DurableReads,
        revision: StateRevision,
        appended: RunFacts,
        driver: &Driver,
    ) -> Result<(), DurableError> {
        let Some(held) = &mut self.facts else {
            return Ok(());
        };
        if held.current.map(|current| current.0.saturating_add(1)) != Some(revision.0) {
            held.current = None;
            return Ok(());
        }
        held.current = Some(revision);
        let live = in_flight(driver);
        for (run, facts) in by_run(appended, &live) {
            let waits = PinnedWaits::read(reads, &facts).await?;
            match held.runs.get_mut(&run) {
                Some((held_facts, held_waits)) => {
                    held_facts.extend(facts);
                    held_waits.extend(waits);
                }
                None => {
                    held.runs.insert(run, (facts, waits));
                }
            }
        }
        held.runs.retain(|run, _| live.contains(run));
        Ok(())
    }

    /// This activation's commit wrote `state`, which `engine` holds, as
    /// the execution root `root`, and `driver` encoded as `stored`.
    pub(super) fn wrote(
        &mut self,
        root: NodeId,
        engine: &str,
        state: EngineState,
        stored: String,
        driver: Driver,
    ) {
        self.engine = Some(HeldEngine {
            root,
            written_by: engine.to_owned(),
            state: Arc::new(state),
        });
        self.driver = Some((stored, driver));
    }
}

impl HeldFacts {
    fn empty() -> Self {
        Self {
            current: None,
            read_limits: lash_durable::domain::graph::OWNER_READ_LIMITS,
            runs: BTreeMap::new(),
        }
    }

    /// `facts`, every fact restored at `revision`, kept for the runs in
    /// `live`.
    async fn of(
        facts: &RunFacts,
        live: &BTreeSet<RunSeq>,
        reads: &dyn DurableReads,
        revision: StateRevision,
    ) -> Result<Self, DurableError> {
        let mut held = Self {
            current: Some(revision),
            read_limits: facts.read_limits,
            runs: BTreeMap::new(),
        };
        for (run, facts) in by_run(facts.clone(), live) {
            let waits = PinnedWaits::read(reads, &facts).await?;
            held.runs.insert(run, (facts, waits));
        }
        Ok(held)
    }

    /// The facts of every run held, in run and ordinal order, with their
    /// waits.
    fn combined(&self) -> (RunFacts, PinnedWaits) {
        let mut facts = RunFacts {
            read_limits: self.read_limits,
            ..RunFacts::default()
        };
        let mut waits = PinnedWaits::default();
        for (run_facts, run_waits) in self.runs.values() {
            facts.extend(run_facts.clone());
            waits.extend(run_waits.clone());
        }
        (facts, waits)
    }
}

/// The runs `driver` holds a step of in flight.
fn in_flight(driver: &Driver) -> BTreeSet<RunSeq> {
    driver.steps.values().map(|step| RunSeq(step.run)).collect()
}

/// `facts` of the runs in `live`, split by run, each run's rows with every
/// node they came with.
fn by_run(facts: RunFacts, live: &BTreeSet<RunSeq>) -> BTreeMap<RunSeq, RunFacts> {
    let mut runs: BTreeMap<RunSeq, RunFacts> = BTreeMap::new();
    for row in facts.rows {
        if !live.contains(&row.run) {
            continue;
        }
        runs.entry(row.run)
            .or_insert_with(|| RunFacts {
                read_limits: facts.read_limits,
                rows: Vec::new(),
                nodes: facts.nodes.clone(),
            })
            .rows
            .push(row);
    }
    runs
}
