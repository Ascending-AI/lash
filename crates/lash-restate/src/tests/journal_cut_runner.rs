//! Journal cuts on the Restate server double: the runner-supplied crash point
//! of the turn-driving laws that cut a turn at a named effect (FIG-3587).
//!
//! lash-restate journals every effect as a `ctx.run` named `lash:` plus the
//! effect's replay key, so a [`JournalCut`] names a run of the invocation's
//! journal. [`JournalCutRunner`] arms the double's crash plan at that run —
//! before its command is stored for [`JournalCutPoint::BeforeEffect`],
//! before its result is for [`JournalCutPoint::BeforeResult`] — and lets the
//! double do what a deployment crash does: drop the handler and retry the
//! invocation, which replays the journal the crashed attempt left.
//!
//! The turn itself runs on the harness's [`LiveTurnRunner`]
//! (`super::live_turn_probe`) as a crash-then-redrive turn. A crashed
//! execution never reports, so the cut attempt's factory reports for it: the
//! execution that retries after the cut fired panics at once, before it
//! journals anything, which the runner takes as the crashing attempt's crash
//! and hands the next execution to the redrive.

use std::sync::Arc;

use lash_conformance::{
    ConformanceTurnAttempt, ConformanceTurnRunner, JournalCut, JournalCutPoint,
};
use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer};

/// A turn runner on the server double that also cuts turns and reads the
/// replay keys the double journaled.
pub(super) struct JournalCutRunner {
    inner: Arc<dyn ConformanceTurnRunner>,
    server: RestateTestServer,
}

/// The `ctx.run` name lash-restate journals an effect under.
fn run_name(replay_key: &str) -> String {
    format!("lash:{replay_key}")
}

impl JournalCutRunner {
    pub(super) fn shared(
        inner: Arc<dyn ConformanceTurnRunner>,
        server: RestateTestServer,
    ) -> Arc<dyn ConformanceTurnRunner> {
        Arc::new(Self { inner, server })
    }
}

#[async_trait::async_trait]
impl ConformanceTurnRunner for JournalCutRunner {
    async fn run_turn(&self, admitted: lash_core::AdmittedScope, attempt: ConformanceTurnAttempt) {
        self.inner.run_turn(admitted, attempt).await;
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    ) {
        self.inner
            .run_crashed_then_redriven_turn(admitted, crashing, redrive)
            .await;
    }

    /// The replay keys of every run the double journaled whose name spells
    /// `scope`'s session and turn, in journal order across invocations.
    async fn recorded_replay_keys(&self, scope: &lash_core::ExecutionScope) -> Option<Vec<String>> {
        let lash_core::ExecutionScope::Turn {
            session_id,
            turn_id,
        } = scope
        else {
            return None;
        };
        let mut keys = Vec::new();
        for invocation in self.server.invocations() {
            for entry in self.server.journal(&invocation.id).unwrap_or_default() {
                if let Some(key) = entry
                    .name
                    .as_deref()
                    .and_then(|name| name.strip_prefix("lash:"))
                    && key.contains(session_id.as_str())
                    && key.contains(turn_id.as_str())
                {
                    keys.push(key.to_owned());
                }
            }
        }
        Some(keys)
    }

    async fn run_cut_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        cut: JournalCut,
        attempt: ConformanceTurnAttempt,
        redrive: ConformanceTurnAttempt,
    ) {
        let crashes_before = self.server.stats().crashes;
        let name = run_name(&cut.replay_key);
        self.server.crash_on(CrashRule::new(match cut.at {
            JournalCutPoint::BeforeEffect => CrashPoint::BeforeRun { name },
            JournalCutPoint::BeforeResult => CrashPoint::BeforeRunResult { name: Some(name) },
        }));
        let server = self.server.clone();
        let cut_attempt: ConformanceTurnAttempt = Arc::new(move |scoped| {
            if server.stats().crashes > crashes_before {
                // The retry after the cut: report the crash for the execution
                // the double dropped, before journaling anything.
                return Box::pin(async {
                    panic!("the journal cut crashed this attempt; the redrive replays its journal")
                });
            }
            attempt(scoped)
        });
        self.inner
            .run_crashed_then_redriven_turn(admitted, cut_attempt, redrive)
            .await;
        assert!(
            self.server.stats().crashes > crashes_before,
            "the journal cut at {cut:?} fired"
        );
    }

    fn process_work(
        &self,
        watched: lash_core::WatchedRegistry,
        worker: lash_core_worker::DurableProcessWorker,
    ) -> lash_core::ProcessWorkWiring {
        self.inner.process_work(watched, worker)
    }
}

// FIG-3587's cell binding-drift law on the server double: the tier cuts the
// law's first attempt at the run the law names, and the double retries it.
//
// Parked (scripts/deferred-law-invocations.toml): the cases that cut after the
// probe's result was recorded refuse on redrive instead of completing from
// the journal — a drifted binding is served only from settled keys, and
// Restate's positional journal has none (FIG-3719). The cases that cut before
// the result is recorded pass here.
mod on_the_server_double {
    use super::super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

    lash_conformance::cell_binding_drift_tests!(
        #[ignore = "parked on the server double until FIG-3719"]
        {
            let harness =
                LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process())
                    .await;
            let server = harness
                .server_double()
                .unwrap_or_else(|| panic!("the in-process harness runs on the server double"));
            let runner = super::JournalCutRunner::shared(harness.turn_runner(), server);
            let host = harness.endpoint_host();
            let prefix: &'static str = Box::leak(
                format!("restate-binding-drift-{}", harness.run_nonce()).into_boxed_str(),
            );
            (
                harness,
                prefix,
                host,
                runner,
                vec![super::super::conformance_and_poison::drift_law_rlm_factory()],
            )
        }
    );
}
