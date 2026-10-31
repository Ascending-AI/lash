//! The sole acknowledged process projection, retained under the claim's
//! OwnerState. External registry facts refresh separately from owner state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::{DecodeLimits, NodeId};
use lash_durable::domain::{ExecRootRow, OwnerKey, ProcessRegistryRow, RunFacts, RunSeq};
use lash_durable::{DurableError, DurableReads, StateRevision};
use lash_execution_contracts::process::{EngineState, ProcessRecord};

use super::activation::{corrupt, registry_failure};
use super::driver::Driver;
use crate::runtime::actor::round::{FoldRefusal, PinnedWaits, PolicyView, RunFold, RunRecords};
use lash_vocabulary::identity::ProcessId;

/// One activation's acknowledged process state. See the module
/// documentation.
#[derive(Clone, Default)]
pub(crate) struct Continuation {
    pub(crate) view: Option<lash_durable::domain::ProcessView>,
    pub(crate) resolutions: BTreeMap<
        lash_core_store::resolutions::ResolutionKey,
        lash_core_store::resolutions::ResolutionRecord,
    >,
    record: Option<(String, Arc<ProcessRecord>)>,
    driver: Option<(String, Driver)>,
    engine: Option<HeldEngine>,
    facts: Option<HeldFacts>,
}

/// The engine state the execution root `root` holds, with the engine that
/// wrote it.
#[derive(Clone)]
struct HeldEngine {
    root: NodeId,
    written_by: String,
    state: Arc<EngineState>,
}

/// The facts of the runs in flight, each run's with the waits its
/// admissions pinned, current at actor revision `current`; `None` once a
/// commit this activation did not see acknowledged may have moved them.
#[derive(Clone)]
struct HeldFacts {
    current: Option<StateRevision>,
    runs: BTreeMap<RunSeq, (RunRecords, PinnedWaits)>,
    projected: Option<(PolicyView, RunFold)>,
}

