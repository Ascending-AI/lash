//! A process's tool and workflow node spans (spec §4.8, §3.8).
//!
//! Each tool step's call is a tool span, admitted with its step under the
//! span of the occurrence that issued it when the process retained
//! `trace_nodes` at registration, else under the process span. A node span
//! is keyed by the occurrence's whole effect identity, so every owner
//! derives the same one, and it starts at the admission of its step, which
//! retains that start in the step's tool scope. It closes once, on the
//! owner whose acknowledged commit consumed its step's outcome, or the
//! process's terminal for an occurrence still open. Whether it exists is
//! the retained `trace_nodes`, never the closing owner's own policy.

use lash_durable::domain::RunSeq;
use lash_trace::{
    DurableTraceScope, SpanKey, TraceContext, TraceDomainCompletion, TraceDomainStatus,
    TraceDomainSubject, TraceEvent, TraceSpanKind,
};

use super::driver::{Driver, InFlight};
use crate::runtime::actor::round::RunFold;
use crate::trace::TraceRuntime;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;

/// The tool scope `step`'s call, admitted at `now_ms`, is admitted with: under its occurrence's node when `record` retained
/// `trace_nodes`, else under the process. A step that is not a tool call
/// has none, and neither has one of a process admitted unobserved or one
/// this node does not observe.
pub(super) fn step_trace_scope(
    tracing: Option<&TraceRuntime>,
    record: &ProcessRecord,
    step: &InFlight,
    now_ms: u64,
) -> Option<DurableTraceScope> {
    let StepRequest::Tool { site, .. } = &step.request else {
        return None;
    };
    tracing.filter(|tracing| tracing.is_observed())?;
    let process = record.trace.as_ref()?.as_parent()?;
    let parent = match node_site(record, step, site.as_deref().map(|site| &site.at)) {
        Some(at) => process.container(&SpanKey::Node { at }),
        None => process,
    };
    Some(crate::trace::tool_trace_scope(
        &crate::EffectOpener::Process {
            process_id: record.id.clone(),
        },
        Some(&parent),
        &step.call,
        now_ms,
    ))
}

/// The occurrence whose node `step`'s call is beneath. A granted tool's
/// call's resolution is lash's lookup of the call, not the occurrence's
/// tool: its target is, and its admission starts the node.
fn node_site<'a>(
    record: &ProcessRecord,
    step: &InFlight,
    site: Option<&'a lash_kernel_value::sites::EffectId>,
) -> Option<&'a lash_kernel_value::sites::EffectId> {
    site.filter(|_| record.trace_nodes && !step.resolves)
}

/// One node span a commit closes.
struct NodeClose {
    at: lash_kernel_value::sites::EffectId,
    started_at_ms: u64,
    status: TraceDomainStatus,
}

/// The node spans the commit being written closes, published once it is
/// acknowledged.
#[derive(Default)]
pub(super) struct NodeCloses(Vec<NodeClose>);

impl NodeCloses {
    /// The node of `step`'s occurrence, whose settled `outcome` the commit
    /// being written hands to the engine.
    pub(super) fn consumed(
        record: &ProcessRecord,
        fold: &RunFold,
        step: &InFlight,
        outcome: &SettledOutput,
    ) -> Self {
        Self(
            node_of(record, fold, step, status_of(outcome))
                .into_iter()
                .collect(),
        )
    }

    /// The nodes of the occurrences `driver` still holds a step for, which
    /// the process's terminal ends.
    pub(super) fn open(record: &ProcessRecord, fold: &RunFold, driver: &Driver) -> Self {
        Self(
            driver
                .steps
                .values()
                .filter_map(|step| node_of(record, fold, step, TraceDomainStatus::Cancelled))
                .collect(),
        )
    }

    #[must_use]
    pub(super) fn and(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self
    }

    /// Publish each node's closing record, which ended at `ended_at_ms`:
    /// only after the commit that closes it was acknowledged (P1).
    pub(super) fn publish(
        self,
        tracing: Option<&TraceRuntime>,
        record: &ProcessRecord,
        ended_at_ms: u64,
    ) {
        let (Some(tracing), Some(scope)) = (
            tracing.filter(|tracing| tracing.is_observed()),
            record.trace.as_ref(),
        ) else {
            return;
        };
        let process = tracing.unreplayed(Some(scope.clone()));
        for node in self.0 {
            let Some(standing) = process.derived(
                TraceSpanKind::WorkflowNode,
                &SpanKey::Node { at: &node.at },
                node.started_at_ms,
            ) else {
                continue;
            };
            let mut context = TraceContext::default().for_process(record.id.clone());
            context.graph_node_id = Some(node.at.position().to_string());
            standing.close_scope(ended_at_ms.max(node.started_at_ms), || {
                (
                    context,
                    TraceEvent::DomainCompleted {
                        completion: TraceDomainCompletion::new(
                            TraceDomainSubject::WorkflowNode { at: node.at },
                            node.started_at_ms,
                            node.status,
                        ),
                    },
                )
            });
        }
    }
}

/// The node of `step`'s occurrence, from the start its admission retained
/// in the step's tool scope; none for a step beneath no node.
fn node_of(
    record: &ProcessRecord,
    fold: &RunFold,
    step: &InFlight,
    status: TraceDomainStatus,
) -> Option<NodeClose> {
    let StepRequest::Tool { site, .. } = &step.request else {
        return None;
    };
    let at = node_site(record, step, site.as_deref().map(|site| &site.at))?;
    let member = fold
        .round(RunSeq(step.run))?
        .members()
        .get(usize::try_from(step.member).ok()?)?;
    Some(NodeClose {
        at: at.clone(),
        started_at_ms: member.draft().trace()?.started_at_ms,
        status,
    })
}

/// How a step's settled outcome ends its occurrence: as the tool answer it
/// carries says, else as the settlement itself does.
fn status_of(outcome: &SettledOutput) -> TraceDomainStatus {
    if let Some(output) = outcome
        .payload()
        .and_then(|payload| serde_json::from_str::<crate::ToolCallOutput>(payload).ok())
    {
        return match output.outcome {
            crate::ToolCallOutcome::Success(_) => TraceDomainStatus::Completed,
            crate::ToolCallOutcome::Failure(_) => TraceDomainStatus::Failed,
            crate::ToolCallOutcome::Cancelled(_) => TraceDomainStatus::Cancelled,
        };
    }
    match outcome {
        SettledOutput::Completed(_) => TraceDomainStatus::Completed,
        SettledOutput::Cancelled { .. } => TraceDomainStatus::Cancelled,
        _ => TraceDomainStatus::Failed,
    }
}
