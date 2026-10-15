//! A recording trace sink for the trace-scope laws (FIG-5363, FIG-5382,
//! FIG-6299): the publication oracle a core's trace runtime is served with.
//!
//! Every process's runtime appends to one collector, so the laws read what
//! every owner published. A container (a turn, a tool call) closes on the
//! owner whose acknowledged commit wrote its terminal (P1), so its closing
//! record is published at most once across every owner, and exactly once
//! unless that owner lost its acknowledgement or its life after the commit
//! (D4's at-most-once window).
//!
//! Each runtime installs two consumers ahead of the collector (L1(g)): a
//! sink that panics on every third append and a flight recorder whose
//! snapshot callback panics. Sink acceptance is per sink: each is offered
//! every record the collector holds, whatever the others did. (The
//! matrix's virtual clock reports no slow operation, so the recorder's
//! callback panic is pinned by the runtime's own containment law.)

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::tracing::{
    FlightRecorderSettings, FlightRecorderSink, SpanRole, TraceCarrier, TraceRecord, TraceScopeId,
    TraceScopeOwner, TraceSink, TraceSinkError, TraceSpanKind,
};
use lash_sansio::sync::MutexExt as _;

/// The collector every process's runtime publishes to.
#[derive(Clone, Default)]
pub struct Telemetry {
    published: Arc<Mutex<Vec<TraceRecord>>>,
    /// Appends offered to the sink that panics on every third.
    panicking: Arc<AtomicUsize>,
    /// Appends the flight recorder forwarded.
    recorded: Arc<AtomicUsize>,
}

/// A sink that counts every append it is offered and panics on every
/// third.
struct PanicEveryThird(Arc<AtomicUsize>);

impl TraceSink for PanicEveryThird {
    fn append(&self, _record: &TraceRecord) -> Result<(), TraceSinkError> {
        let offered = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(!offered.is_multiple_of(3), "the third append panics");
        Ok(())
    }
}

/// A sink that counts what it is forwarded.
struct Counting(Arc<AtomicUsize>);

