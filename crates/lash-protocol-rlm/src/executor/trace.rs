//! Language observation of a cell's run, at the machine's own identities.
//!
//! A cell is the `main` of its document. Each tool call it performs is a
//! fact at the effect identity the machine issued it under (task, site,
//! occurrence, loops), so a host tells the elements of a fan-out apart. A
//! resumed cell reads the same records again, so every fact's key is a
//! function of the cell's effect and the identity: a repeat is the same
//! fact.

use lash_core::RuntimeExecutionContext;
use lash_core::plugin::PluginExecutionTrace;
use lash_kernel_doc::{DocumentId, EffectIdentity};
use lash_trace::{
    TraceEvent, TraceLanguageExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionGeneration, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceNodeFact, TraceRuntimeScope,
    TraceRuntimeSubject, WorkflowDocumentEntry, WorkflowDocumentRef,
};

pub(super) struct CellTrace {
    tracing: PluginExecutionTrace,
    language: String,
    identity: TraceLanguageExecutionIdentity,
}

impl CellTrace {
    /// The trace of the cell `ctx` runs, when a host observes language
    /// execution and the cell runs under a recorded effect.
    pub(super) fn new(
        ctx: &RuntimeExecutionContext<'_>,
        document: DocumentId,
        language: &str,
    ) -> Option<Self> {
        let tracing = PluginExecutionTrace::new(ctx.trace_standing()?);
        if !tracing.observes_language() {
            return None;
        }
        let invocation = ctx.parent_invocation()?;
        let effect_id = invocation.effect_id()?;
        let address = invocation.effect_address()?.clone();
        let generation = match ctx.admitted_process() {
            Some(_) => Some(TraceLanguageExecutionGeneration::new(
                ctx.admitted_process_attempt()?,
            )),
            None => None,
        };
        Some(Self {
            tracing,
            language: language.to_owned(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope {
                    session_id: invocation.attribution.session_id.clone(),
                    turn_id: invocation.attribution.turn_id.clone(),
                    turn_index: invocation.attribution.turn_index,
                    protocol_iteration: invocation.attribution.protocol_iteration,
                },
                subject: TraceRuntimeSubject::Effect {
                    address,
                    effect_id: effect_id.to_string(),
                },
                document: WorkflowDocumentRef {
                    document,
                    entry: WorkflowDocumentEntry::Main,
                },
                entry_name: "main".to_owned(),
                engine_execution_id: ctx.engine_execution_id().map(str::to_owned),
                generation,
            },
        })
    }

    pub(super) fn started(&self) {
        self.emit(TraceLanguageExecutionPayload::ExecutionStarted);
    }

    pub(super) fn finished(&self, status: TraceLanguageExecutionStatus, error: Option<String>) {
        self.emit(TraceLanguageExecutionPayload::ExecutionFinished { status, error });
    }

    /// The cell's `perform` at `at` was admitted as the tool call `call`.
    pub(super) fn call_started(&self, at: &EffectIdentity, call: &lash_core::ToolCallId) {
        self.node(
            at,
            TraceNodeFact::Started {
                call_id: Some(call.clone()),
            },
        );
    }

    /// The tool call the `perform` at `at` was admitted as ended with
    /// `outcome`.
    pub(super) fn call_ended(
        &self,
        at: &EffectIdentity,
        call: &lash_core::ToolCallId,
        outcome: &lash_core::ToolCallOutcome,
    ) {
        let fact = match outcome {
            lash_core::ToolCallOutcome::Success(_) => TraceNodeFact::Completed {
                call_id: Some(call.clone()),
            },
            lash_core::ToolCallOutcome::Failure(failure) => TraceNodeFact::Failed {
                call_id: Some(call.clone()),
                failure: TraceLanguageExecutionFailure::Effect {
                    class: failure.class.clone(),
                    code: failure.code.clone(),
                    message: failure.message.clone(),
                    replay_key: call.to_string(),
                    source: failure.source.clone(),
                    suggested_delay_ms: failure.suggested_delay_ms,
                },
            },
            lash_core::ToolCallOutcome::Cancelled(_) => TraceNodeFact::Cancelled,
        };
        self.node(at, fact);
    }

    fn node(&self, at: &EffectIdentity, fact: TraceNodeFact) {
        self.emit(TraceLanguageExecutionPayload::Node {
            at: at.clone(),
            fact,
        });
    }

    fn emit(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::Node { at, fact } => {
                // The whole identity names an occurrence: every task counts
                // a site's occurrences from 0.
                let occurrence = serde_json::to_string(at).unwrap_or_default();
                let transition = match fact {
                    TraceNodeFact::BranchSelected { .. } => "branch",
                    TraceNodeFact::ChildStarted { .. } => "child",
                    TraceNodeFact::Started { .. } => "started",
                    TraceNodeFact::Completed { .. } => "completed",
                    TraceNodeFact::Failed { .. } => "failed",
                    TraceNodeFact::Cancelled => "cancelled",
                    TraceNodeFact::Waiting { .. } => "waiting",
                    TraceNodeFact::Resumed { .. } => "resumed",
                };
                (
                    format!("node:{occurrence}:{transition}"),
                    Some(at.site.to_string()),
                )
            }
        };
        let event_key = format!("kernel_execution:{}:{suffix}", self.identity.graph_key());
        let mut context = self.tracing.trace_runtime().base_context().clone();
        context.session_id = self.identity.scope.session_id.clone();
        context.turn_id = self.identity.scope.turn_id.clone();
        context.turn_index = self.identity.scope.turn_index;
        context.protocol_iteration = self.identity.scope.protocol_iteration;
        if let TraceRuntimeSubject::Effect { effect_id, .. } = &self.identity.subject {
            context.effect_id = Some(effect_id.clone());
        }
        context.graph_node_id = node;
        let event = TraceLanguageExecution {
            event_key,
            identity: self.identity.clone(),
            payload,
        };
        self.tracing.observe_language(&event.event_key, || {
            (
                context.clone(),
                TraceEvent::LanguageExecution {
                    language: Some(self.language.clone()),
                    event: event.clone(),
                },
            )
        });
    }
}
