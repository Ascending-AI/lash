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
// law's first attempt at the run the law names, and the double retries it. A
// drifted binding whose result the journal recorded is served by the replay;
// one whose run the replay reaches live refuses (FIG-3719).
mod on_the_server_double {
    use super::super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

    lash_conformance::cell_binding_drift_tests!({
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let server = harness
            .server_double()
            .unwrap_or_else(|| panic!("the in-process harness runs on the server double"));
        let runner = super::JournalCutRunner::shared(harness.turn_runner(), server);
        let host = harness.endpoint_host();
        let prefix: &'static str =
            Box::leak(format!("restate-binding-drift-{}", harness.run_nonce()).into_boxed_str());
        let stores = harness.law_stores();
        (
            harness,
            prefix,
            host,
            stores,
            runner,
            vec![super::super::conformance_and_poison::drift_law_rlm_factory()],
        )
    });
}

// FIG-3719 on the server double: a served-only effect that acts outside a
// `ctx.run` closure — the process start a drifted orchestrating binding
// (`agents.spawn`, for one) issues, or a retry sleep — refuses up front, so
// the drifted command parks with no process started and no sleep journaled.
mod served_only_outside_a_run {
    use std::sync::Arc;

    use lash_conformance::{ConformanceTurnAttempt, ConformanceTurnEnd};
    use lash_core::{
        CommandJournalGuard, EffectAddress, ProcessCommand, ProcessId, RuntimeAttribution,
        RuntimeEffectCommand, RuntimeEffectControllerError, RuntimeEffectEnvelope,
        RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeErrorCode, ServedOnlyRange,
    };
    use lash_sansio::{SessionId, TurnId};

    use super::super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};

    struct NoopProcessWork;

    #[async_trait::async_trait]
    impl lash_core::ProcessWorkSubstrate for NoopProcessWork {
        async fn admit_pending_processes(
            &self,
            _reason: &str,
        ) -> Result<lash_core::facade_support::ProcessAdmissionReport, lash_core::PluginError>
        {
            Ok(lash_core::facade_support::ProcessAdmissionReport::default())
        }

        async fn await_process_terminal(
            &self,
            process_ref: &lash_core::ProcessRef,
        ) -> Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
            panic!("unexpected terminal wait for {process_ref}")
        }
    }

    /// What the served-only effects answered: the process start's result, the
    /// sleep's result, and the refusal the command's guard tripped on.
    type Answers = (
        Result<(), RuntimeEffectControllerError>,
        Result<(), RuntimeEffectControllerError>,
        Option<RuntimeEffectControllerError>,
    );

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_drifted_process_start_or_sleep_parks_without_acting() {
        let harness =
            LiveConformanceHarness::start_for_tool_children_on(HarnessServer::in_process()).await;
        let server = harness
            .server_double()
            .unwrap_or_else(|| panic!("the in-process harness runs on the server double"));
        let registry = harness.law_stores().process_registry();
        let nonce = harness.run_nonce();
        let session_id = SessionId::from(format!("served-only-{nonce}"));
        let turn_id = TurnId::from("served-only-turn");
        let process_id = ProcessId::from(format!("served-only-probe-{nonce}"));
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel::<Answers>();
        let job: ConformanceTurnAttempt = {
            let registry = Arc::clone(&registry);
            let process_id = process_id.clone();
            Arc::new(move |scoped| {
                let registry = Arc::clone(&registry);
                let process_id = process_id.clone();
                let answers = answers.clone();
                Box::pin(async move {
                    let scope = scoped.execution_scope().clone();
                    let namespace = format!("{}:exec:lk2", scope.id());
                    let guard =
                        Arc::new(CommandJournalGuard::open().served_only(ServedOnlyRange {
                            lower: format!("{namespace}:"),
                            upper: format!("{namespace}:~seal"),
                            refusal: RuntimeEffectControllerError::new(
                                RuntimeErrorCode::LashlangCellBindingDrift,
                                "code cell binding `agents.spawn` (tool `spawn_agent`) is \
                                 changed in the live tool registry",
                            ),
                        }));
                    let guarded = scoped.with_journal_guard(Arc::clone(&guard));
                    let invocation = |key: String| {
                        RuntimeEffectInvocation::new(
                            EffectAddress::new(scope.clone(), key.clone())
                                .unwrap_or_else(|error| panic!("address {key}: {error}")),
                            RuntimeAttribution::none(),
                            key,
                        )
                    };
                    let registration = lash_core::ProcessRegistration::new(
                        process_id.clone(),
                        lash_core::ProcessInput::External {
                            metadata: serde_json::json!({ "fixture": "served-only" }),
                        },
                        lash_core::RecoveryContract::ExternallyOwned,
                        lash_core::ProcessProvenance::host(),
                        lash_core::ProcessLifecyclePolicy::new(
                            lash_core::ParentScope::Host,
                            lash_core::OnParentEnd::Abandon,
                        ),
                    );
                    let started = guarded
                        .execute_effect(
                            RuntimeEffectEnvelope::new(
                                invocation(format!(
                                    "{namespace}:0000000000:process:start:{process_id}"
                                )),
                                RuntimeEffectCommand::Process {
                                    command: Box::new(ProcessCommand::Start {
                                        registration,
                                        observers: Vec::new(),
                                        env_spec: None,
                                        execution_context: Box::default(),
                                    }),
                                },
                            ),
                            RuntimeEffectLocalExecutor::processes(
                                registry,
                                Arc::new(NoopProcessWork),
                            ),
                        )
                        .await
                        .map(|_| ());
                    let slept = guarded
                        .execute_effect(
                            RuntimeEffectEnvelope::new(
                                invocation(format!("{namespace}:0000000000:attempt:1:sleep")),
                                RuntimeEffectCommand::Sleep {
                                    spec: lash_core::SleepSpec::For {
                                        duration_ms: 60_000,
                                    },
                                },
                            ),
                            RuntimeEffectLocalExecutor::sleep(
                                tokio_util::sync::CancellationToken::new(),
                            ),
                        )
                        .await
                        .map(|_| ());
                    let _ = answers.send((started, slept, guard.tripped()));
                    ConformanceTurnEnd::Settled
                })
            })
        };
        harness
            .turn_runner()
            .run_turn(lash_core::AdmittedScope::turn(&session_id, &turn_id), job)
            .await;
        let (started, slept, tripped) = answered
            .recv()
            .await
            .unwrap_or_else(|| panic!("the served-only job ran"));
        for (effect, answer) in [("process start", started), ("sleep", slept)] {
            let refusal = answer.expect_err("a served-only effect outside a run refuses");
            assert_eq!(
                refusal.code,
                RuntimeErrorCode::LashlangCellBindingDrift,
                "the {effect} refuses with the drift: {refusal:?}"
            );
        }
        assert_eq!(
            tripped.map(|refusal| refusal.code),
            Some(RuntimeErrorCode::LashlangCellBindingDrift),
            "the refusal trips the command's guard, so the turn parks"
        );
        assert!(
            registry
                .get_process(&process_id)
                .await
                .unwrap_or_else(|error| panic!("read the registry: {error}"))
                .is_none(),
            "the drifted command started no process"
        );
        assert!(
            server
                .invocations()
                .iter()
                .all(|invocation| !invocation.target.contains(process_id.as_str())),
            "the drifted command submitted no process workflow"
        );
        harness.finish().await;
    }
}
