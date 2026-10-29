//! Language observation of a process's run, at the machine's own
//! identities.
//!
//! The machine reports each `perform` and `sleep` with its effect identity
//! (task, site, occurrence, loops), and a fact about one is published at
//! exactly that identity: a host can tell which element of a fan-out it
//! belongs to. A `kernel_run` may be recomputed, so every fact's key is a
//! function of the process and the identity: a repeat is the same fact.

use lash_core::plugin::PluginExecutionTrace;
use lash_kernel_value::sites::EffectId;
use lash_trace::{
    TraceContext, TraceLanguageExecutionFailure, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceNodeFact, TraceRuntimeScope,
    TraceRuntimeSubject,
};
use lash_vm_client::wire::OutcomeWire;

use super::state::{Delivery, Issued, KernelProcessInput};

#[derive(Clone)]
pub(super) struct ProcessTrace {
    tracing: PluginExecutionTrace,
    identity: TraceLanguageExecutionIdentity,
    process_id: lash_vocabulary::identity::ProcessId,
    observation: lash_kernel_value::observation::RunObservation,
}

impl ProcessTrace {
    pub(super) fn new(
        runtime: Option<&lash_core::trace::TraceRuntime>,
        process: &lash_vocabulary::identity::ProcessId,
        input: &KernelProcessInput,
        observation: lash_kernel_value::observation::RunObservation,
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
                document: match &input.code {
                    super::InlineRequest::Linked { start } => {
                        lash_trace::TraceExecutionCode::Start(start.clone())
                    }
                    super::InlineRequest::Callable { function, .. } => {
                        lash_trace::TraceExecutionCode::Callable(function.callable)
                    }
                    super::InlineRequest::Unlinked { .. } => return None,
                },
                entry_name: match &input.code {
                    super::InlineRequest::Linked { start } => match &start.selector {
                        lash_kernel_value::names::StartSelector::Main => "main".into(),
                        lash_kernel_value::names::StartSelector::Export(name) => name.to_string(),
                    },
                    super::InlineRequest::Callable { function, .. } => {
                        function.callable.function.to_string()
                    }
                    super::InlineRequest::Unlinked { .. } => return None,
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

    pub(super) fn origin(&self, at: &EffectId, origin: &lash_kernel_value::sites::Site) {
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
    pub(super) fn ran(
        &self,
        segment: &str,
        frame: u64,
        delta: &lash_kernel_value::observation::SiteCounts,
    ) {
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
        lash_core::trace::observe_language(
            &self.tracing,
            &self.identity,
            payload,
            TraceContext::default(),
            None,
        );
    }
}
