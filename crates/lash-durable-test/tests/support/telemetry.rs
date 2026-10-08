//! A recording telemetry adapter for the trace-scope laws (FIG-5363,
//! FIG-5382): the scope factory and projector a core's trace runtime is
//! served with.
//!
//! The factory mints a fresh trace for every root or linked proposal and
//! keeps its parent's trace for an owned one, as an exporting adapter does,
//! and records every admission it exported: the `lash.turn.admitted` span
//! an adapter exports for a turn's scope, and the `lash.tool.admitted` span
//! for a tool call's, whether a candidate was selected for it or a reader
//! reconciled it (FIG-5395). Like an exporting adapter, it dedupes an
//! admission's identity, its anchor, and holds a deferred candidate until a
//! reconcile of its anchor selects it. The projector records the scope
//! every record was made under.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use lash::tracing::{
    AttemptObservation, DurableTraceScope, EmissionSource, TraceAdmissionCandidate, TraceAnchor,
    TraceCandidateOutcome, TraceCarrier, TraceCause, TraceDomainProjector, TraceRecord,
    TraceScopeFactory, TraceScopeId, TraceScopeOwner, W3cSpanId, W3cTraceFlags, W3cTraceId,
    W3cTraceState,
};
use lash_sansio::sync::MutexExt as _;

/// What the recording adapter saw.
#[derive(Default)]
struct Recorded {
    /// How many candidates were proposed: each mints the next span.
    proposed: u64,
    /// Every exported admission, with the anchor it gave its scope.
    admitted: Vec<(TraceScopeId, TraceCarrier)>,
    /// The candidates held deferred.
    deferred: Vec<(TraceScopeId, TraceCarrier)>,
    /// Every projected record's scope and turn.
    projected: Vec<(DurableTraceScope, Option<lash_sansio::TurnId>)>,
}

/// The recording adapter: its scope factory and its projector.
#[derive(Clone, Default)]
pub struct Telemetry(Arc<Mutex<Recorded>>);

struct Candidate {
    scope: TraceScopeId,
    carrier: TraceCarrier,
    recorded: Arc<Mutex<Recorded>>,
}

impl TraceAdmissionCandidate for Candidate {
    fn anchor(&self) -> TraceAnchor {
        TraceAnchor::Context(self.carrier.clone())
    }

    fn settle(self: Box<Self>, outcome: TraceCandidateOutcome) {
        if outcome == TraceCandidateOutcome::Selected {
            self.recorded
                .lock_recover()
                .export(self.scope, self.carrier);
        }
    }

    fn defer(self: Box<Self>) {
        self.recorded
            .lock_recover()
            .deferred
            .push((self.scope, self.carrier));
    }
}

impl Recorded {
    /// Export the admission of `scope` under `carrier` unless its identity
    /// already was: the deferred candidates of the scope are settled.
    fn export(&mut self, scope: TraceScopeId, carrier: TraceCarrier) {
        self.deferred.retain(|(deferred, _)| *deferred != scope);
        if !self
            .admitted
            .iter()
            .any(|(_, exported)| *exported == carrier)
        {
            self.admitted.push((scope, carrier));
        }
    }
}

impl TraceScopeFactory for Telemetry {
    fn capture_current(&self) -> Option<TraceCarrier> {
        None
    }

    fn propose(
        &self,
        scope: &TraceScopeId,
        cause: &TraceCause,
    ) -> Box<dyn TraceAdmissionCandidate> {
        let proposed = {
            let mut recorded = self.0.lock_recover();
            recorded.proposed += 1;
            recorded.proposed
        };
        // An owned invocation continues its parent's trace; anything else
        // starts one of its own.
        let trace = match cause {
            TraceCause::Parent(parent) => parent.trace_id(),
            TraceCause::Root | TraceCause::Linked(_) => {
                W3cTraceId::from_bytes(u128::from(proposed).to_be_bytes()).expect("a trace id")
            }
        };
        let carrier = TraceCarrier::new(
            trace,
            W3cSpanId::from_bytes(proposed.to_be_bytes()).expect("a span id"),
            W3cTraceFlags::from_byte(W3cTraceFlags::SAMPLED),
            W3cTraceState::default(),
        );
        Box::new(Candidate {
            scope: scope.clone(),
            carrier,
            recorded: Arc::clone(&self.0),
        })
    }

    fn export_admitted(&self, scope: &DurableTraceScope) {
        if let TraceAnchor::Context(carrier) = &scope.anchor {
            self.0
                .lock_recover()
                .export(scope.scope.clone(), carrier.clone());
        }
    }
}