impl Continuation {
    pub(crate) fn loaded(view: lash_durable::domain::ProcessView) -> Self {
        Self {
            view: Some(view),
            ..Self::default()
        }
    }
    pub(crate) fn refresh_frontier(
        &mut self,
        frontier: lash_durable::domain::ProcessFrontier,
    ) -> Result<(), DurableError> {
        let Some(view) = &mut self.view else {
            return Ok(());
        };
        let ProcessRegistryRow::Retained(json) = &view.registry else {
            return Ok(());
        };
        let mut record = match &self.record {
            Some((held, record)) if held == json => Arc::clone(record),
            _ => Arc::new(
                lash_execution_contracts::process::registry_transitions::decode_record(json)
                    .map_err(|error| registry_failure(&error))?,
            ),
        };
        let external_ref = frontier
            .external_ref
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|error| corrupt("external process reference", error))?;
        if record.last_event_sequence != frontier.last_event_sequence
            || record.updated_at_ms != frontier.updated_at_ms
            || record.cancel_request.as_deref() != frontier.cancel.as_ref()
            || record.external_ref != external_ref
        {
            let next = Arc::make_mut(&mut record);
            next.last_event_sequence = frontier.last_event_sequence;
            next.updated_at_ms = frontier.updated_at_ms;
            next.cancel_request = frontier.cancel.map(Box::new);
            next.external_ref = external_ref;
            let json =
                lash_execution_contracts::process::registry_transitions::encode_view_record(next)
                    .map_err(|error| registry_failure(&error))?;
            view.registry = ProcessRegistryRow::Retained(json);
            if let Some(row) = &mut view.row {
                row.cancel_requested_at = next
                    .cancel_request
                    .as_ref()
                    .map(|cancel| {
                        i64::try_from(cancel.requested_at_ms).map(lash_durable::DurableInstant)
                    })
                    .transpose()
                    .map_err(|error| corrupt("process cancellation time", error))?;
            }
        }
        if let ProcessRegistryRow::Retained(json) = &view.registry {
            self.record = Some((json.clone(), record));
        }
        Ok(())
    }

    pub(super) fn facts_current(&self, revision: StateRevision) -> bool {
        self.facts
            .as_ref()
            .is_some_and(|held| held.current == Some(revision))
    }

    /// Fold only publications newly written by this candidate, and checkpoint
    /// their exact namespace frontier in that same transaction.
    pub(crate) fn prepare(&self, tx: &mut lash_durable::ActorTx) -> Result<(), DurableError> {
        if tx.domain().iter().any(|write| {
            matches!(
                write,
                lash_durable::DomainWrite::Process(
                    lash_durable::domain::ProcessWrite::Terminal { .. }
                )
            )
        }) {
            return Ok(());
        }
        let resolutions = tx.process_publications();
        if resolutions.is_empty() {
            return Ok(());
        }
        let Some(row) = self.view.as_ref().and_then(|view| view.row.as_ref()) else {
            return Err(corrupt("namespace checkpoint", "no process row"));
        };
        let mut expected_rev = row.state_rev;
        let mut driver = Driver::decode(row.driver_json.as_deref())
            .map_err(|error| corrupt("namespace checkpoint", error))?;
        // Respect the candidate's own driver, including admissions and consumed
        // runs. No engine or plugin callback runs inside the commit owner.
        for write in tx.domain() {
            match write {
                lash_durable::DomainWrite::Process(
                    lash_durable::domain::ProcessWrite::Advance {
                        process,
                        expected_rev: expected,
                        driver_json,
                    },
                ) if process == row.process => {
                    driver = Driver::decode(Some(driver_json))
                        .map_err(|error| corrupt("namespace checkpoint", error))?;
                    expected_rev = expected + 1;
                }
                lash_durable::DomainWrite::Process(
                    lash_durable::domain::ProcessWrite::ComponentFormats {
                        process,
                        driver_json,
                        ..
                    },
                ) if process == row.process => {
                    driver = Driver::decode(Some(driver_json))
                        .map_err(|error| corrupt("namespace checkpoint", error))?;
                }
                _ => {}
            }
        }
        let state = driver
            .namespace_state
            .take()
            .or_else(|| {
                driver
                    .component_formats
                    .as_ref()
                    .map(|cut| cut.state.clone())
            })
            .unwrap_or_default();
        driver.namespace_state = Some(
            crate::plugin::ComponentState::replay_snapshot(state, resolutions)
                .map_err(|error| corrupt("namespace checkpoint", error))?,
        );
        tx.write(lash_durable::DomainWrite::Process(
            lash_durable::domain::ProcessWrite::ComponentFormats {
                process: row.process.clone(),
                expected_rev,
                driver_json: driver.encode(),
            },
        ));
        Ok(())
    }

    pub(crate) fn acknowledge(
        mut self,
        receipt: &lash_durable::ActorCommit,
        actor: &lash_durable::ActorKey,
        appended: bool,
    ) -> Result<Self, DurableError> {
        let [view] = receipt.process_views.as_slice() else {
            return Err(DurableError::ProcessPostimage {
                actor: actor.clone(),
                message: "expected exactly one process view".into(),
            });
        };
        let process = ProcessId::parse(actor.id())
            .map_err(|error| corrupt("process receipt identity", error))?;
        if view
            .row
            .as_ref()
            .is_some_and(|row| row.process.as_str() != actor.id())
            || !receipt.session_heads.is_empty()
            || view
                .root
                .as_ref()
                .is_some_and(|root| root.exec != lash_durable::domain::ExecKey::Process(process))
        {
            return Err(DurableError::ProcessPostimage {
                actor: actor.clone(),
                message: "receipt names another owner".into(),
            });
        }
        // Decode and validate the entire record before replacing any field.
        if let ProcessRegistryRow::Retained(json) = &view.registry {
            let record =
                lash_execution_contracts::process::registry_transitions::decode_record(json)
                    .map_err(|error| registry_failure(&error))?;
            if record.id.as_str() != actor.id() {
                return Err(DurableError::ProcessPostimage {
                    actor: actor.clone(),
                    message: "registry names another process".into(),
                });
            }
        }
        self.view = Some(view.clone());
        if let Some(held) = &mut self.facts {
            if appended {
                held.current = None;
            } else if held.current.map(|rev| rev.0 + 1) == Some(receipt.revision.0) {
                held.current = Some(receipt.revision);
            }
        }
        Ok(self)
    }

    pub(crate) fn enrich(&mut self, derived: Self) {
        self.record = derived.record;
        self.driver = derived.driver;
        self.engine = derived.engine;
        self.facts = derived.facts;
    }

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
    /// reads, with the waits their admissions pinned: only the runs `driver`
    /// holds in flight, from the store only when this
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
    ) -> Result<usize, DurableError> {
        let owner = OwnerKey::Process(process.clone());
        let live = in_flight(driver);
        let current = self
            .facts
            .as_ref()
            .is_some_and(|held| held.current == Some(revision));
        if !current {
            let mut held = HeldFacts {
                current: Some(revision),
                runs: BTreeMap::new(),
                projected: None,
            };
            for run in &live {
                let facts = reads.run_facts_of(&owner, *run).await?;
                let waits = PinnedWaits::read(reads, &facts).await?;
                held.runs.insert(*run, (RunRecords::new(facts), waits));
            }
            self.facts = Some(held);
        }
        let held = self.facts.get_or_insert_with(HeldFacts::empty);
        let previous = held.runs.len();
        held.runs.retain(|run, _| live.contains(run));
        if held.runs.len() != previous {
            held.projected = None;
        }
        Ok(held
            .runs
            .values()
            .map(|(records, _)| records.facts().rows.len())
            .sum())
    }

    pub(super) fn reduced(&mut self) -> Result<RunFold, FoldRefusal> {
        let held = self.facts.get_or_insert_with(HeldFacts::empty);
        if let Some((_, fold)) = &held.projected {
            return Ok(fold.fresh_cursors());
        }
        held.fold(&PolicyView::default())
    }

    pub(super) fn fold(&mut self, current: &PolicyView) -> Result<RunFold, FoldRefusal> {
        self.facts
            .get_or_insert_with(HeldFacts::empty)
            .fold(current)
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
        let appended = by_run(appended, &live);
        if !appended.is_empty() {
            held.projected = None;
        }
        for (run, facts) in appended {
            let waits = PinnedWaits::read(reads, &facts).await?;
            match held.runs.get_mut(&run) {
                Some((held_facts, held_waits)) => {
                    held_facts.extend(facts, Vec::new());
                    held_waits.extend(waits);
                }
                None => {
                    held.runs.insert(run, (RunRecords::new(facts), waits));
                }
            }
        }
        let previous = held.runs.len();
        held.runs.retain(|run, _| live.contains(run));
        if held.runs.len() != previous {
            held.projected = None;
        }
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
            runs: BTreeMap::new(),
            projected: None,
        }
    }

    fn fold(&mut self, current: &PolicyView) -> Result<RunFold, FoldRefusal> {
        if let Some((policies, fold)) = &self.projected
            && policies == current
        {
            return Ok(fold.fresh_cursors());
        }
        let mut folded = RunFold::default();
        for (records, waits) in self.runs.values_mut() {
            folded.extend(records.fold(current, waits)?);
        }
        let projected = folded.fresh_cursors();
        self.projected = Some((current.clone(), folded));
        Ok(projected)
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
