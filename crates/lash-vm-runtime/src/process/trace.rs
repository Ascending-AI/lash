//! Language observation of a process's run, at the machine's own
//! identities.
//!
//! The machine reports each `perform` and `sleep` with its effect identity
//! (task, site, occurrence, loops), and a fact about one is published at
//! exactly that identity: a host can tell which element of a fan-out it
//! belongs to. A `kernel_run` may be recomputed, so every fact's key is a
//! function of the process and the identity: a repeat is the same fact.

use lash_core::plugin::PluginExecutionTrace;
use lash_kernel_doc::EffectId;
use lash_trace::{
    TraceContext, TraceEvent, TraceLanguageExecution, TraceLanguageExecutionFailure,
    TraceLanguageExecutionIdentity, TraceLanguageExecutionPayload, TraceLanguageExecutionStatus,
    TraceNodeAwaited, TraceNodeFact, TraceNodeWaitResolution, TraceRuntimeScope,
    TraceRuntimeSubject, WorkflowDocumentEntry, WorkflowDocumentRef,
};
use lash_vm_client::wire::OutcomeWire;

use super::state::{Delivery, Issued, KernelProcessInput};

#[derive(Clone)]
pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
    process_id: lash_core::ProcessId,
    observation: lash_kernel_doc::RunObservation,
}

impl ProcessTrace {
    pub(super) fn new(
        runtime: Option<&lash_core::trace::TraceRuntime>,
        process: &lash_core::ProcessId,
        input: &KernelProcessInput,
        observation: lash_kernel_doc::RunObservation,
    ) -> Option<Self> {
        let tracing = PluginExecutionTrace::new(runtime?.unreplayed(None));
        if !tracing.observes_language() {
            return None;
        }
        Some(Self {
            observation,
            tracing,
            process_id: process.clone(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::none(),
                start: input.start.clone(),
                entry_name: match &input.start.selector {
                    lash_vocabulary::names::StartSelector::Main => "main".into(),
                    lash_vocabulary::names::StartSelector::Export(name) => name.to_string(),
                },
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
        self.emit(TraceLanguageExecutionPayload::ExecutionStarted {
            subject: self.subject(),
            observation: self.observation,
        });
    }

    pub(super) fn finished(&self, outcome: &lash_execution_contracts::process::ProcessOutcome) {
        let status = match outcome.terminal_status() {
            Some(lash_execution_contracts::process::TerminalProcessStatus::Completed) => {
                TraceLanguageExecutionStatus::Completed
            }
            Some(lash_execution_contracts::process::TerminalProcessStatus::Cancelled) => {
                TraceLanguageExecutionStatus::Cancelled
            }
            _ => TraceLanguageExecutionStatus::Failed,
        };
        self.emit(TraceLanguageExecutionPayload::ExecutionFinished {
            subject: self.subject(),
            status,
            error: None,
        });
    }

    pub(super) fn origin(&self, at: &EffectId, origin: &lash_kernel_doc::Site) {
        self.node(
            at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
    }

    pub(super) fn node(&self, at: &EffectId, fact: TraceNodeFact) {
        self.emit(TraceLanguageExecutionPayload::Node {
            subject: self.subject(),
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
        };
        self.node(&delivery.identity, fact);
    }

    /// One wait the machine asked for at a park. An effect that became a
    /// step is not reported here: its actor reports the body's start once
    /// it admits the step, bound to its call, and a start published here
    /// would claim a call that admission can still refuse.
    pub(super) fn issued(&self, issued: &Issued) {
        match issued {
            Issued::Effect { .. } => {}
            Issued::Refused { admission, .. } => {
                self.node(
                    admission.identity(),
                    TraceNodeFact::Started { call_id: None },
                );
            }
        }
    }

    /// One live delta of the segment that started at `segment`, its
    /// `frame`th: a recomputed segment reports the same facts.
    pub(super) fn ran(&self, segment: &str, frame: u64, delta: &lash_kernel_vm::SiteCounts) {
        self.emit(TraceLanguageExecutionPayload::Counts {
            counts: lash_trace::TraceCountObservation::Process(lash_trace::TraceProcessCounts {
                process_id: self.process_id.clone(),
                segment: segment.to_owned(),
                frame,
                delta: delta.iter().copied().collect(),
            }),
        });
    }

    fn subject(&self) -> TraceRuntimeSubject {
        TraceRuntimeSubject::Process {
            process_id: self.process_id.clone(),
        }
    }

    fn emit(&self, payload: TraceLanguageExecutionPayload) {
        use TraceLanguageExecutionPayload as Payload;
        let (suffix, node) = match &payload {
            Payload::ExecutionStarted { .. } => ("started".to_owned(), None),
            Payload::ExecutionFinished { .. } => ("finished".to_owned(), None),
            Payload::Counts { counts } => {
                let event_key = counts.event_key(&self.identity);
                (event_key, None)
            }
            Payload::Node { at, fact, .. } => {
                // The whole identity names an occurrence: every task counts
                // a site's occurrences from 0.
                let occurrence = serde_json::to_string(at).unwrap_or_default();
                let transition = match fact {
                    TraceNodeFact::BranchSelected { .. } => "branch",
                    TraceNodeFact::ChildStarted { .. } => "child",
                    TraceNodeFact::Origin { .. } => "origin",
                    TraceNodeFact::Started { .. } => "started",
                    TraceNodeFact::Completed { .. } => "completed",
                    TraceNodeFact::Failed { .. } => "failed",
                    TraceNodeFact::Cancelled => "cancelled",
                    TraceNodeFact::Waiting { .. } => "waiting",
                    TraceNodeFact::Resumed { .. } => "resumed",
                };
                (
                    format!("node:{occurrence}:{transition}"),
                    Some(at.position().to_string()),
                )
            }
        };
        let event_key = if matches!(&payload, Payload::Counts { .. }) {
            suffix
        } else {
            format!(
                "kernel_execution:{}:{suffix}",
                self.identity.graph_key(&self.subject())
            )
        };
        let context = TraceContext {
            graph_node_id: node,
            ..TraceContext::default()
        };
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
