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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use lash_durable::domain::{AdmittedId, RunSeq};
use lash_trace::{
    DurableTraceScope, SpanKey, TraceContext, TraceDomainCompletion, TraceDomainStatus,
    TraceDomainSubject, TraceEvent, TraceSpanKind,
};

use super::driver::{Driver, InFlight};
use crate::ToolCallId;
use crate::runtime::actor::round::{RoundView, RunFold};
use crate::trace::TraceRuntime;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;

/// The tool scope `call`, the call of `request` admitted at `now_ms`, is
/// admitted with: under its occurrence's node when `record` retained
/// `trace_nodes`, else under the process. A step that is not a tool call
/// has none, and neither has one of a process admitted unobserved or one
/// this node does not observe.
pub(super) fn step_trace_scope(
    tracing: Option<&TraceRuntime>,
    record: &ProcessRecord,
    request: &StepRequest,
    call: &ToolCallId,
    now_ms: u64,
) -> Option<DurableTraceScope> {
    let StepRequest::Tool { site, .. } = request else {
        return None;
    };
    tracing.filter(|tracing| tracing.is_observed())?;
    let process = record.trace.as_ref()?.as_parent()?;
    let parent = match node_site(record, request, site.as_ref()) {
        Some(at) => process.container(&SpanKey::Node { at }),
        None => process,
    };
    Some(crate::trace::tool_trace_scope(
        &crate::EffectOpener::Process {
            process_id: record.id.clone(),
        },
        Some(&parent),
        call,
        now_ms,
    ))
}

/// The occurrence whose node `request`'s call is beneath. A `call_dynamic`
/// call's resolution is lash's lookup of the call, not the occurrence's
/// tool: its target is, and its admission starts the node.
fn node_site<'a>(
    record: &ProcessRecord,
    request: &StepRequest,
    site: Option<&'a lash_protocol_types::EffectIdentity>,
) -> Option<&'a lash_protocol_types::EffectIdentity> {
    site.filter(|_| record.trace_nodes && !request.resolves())
}

/// One node span a commit closes.
struct NodeClose {
    at: lash_protocol_types::EffectIdentity,
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
            context.graph_node_id = Some(node.at.site.to_string());
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
    let at = node_site(record, &step.request, site.as_ref())?;
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

/// The tool spans of a process's step calls, as one activation observes its
/// folds: the admitting owner opens a call's span, and the owner whose
/// acknowledged commit wrote its final outcome closes it (P1).
#[derive(Default)]
pub(super) struct StepToolSpans {
    /// Calls this activation observed, and whether it closed their span.
    traced: Mutex<BTreeMap<ToolCallId, bool>>,
}

/// What the tool span of one step call names.
pub(super) struct StepCall {
    pub(super) process: crate::ProcessId,
    pub(super) request: StepRequest,
    /// The tool's name, as the step's catalog lists it.
    pub(super) tool: String,
}

impl StepToolSpans {
    /// Observe `round`'s committed state: open the span of each call
    /// `admitted_here`, and close the span of each call `settled_here`.
    pub(super) fn observe(
        &self,
        tracing: Option<&TraceRuntime>,
        round: &RoundView,
        admitted_here: &BTreeSet<AdmittedId>,
        settled_here: &BTreeSet<AdmittedId>,
        step: impl Fn(&ToolCallId, &crate::ToolId) -> Option<StepCall>,
    ) {
        let Some(tracing) = tracing.filter(|tracing| tracing.is_observed()) else {
            return;
        };
        for member in round.members() {
            let Some(scope) = member.draft().trace() else {
                continue;
            };
            let id = round.id_of(member);
            let call = member.call();
            let (start, close) = {
                let mut traced = self.traced.lock().unwrap_or_else(PoisonError::into_inner);
                let start = !traced.contains_key(call);
                let closed = traced.entry(call.clone()).or_insert(false);
                let close = !*closed && settled_here.contains(&id) && member.outcome().is_some();
                *closed |= close;
                (start, close)
            };
            let open = start && admitted_here.contains(&id);
            if !open && !close {
                continue;
            }
            let Some(step) = step(call, member.draft().tool()) else {
                continue;
            };
            let StepRequest::Tool { input, site, .. } = &step.request else {
                continue;
            };
            let context = TraceContext::default()
                .for_process(step.process.clone())
                .for_call(call.clone());
            let issuing_node = site.as_ref().map(|at| at.site.to_string());
            let standing = tracing
                .unreplayed(Some(scope.clone()))
                .selected(TraceSpanKind::Tool, &context.ids());
            let content = standing.content();
            if open {
                standing.open_scope(|| {
                    (
                        context.clone(),
                        TraceEvent::ToolCallStarted {
                            call_id: call.clone(),
                            provider_call_id: None,
                            name: step.tool.clone(),
                            args: content
                                .capture(lash_trace::ContentCategory::ToolArguments, || {
                                    input.clone()
                                }),
                            issuing_node_id: issuing_node.clone(),
                        },
                    )
                });
            }
            let (true, Some(outcome), Some(ended_at_ms)) =
                (close, member.outcome(), member.ended_at_ms())
            else {
                continue;
            };
            let record = step_record(call, &step.tool, input, outcome);
            let attempts = member
                .attempts()
                .map(|(ordinal, output, delay)| {
                    crate::trace::trace_tool_attempt(
                        ordinal,
                        &step_record(call, &step.tool, input, output),
                        delay,
                    )
                })
                .collect::<Vec<_>>();
            standing.close_scope(ended_at_ms, || {
                (
                    context,
                    TraceEvent::ToolCallCompleted {
                        call_id: call.clone(),
                        provider_call_id: None,
                        name: record.tool.clone().into(),
                        args: content
                            .capture(lash_trace::ContentCategory::ToolArguments, || {
                                record.args.clone()
                            })
                            .into(),
                        output: crate::trace::trace_tool_call_output(&record.output, content),
                        duration_ms: ended_at_ms.saturating_sub(scope.started_at_ms),
                        issuing_node_id: issuing_node,
                        attempts: (!attempts.is_empty()).then_some(attempts),
                        span_attributes: lash_trace::ToolSpanAnnotations::default(),
                    },
                )
            });
        }
    }
}

/// The call record a step's settled `output` retains, else the answer its
/// settlement stands for.
fn step_record(
    call: &ToolCallId,
    tool: &str,
    input: &serde_json::Value,
    output: &SettledOutput,
) -> crate::ToolCallRecord {
    if let Some(record) = output
        .payload()
        .and_then(|payload| serde_json::from_str::<crate::ToolCallRecord>(payload).ok())
    {
        return record;
    }
    crate::ToolCallRecord {
        call_id: call.clone(),
        provider_call_id: None,
        tool: tool.to_owned(),
        args: input.clone(),
        output: output.stopped_answer().unwrap_or_else(|| {
            crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "tool_outcome_unreadable",
                "the step's committed output is not a tool answer",
            ))
        }),
    }
}
