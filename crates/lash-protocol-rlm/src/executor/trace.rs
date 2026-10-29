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
use lash_protocol_types::sync::MutexExt as _;
use lash_trace::{
    TraceEvent, TraceLanguageExecutionFailure, TraceLanguageExecutionIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus, TraceNodeFact, TraceRuntimeScope,
    TraceRuntimeSubject,
};

use lash_kernel_value::sites::EffectId;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Emit the outcome of one protocol step once the model response that asked
/// for it is committed.
pub(super) fn emit_step_trace(ctx: &RuntimeExecutionContext<'_>, result: Result<(), &str>) {
    let Some(invocation) = ctx.parent_invocation().cloned() else {
        return;
    };
    let Some(step_index) = invocation.attribution.protocol_iteration else {
        return;
    };
    let Some(standing) = ctx.trace_standing() else {
        return;
    };
    let tracing = PluginExecutionTrace::new(standing);
    let failure = result.err().map(str::to_owned);
    let emit = move || step_trace(&tracing, &invocation, step_index, failure.as_deref());
    // Nothing of the cell is traced before a commit holds the model
    // response that asked for it (FIG-6000).
    match ctx.held_response() {
        Some(held) => held.after_commit(Box::new(emit)),
        None => emit(),
    }
}

fn step_trace(
    tracing: &PluginExecutionTrace,
    invocation: &lash_core::RuntimeInvocation,
    step_index: usize,
    failure: Option<&str>,
) {
    tracing.emit(|| {
        let context = lash_core::facade_support::trace_context_for_runtime_invocation(
            lash_trace::TraceContext::default(),
            invocation,
        );
        let outcome = match failure {
            None => lash_trace::TraceProgramStepOutcome::Ok,
            Some(diagnostic) => lash_trace::TraceProgramStepOutcome::Failure {
                diagnostic: tracing
                    .trace_runtime()
                    .diagnostic_limits()
                    .diagnostic_error(diagnostic),
            },
        };
        (
            context,
            TraceEvent::ProgramStep {
                step_index,
                outcome,
            },
        )
    });
}

#[derive(Clone)]
pub(super) struct CellTrace {
    tracing: PluginExecutionTrace,
    language: String,
    identity: TraceLanguageExecutionIdentity,
    address: lash_protocol_types::EffectAddress,
    effect_id: String,
    /// The model response that asked for the cell, while its turn holds
    /// it: nothing of the cell is traced before a commit holds it
    /// (FIG-6000).
    held: Option<lash_core::HeldResponse>,
    observation: lash_kernel_value::observation::RunObservation,
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
        link: lash_kernel_value::package::LinkId,
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
            observation: ctx.run_spec().map_or(
                lash_kernel_value::observation::RunObservation::Off,
                |spec| spec.observe,
            ),
            segment,
            counts: Arc::default(),
            tracing,
            language: language.to_owned(),
            address,
            effect_id: effect_id.to_string(),
            identity: TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope {
                    session_id: invocation.attribution.session_id.clone(),
                    turn_id: invocation.attribution.turn_id.clone(),
                    turn_index: invocation.attribution.turn_index,
                    protocol_iteration: invocation.attribution.protocol_iteration,
                },
                document: lash_trace::TraceExecutionCode::Start(
                    lash_kernel_value::names::LinkedStart {
                        link,
                        selector: lash_kernel_value::names::StartSelector::Main,
                    },
                ),
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
            subject: self.subject(),
            observation: self.observation,
        });
    }

    /// A cell reports cumulative counts within its stable activation segment.
    /// The fold uses maxima, so recovery, duplicates and reordering add nothing.
    pub(super) fn ran(&self, delta: &lash_kernel_value::observation::SiteCounts) {
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
        self.emit(TraceLanguageExecutionPayload::Counts {
            counts: lash_trace::TraceCountObservation::Cell(lash_trace::TraceCellCounts {
                address: self.address.clone(),
                effect_id: self.effect_id.clone(),
                segment: self.segment.clone(),
                frame,
                totals: ran,
            }),
        });
    }

    pub(super) fn finished(&self, status: TraceLanguageExecutionStatus, error: Option<String>) {
        self.emit(TraceLanguageExecutionPayload::ExecutionFinished {
            failure: None,
            subject: self.subject(),
            status,
            error,
        });
    }

    /// The cell's `perform` at `at` was admitted as the tool call `call`.
    pub(super) fn call_started(&self, at: &EffectId, call: &lash_core::ToolCallId) {
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
        at: &EffectId,
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

    pub(super) fn origin(&self, at: &EffectId, origin: &lash_kernel_value::sites::Site) {
        self.node(
            at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
    }

    fn node(&self, at: &EffectId, fact: TraceNodeFact) {
        self.emit(TraceLanguageExecutionPayload::Node {
            subject: self.subject(),
            at: at.clone(),
            fact,
        });
    }

    fn subject(&self) -> TraceRuntimeSubject {
        TraceRuntimeSubject::Effect {
            address: self.address.clone(),
            effect_id: self.effect_id.clone(),
        }
    }

    /// Trace `payload` once a commit holds the cell's model response: now,
    /// unless its turn still holds it.
    fn emit(&self, payload: TraceLanguageExecutionPayload) {
        match &self.held {
            Some(held) => {
                let trace = self.clone();
                held.after_commit(Box::new(move || trace.emit_now(payload)));
            }
            None => self.emit_now(payload),
        }
    }

    fn emit_now(&self, payload: TraceLanguageExecutionPayload) {
        lash_core::trace::observe_language(
            &self.tracing,
            &self.identity,
            payload,
            lash_trace::TraceContext {
                session_id: self.identity.scope.session_id.clone(),
                turn_id: self.identity.scope.turn_id.clone(),
                turn_index: self.identity.scope.turn_index,
                protocol_iteration: self.identity.scope.protocol_iteration,
                effect_id: Some(self.effect_id.clone()),
                ..lash_trace::TraceContext::default()
            },
            Some(self.language.clone()),
        );
    }
}