impl TraceDomainProjector for Telemetry {
    fn project(
        &self,
        scope: &DurableTraceScope,
        _attempt: Option<&AttemptObservation>,
        _source: &EmissionSource,
        record: &TraceRecord,
    ) {
        self.0
            .lock_recover()
            .projected
            .push((scope.clone(), record.context.turn_id.clone()));
    }
}

impl Telemetry {
    /// A trace runtime served with this adapter.
    pub fn runtime(&self) -> lash_core::runtime::TraceRuntime {
        lash_core::runtime::TraceRuntime::new(Arc::new(lash_core::runtime::SystemClock))
            .with_scopes(Arc::new(self.clone()))
            .with_projector(Arc::new(self.clone()))
    }

    /// How `session`'s first turn broke its trace scope: unless its
    /// admission scope was exported exactly once, and every record of the
    /// turn carries the trace that admission started (every record of the
    /// turn's own scope, its anchor).
    pub fn first_turn_violations(&self, session: &str) -> Vec<String> {
        let recorded = self.0.lock_recover();
        let turn_admissions = recorded
            .admitted
            .iter()
            .filter(|(scope, _)| {
                scope.boundary == 0
                    && matches!(
                        &scope.owner,
                        TraceScopeOwner::Turn { session_id, .. } if session_id.as_str() == session
                    )
            })
            .collect::<Vec<_>>();
        let Some((scope, _)) = turn_admissions.first() else {
            return vec![format!("no turn of session {session} was admitted")];
        };
        let TraceScopeOwner::Turn { turn_id, .. } = &scope.owner else {
            unreachable!("filtered to turn scopes");
        };
        let admitted = turn_admissions
            .iter()
            .filter(|(other, _)| other == scope)
            .map(|(_, carrier)| carrier)
            .collect::<Vec<_>>();
        let original = admitted[0];
        let mut violations = Vec::new();
        if admitted.len() != 1 {
            violations.push(format!(
                "turn {turn_id}'s scope must be admitted exactly once, but was admitted with \
                 {admitted:?}"
            ));
        }
        for (record_scope, _) in recorded
            .projected
            .iter()
            .filter(|(record_scope, record_turn)| {
                record_turn.as_ref() == Some(turn_id) || record_scope.scope == *scope
            })
        {
            let anchor = record_scope.anchor.context();
            let stray = if record_scope.scope == *scope {
                anchor != Some(original)
            } else {
                anchor.map(TraceCarrier::trace_id) != Some(original.trace_id())
            };
            if stray {
                violations.push(format!(
                    "a record of turn {turn_id} under {:?} carries {anchor:?}, not its \
                     admission's trace {original:?}",
                    record_scope.scope
                ));
            }
        }
        violations
    }

    /// How the tool calls of `session`'s first turn broke their admission
    /// (FIG-5382, FIG-5395): unless each call's scope was admitted exactly
    /// once (the `lash.tool.admitted` span an adapter exports), on the trace
    /// the turn's admission started.
    pub fn first_turn_tool_violations(&self, session: &str) -> Vec<String> {
        let recorded = self.0.lock_recover();
        let Some((turn_scope, turn_anchor)) = recorded.admitted.iter().find(|(scope, _)| {
            scope.boundary == 0
                && matches!(
                    &scope.owner,
                    TraceScopeOwner::Turn { session_id, .. } if session_id.as_str() == session
                )
        }) else {
            return vec![format!("no turn of session {session} was admitted")];
        };
        let TraceScopeOwner::Turn { turn_id, .. } = &turn_scope.owner else {
            unreachable!("found a turn scope");
        };
        let mut calls = std::collections::BTreeMap::<&str, Vec<&TraceCarrier>>::new();
        for (scope, carrier) in &recorded.admitted {
            if let TraceScopeOwner::Tool {
                owner: lash::tracing::TraceToolOwner::Turn { turn_id: owner, .. },
                call_id,
            } = &scope.owner
                && owner == turn_id
            {
                calls.entry(call_id.as_str()).or_default().push(carrier);
            }
        }
        let mut violations = Vec::new();
        if calls.is_empty() {
            violations.push(format!("no tool call of turn {turn_id} was admitted"));
        }
        for (call_id, admitted) in calls {
            if admitted.len() != 1 {
                violations.push(format!(
                    "tool call {call_id} of turn {turn_id} must be admitted exactly once, but \
                     was admitted with {admitted:?}"
                ));
            }
            for carrier in admitted {
                if carrier.trace_id() != turn_anchor.trace_id() {
                    violations.push(format!(
                        "tool call {call_id}'s admission {carrier:?} is not on its turn's trace \
                         {turn_anchor:?}"
                    ));
                }
            }
        }
        violations
    }
}
