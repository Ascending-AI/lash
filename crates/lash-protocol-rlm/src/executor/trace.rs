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
use lash_protocol_types::sync::MutexExt as _;
use lash_trace::{
    TraceEvent, TraceLanguageExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceNodeFact, TraceRuntimeScope, TraceRuntimeSubject, WorkflowDocumentEntry,
    WorkflowDocumentRef,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct CellTrace {
    tracing: PluginExecutionTrace,
    language: String,
    identity: TraceLanguageExecutionIdentity,
    /// The model response that asked for the cell, while its turn holds
    /// it: nothing of the cell is traced before a commit holds it
    /// (FIG-6000).
    held: Option<lash_core::HeldResponse>,
    observation: lash_kernel_doc::RunObservation,
    segment: String,
    counts: Arc<Mutex<CellCounts>>,
}

#[derive(Default)]
struct CellCounts {
    frame: u64,
    ran: BTreeMap<u32, u64>,
}

impl CellTrace {
    /// The trace of the cell `ctx` runs, when a host observes language
    /// execution and the cell runs under a recorded effect.
    pub(super) fn new(
        ctx: &RuntimeExecutionContext<'_>,
        document: DocumentId,
        language: &str,
        segment: String,
    ) -> Option<Self> {
        let tracing = PluginExecutionTrace::new(ctx.trace_standing()?);
        if !tracing.observes_language() {
            return None;
        }
        let invocation = ctx.parent_invocation()?;
        let effect_id = invocation.effect_id()?;
        let address = invocation.effect_address()?.clone();
        Some(Self {
            observation: ctx
                .run_spec()
                .map_or(lash_kernel_doc::RunObservation::Off, |spec| spec.observe),
            segment,
            counts: Arc::default(),
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
                engine_execution_id: None,
                // A process names its lifetime by its minted id; it records
                // no execution attempt to tell generations apart.
                generation: None,
            },
            held: ctx.held_response().cloned(),
        })
    }

    pub(super) fn started(&self) {
        self.emit(TraceLanguageExecutionPayload::ExecutionStarted {
            observation: self.observation,
        });
    }

    /// A cell reports cumulative counts within its stable activation segment.
    /// The fold uses maxima, so recovery, duplicates and reordering add nothing.
    pub(super) fn ran(&self, delta: &lash_kernel_vm::RanDelta) {
        let mut counts = self.counts.lock_recover();
        for (ordinal, count) in delta {
            let total = counts.ran.entry(*ordinal).or_default();
            *total = total.saturating_add(*count);
        }
        counts.frame += 1;
        let frame = counts.frame;
        let ran = counts
            .ran
            .iter()
            .map(|(ordinal, count)| (*ordinal, *count))
            .collect();
        drop(counts);
        self.emit_keyed(
            Some(format!("ran:{}:{frame}", self.segment)),
            TraceLanguageExecutionPayload::Ran {
                segment: Some(self.segment.clone()),
                ran,
            },
        );
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

    /// Trace `payload` once a commit holds the cell's model response: now,
    /// unless its turn still holds it.
    fn emit(&self, payload: TraceLanguageExecutionPayload) {
        self.emit_keyed(None, payload);
    }

    fn emit_keyed(&self, key: Option<String>, payload: TraceLanguageExecutionPayload) {
        match &self.held {
            Some(held) => {
                let trace = self.clone();
                held.after_commit(Box::new(move || trace.emit_now(key, payload)));
            }
            None => self.emit_now(key, payload),
        }
    }

    fn emit_now(&self, key: Option<String>, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted { .. } => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::Ran { .. } => (key.unwrap_or_default(), None),
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
        let mut context = lash_trace::TraceContext {
            session_id: self.identity.scope.session_id.clone(),
            turn_id: self.identity.scope.turn_id.clone(),
            turn_index: self.identity.scope.turn_index,
            protocol_iteration: self.identity.scope.protocol_iteration,
            graph_node_id: node,
            ..lash_trace::TraceContext::default()
        };
        if let TraceRuntimeSubject::Effect { effect_id, .. } = &self.identity.subject {
            context.effect_id = Some(effect_id.clone());
        }
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
