//! Process step bodies and the claim obligations their lifecycle commits.

use super::activation::corrupt;
use super::driver::Driver;
use super::node_trace::{StepCall, StepToolSpans};
use crate::ToolCallId;
use crate::runtime::actor::claim_obligations::RecordedWork;
use crate::runtime::actor::round::{self, AdmittedExecution, MemberBodies, MemberBody};
use crate::runtime::actor::waits::Resolution;
use crate::runtime::process::steps::{ProcessSteps, StepRuntime};
use lash_core_store::tool_run::{AvailableEvidence, CompletionSource, Material, SettledOutput};
use lash_durable::domain::{RunFactKind, RunFacts};
use lash_durable::{ActorTx, DurableError, Epoch};
use lash_execution_contracts::process::{ProcessRecord, StepRequest};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

/// The bodies of a process's steps, from the host's [`ProcessSteps`]: what
/// the lifecycle runs, for the process record and the in-flight steps as
/// the last pass read them.
pub(super) struct StepBodies {
    pub(super) steps: Arc<dyn ProcessSteps>,
    /// What step bodies run under: the process actor's claimed context, and
    /// the process's namespaces, which this activation's committed step
    /// outcomes publish into.
    pub(super) runtime: Arc<StepRuntime>,
    pub(super) seen: Mutex<SeenSteps>,
    /// What the steps' tool spans are reported through; none on a node
    /// given no tracing.
    pub(super) tracing: Option<crate::trace::TraceRuntime>,
    pub(super) spans: StepToolSpans,
}

/// The process and its in-flight steps' requests, by call, as a pass read
/// them.
#[derive(Default)]
pub(super) struct SeenSteps {
    work: Option<RecordedWork>,
    resolving: BTreeSet<ToolCallId>,
    record: Option<Arc<ProcessRecord>>,
    requests: BTreeMap<ToolCallId, StepRequest>,
}

impl StepBodies {
    /// Note `record` and `driver`'s in-flight steps, as this pass read them.
    pub(super) fn saw(&self, record: &Arc<ProcessRecord>, driver: &Driver, work: RecordedWork) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.work = Some(work);
        seen.resolving = driver
            .steps
            .values()
            .filter(|step| step.resolves)
            .map(|step| step.call.clone())
            .collect();
        seen.record = Some(Arc::clone(record));
        seen.requests = driver
            .steps
            .values()
            .map(|step| (step.call.clone(), step.request.clone()))
            .collect();
    }

    /// The process and the request of the step `call` names.
    fn step(&self, call: &ToolCallId) -> Option<(Arc<ProcessRecord>, StepRequest)> {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        Some((
            Arc::clone(seen.record.as_ref()?),
            seen.requests.get(call)?.clone(),
        ))
    }
}

fn unknown_step() -> SettledOutput {
    SettledOutput::Cancelled {
        evidence: AvailableEvidence::default(),
    }
}

impl MemberBodies for StepBodies {
    fn stage_commit(&self, tx: &mut ActorTx, epoch: Epoch) -> Result<(), DurableError> {
        let facts = RunFacts::appended(tx.domain(), tx.graph(), epoch);
        if !facts
            .rows
            .iter()
            .any(|row| row.kind == RunFactKind::XOutcome)
        {
            return Ok(());
        }
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut work = seen
            .work
            .clone()
            .ok_or_else(|| corrupt("process obligations", "no driver was observed"))?;
        for row in &facts.rows {
            if row.kind != RunFactKind::XOutcome {
                continue;
            }
            let Some(call) = &row.call else {
                continue;
            };
            work.settle(&row.owner, call);
            if seen.resolving.contains(call) {
                let output = round::settled_output_of(&facts, row)
                    .map_err(|error| corrupt("a dispatch resolution", error))?;
                if let Some(matched) = crate::runtime::process::steps::resolved_match(&output) {
                    work.admit_linked(row.owner.clone(), call.clone(), None, matched.resolved());
                }
            }
        }
        tx.set_requires(work.requires());
        Ok(())
    }

    fn observe(
        &self,
        round: &round::RoundView,
        admitted_here: &BTreeSet<lash_durable::domain::AdmittedId>,
        settled_here: &BTreeSet<lash_durable::domain::AdmittedId>,
    ) {
        self.spans.observe(
            self.tracing.as_ref(),
            round,
            admitted_here,
            settled_here,
            |call, tool| {
                let (record, request) = self.step(call)?;
                let name = self
                    .runtime
                    .built_tools()
                    .and_then(|built| {
                        built
                            .catalog
                            .entries()
                            .iter()
                            .find(|entry| entry.manifest.id == *tool)
                            .map(|entry| entry.manifest.tool())
                    })
                    .unwrap_or_else(|| tool.to_string());
                Some(StepCall {
                    process: record.id.clone(),
                    request,
                    tool: name,
                })
            },
        );
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        match self.step(execution.call()) {
            Some((record, step)) => self.steps.body(&self.runtime, &record, &step, execution),
            // The lifecycle runs only admitted steps; a body for any other
            // is never asked for. Answer as a stop rather than run anything.
            None => Box::new(|_| Box::pin(async { unknown_step().into() })),
        }
    }

    fn stop_grace(&self) -> std::time::Duration {
        self.steps.stop_grace()
    }

    fn resolved(
        &self,
        execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        match self.step(execution.call()) {
            Some((record, step)) => self
                .steps
                .resolved(&record, &step, execution, parked, resolution),
            None => unknown_step(),
        }
    }

    fn publish_state(
        &self,
        state: &[crate::plugin::StateResolution],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        self.runtime.publish_state(state)
    }
}