impl TraceSink for Counting {
    fn append(&self, _record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl TraceSink for Telemetry {
    fn append(&self, record: &TraceRecord) -> Result<(), TraceSinkError> {
        self.published.lock_recover().push(record.clone());
        Ok(())
    }
}

impl Telemetry {
    /// A trace runtime that publishes to this collector, with lash's own
    /// identity: no adapter, so producers root sampled traces.
    /// The failing consumers go first: one whose failure stopped later
    /// sinks would starve the collector.
    pub fn runtime(&self) -> lash_core::runtime::TraceRuntime {
        let recorder = FlightRecorderSink::new(
            Counting(Arc::clone(&self.recorded)),
            FlightRecorderSettings {
                slow_threshold: std::time::Duration::ZERO,
                minimum_interval: std::time::Duration::ZERO,
                ..FlightRecorderSettings::default()
            },
            |_snapshot| panic!("the snapshot callback panics"),
        );
        lash_core::runtime::TraceRuntime::new(Arc::new(lash_core::runtime::SystemClock))
            .with_trace_sink(Arc::new(PanicEveryThird(Arc::clone(&self.panicking))))
            .with_trace_sink(Arc::new(recorder))
            .with_trace_sink(Arc::new(self.clone()))
    }

    /// How publication broke across every owner (L1(c), (f), (g)): unless
    /// each failing consumer was offered every record the collector holds,
    /// and every published parent is a span some owner published. Only
    /// when `lossy` may a parent be missing, and only a container's whose
    /// opening record was published: an owner lost its acknowledgement or
    /// its life between that container's terminal commit and its close.
    pub fn publication_violations(&self, lossy: bool) -> Vec<String> {
        let published = self.published.lock_recover();
        let mut violations = Vec::new();
        for (consumer, offered) in [
            ("the panicking sink", self.panicking.load(Ordering::SeqCst)),
            ("the flight recorder", self.recorded.load(Ordering::SeqCst)),
        ] {
            if offered != published.len() {
                violations.push(format!(
                    "{consumer} was offered {offered} records, the collector holds {}",
                    published.len()
                ));
            }
        }
        let positions = || {
            published
                .iter()
                .filter_map(|record| record.position.as_ref())
        };
        let closed: HashSet<_> = positions()
            .filter(|position| position.publishes())
            .map(|position| position.span.span_id())
            .collect();
        let opened: HashSet<_> = positions()
            .filter(|position| position.role == SpanRole::Opens)
            .map(|position| position.span.span_id())
            .collect();
        for position in positions() {
            let Some(parent) = &position.parent else {
                continue;
            };
            let parent = parent.span_id();
            if closed.contains(&parent) || (lossy && opened.contains(&parent)) {
                continue;
            }
            violations.push(format!(
                "a {:?} {:?} record names parent {parent:?}, which no owner published",
                position.kind, position.role
            ));
        }
        violations
    }

    /// The collector of a process started after this one's: the same
    /// collector, so the laws read every owner's publications.
    #[must_use]
    pub fn restarted(&self) -> Self {
        self.clone()
    }

    /// Every record published so far.
    pub fn published(&self) -> Vec<TraceRecord> {
        self.published.lock_recover().clone()
    }

    /// How `session`'s first turn broke its trace: unless every record of
    /// the turn carries a position on the trace of the turn's span, the
    /// turn's opening and closing records name one span, and that span
    /// closes once (at most once when `lossy`: an owner may have lost its
    /// acknowledgement or its life after the terminal commit). Span ids are
    /// unique across every published closing or whole record.
    pub fn first_turn_violations(&self, session: &str, lossy: bool) -> Vec<String> {
        let published = self.published.lock_recover();
        let mut violations = unique_span_violations(&published);
        let Some((scope, span)) = first_turn(&published, session) else {
            violations.push(format!("no turn of session {session} was published"));
            return violations;
        };
        let TraceScopeOwner::Turn { turn_id, .. } = &scope.owner else {
            unreachable!("found a turn scope");
        };
        let turn_records = published.iter().filter(|record| {
            record.context.turn_id.as_ref() == Some(turn_id)
                || record
                    .position
                    .as_ref()
                    .is_some_and(|position| position.scope.as_ref() == Some(&scope))
        });
        let mut closes = 0;
        for record in turn_records {
            let Some(position) = &record.position else {
                violations.push(format!(
                    "a record of turn {turn_id} carries no position: {:?}",
                    record.event.kind()
                ));
                continue;
            };
            if position.span.trace_id() != span.trace_id() {
                violations.push(format!(
                    "a {:?} record of turn {turn_id} is on trace {:?}, not its turn's {:?}",
                    record.event.kind(),
                    position.span.trace_id(),
                    span.trace_id()
                ));
            }
            if position.kind == TraceSpanKind::Agent && position.scope.as_ref() == Some(&scope) {
                if position.span.span_id() != span.span_id() {
                    violations.push(format!(
                        "turn {turn_id}'s records name two spans: {:?} and {:?}",
                        span.span_id(),
                        position.span.span_id()
                    ));
                }
                closes += usize::from(position.role == SpanRole::Closes);
            }
        }
        if closes > 1 || (closes == 0 && !lossy) {
            violations.push(format!(
                "turn {turn_id}'s span must close once, but closed {closes} times"
            ));
        }
        violations
    }

    /// How the tool calls of `session`'s first turn broke their spans
    /// (FIG-5382): unless each call's span closes once (at most once when
    /// `lossy`), on the trace of its turn, under a parent on that trace.
    pub fn first_turn_tool_violations(&self, session: &str, lossy: bool) -> Vec<String> {
        let published = self.published.lock_recover();
        let Some((scope, span)) = first_turn(&published, session) else {
            return vec![format!("no turn of session {session} was published")];
        };
        let TraceScopeOwner::Turn { turn_id, .. } = &scope.owner else {
            unreachable!("found a turn scope");
        };
        let mut calls = BTreeMap::<&str, Vec<&TraceCarrier>>::new();
        let mut violations = Vec::new();
        for record in published.iter() {
            let Some(position) = &record.position else {
                continue;
            };
            let Some(TraceScopeOwner::Tool {
                owner: lash::tracing::TraceToolOwner::Turn { turn_id: owner, .. },
                call_id,
            }) = position.scope.as_ref().map(|scope| &scope.owner)
            else {
                continue;
            };
            if owner != turn_id || position.kind != TraceSpanKind::Tool {
                continue;
            }
            let closes = calls.entry(call_id.as_str()).or_default();
            if position.role == SpanRole::Closes {
                closes.push(&position.span);
            }
            if position.span.trace_id() != span.trace_id()
                || position
                    .parent
                    .as_ref()
                    .is_none_or(|parent| parent.trace_id() != span.trace_id())
            {
                violations.push(format!(
                    "tool call {call_id}'s {:?} record {:?} under {:?} is not on its turn's \
                     trace {:?}",
                    position.role,
                    position.span,
                    position.parent,
                    span.trace_id()
                ));
            }
        }
        if calls.is_empty() {
            violations.push(format!("no tool call of turn {turn_id} was published"));
        }
        for (call_id, closes) in calls {
            if closes.len() > 1 || (closes.is_empty() && !lossy) {
                violations.push(format!(
                    "tool call {call_id} of turn {turn_id} must close once, but closed with \
                     {closes:?}"
                ));
            }
        }
        violations
    }
}

/// The scope and span of `session`'s first turn, from its first published
/// agent record.
fn first_turn(published: &[TraceRecord], session: &str) -> Option<(TraceScopeId, TraceCarrier)> {
    published.iter().find_map(|record| {
        let position = record.position.as_ref()?;
        let scope = position.scope.as_ref()?;
        (position.kind == TraceSpanKind::Agent
            && scope.boundary == 0
            && matches!(
                &scope.owner,
                TraceScopeOwner::Turn { session_id, .. } if session_id.as_str() == session
            ))
        .then(|| (scope.clone(), position.span.clone()))
    })
}

/// A span id named by two published closing or whole records (L1(b)).
///
/// A code cell is exempt until its span closes on the commit that consumes
/// its answer (the tracing lane T5, §3.8's cell row): until then a cell
/// that re-runs after a cut before that commit publishes its span again.
fn unique_span_violations(published: &[TraceRecord]) -> Vec<String> {
    let mut seen = HashSet::new();
    published
        .iter()
        .filter_map(|record| record.position.as_ref())
        .filter(|position| position.publishes() && position.kind != TraceSpanKind::Cell)
        .filter(|position| !seen.insert(position.span.span_id()))
        .map(|position| {
            format!(
                "span {:?} ({:?}) was published twice",
                position.span.span_id(),
                position.kind
            )
        })
        .collect()
}
