//! Language observation of a process's run, at the machine's own
//! identities.
//!
//! The machine reports each `perform` and `sleep` with its effect identity
//! (task, site, occurrence, loops), and a fact about one is published at
//! exactly that identity: a host can tell which element of a fan-out it
//! belongs to. A `kernel_run` may be recomputed, so every fact's key is a
//! function of the process and the identity: a repeat is the same fact.

use lash_core::plugin::PluginExecutionTrace;
use lash_kernel_doc::EffectIdentity;
use lash_trace::{
    TraceEvent, TraceLanguageExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceNodeAwaited, TraceNodeFact, TraceNodeWaitResolution, TraceRuntimeScope,
    TraceRuntimeSubject, WorkflowDocumentEntry, WorkflowDocumentRef,
};
use lash_vm_client::wire::OutcomeWire;

use super::state::{Delivery, Issued, KernelProcessInput};

pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
}

impl ProcessTrace {
    pub(super) fn new(
        runtime: Option<&lash_core::trace::TraceRuntime>,
        process: &lash_core::ProcessId,
        input: &KernelProcessInput,
    ) -> Option<Self> {
        let tracing = PluginExecutionTrace::new(runtime?.unreplayed(None));
        if !tracing.observes_language() {
            return None;
        }
        Some(Self {
            tracing,
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::none(),
                subject: TraceRuntimeSubject::Process {
                    process_id: process.clone(),
                },
                document: WorkflowDocumentRef {
                    document: input.document,
                    entry: WorkflowDocumentEntry::Entry {
                        function: input.entry.clone(),
                    },
                },
                entry_name: input.entry.to_string(),
                engine_execution_id: Some(process.to_string()),
                // The durable actor has no execution-attempt fact. Its
                // minted process identity names the resumed lifetime.
                generation: None,
            },
        })
    }

    /// The execution began: it names the document it runs and the entry it
    /// enters it by.
    pub(super) fn started(&self) {
        self.emit(TraceLanguageExecutionPayload::ExecutionStarted);
    }

    pub(super) fn finished(&self, outcome: &lash_core::ProcessOutcome) {
        let status = match outcome.terminal_status() {
            Some(lash_core::TerminalProcessStatus::Completed) => {
                TraceLanguageExecutionStatus::Completed
            }
            Some(lash_core::TerminalProcessStatus::Cancelled) => {
                TraceLanguageExecutionStatus::Cancelled
            }
            _ => TraceLanguageExecutionStatus::Failed,
        };
        self.emit(TraceLanguageExecutionPayload::ExecutionFinished {
            status,
            error: None,
        });
    }

    fn node(&self, at: &EffectIdentity, fact: TraceNodeFact) {
        self.emit(TraceLanguageExecutionPayload::Node {
            at: at.clone(),
            fact,
        });
    }

    /// One outcome the machine is handed: its wait ended.
    pub(super) fn delivered(&self, delivery: &Delivery) {
        let fact = match &delivery.outcome {
            OutcomeWire::Completed(_) => TraceNodeFact::Completed { call_id: None },
            OutcomeWire::Failed(error) => TraceNodeFact::Failed {
                call_id: None,
                failure: TraceLanguageExecutionFailure::Runtime {
                    code: error.kind.clone(),
                    message: error.message.clone(),
                },
            },
            OutcomeWire::Elapsed => TraceNodeFact::Resumed {
                resolution: TraceNodeWaitResolution::Resumed,
            },
        };
        self.node(&delivery.identity, fact);
        // A sleep that ended is done; an effect's own terminal says so.
        if matches!(delivery.outcome, OutcomeWire::Elapsed) {
            self.node(
                &delivery.identity,
                TraceNodeFact::Completed { call_id: None },
            );
        }
    }

    /// One wait the machine asked for at a park. An effect that became a
    /// step is not reported here: its actor reports the body's start once
    /// it admits the step, bound to its call, and a start published here
    /// would claim a call that admission can still refuse.
    pub(super) fn issued(&self, issued: &Issued) {
        match issued {
            Issued::Effect { .. } => {}
            Issued::Refused { identity, .. } => {
                self.node(identity, TraceNodeFact::Started { call_id: None });
            }
            Issued::Sleep {
                identity, until_ms, ..
            } => {
                self.node(identity, TraceNodeFact::Started { call_id: None });
                self.node(
                    identity,
                    TraceNodeFact::Waiting {
                        awaited: TraceNodeAwaited::Sleep {
                            deadline_ms: u64::try_from(*until_ms).ok(),
                        },
                    },
                );
            }
        }
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
                    language: None,
                    event: event.clone(),
                },
            )
        });
    }
}
