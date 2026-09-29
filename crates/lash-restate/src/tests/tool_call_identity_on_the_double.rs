//! The tool-call identity laws (FIG-4079) on the Restate server double.
//!
//! Two tiers over `lash-restate-test`'s in-process server: in process, the
//! double answers every await in-stream; the double tier replays the handler
//! at every await it cannot answer from its journal, the
//! `INACTIVITY_TIMEOUT=0s` mode. A crashed attempt fails its invocation
//! retryably and the double replays it into the redrive. The live tier is
//! registered beside the other live laws in `conformance_and_poison.rs`.

use std::sync::Arc;

use lash_core::EffectHost;

/// The tiers' [`lash_conformance::ConformanceTurnRunner`]: `run_in_handler`
/// lends the attempt the scoped controller the invocation's journal owns.
struct DoubleTurnRunner {
    backend: lash_restate_test::RestateTestBackend,
}

/// `attempt` as a `HandlerAttempt`: the law reads the attempt's answer off
/// its own channel.
fn into_handler_attempt(
    attempt: lash_conformance::ConformanceTurnAttempt,
) -> lash_restate_test::HandlerAttempt {
    Arc::new(
        move |controller| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            let attempt = Arc::clone(&attempt);
            Box::pin(async move {
                attempt(controller).await;
            })
        },
    )
}

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
    /// The double keeps every journal it records; a finished scenario's
    /// completed ones are dead weight to the next.
    async fn scenario_finished(&self) {
        self.backend.server().drop_completed_journals();
    }

    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_in_handler(admitted, into_handler_attempt(attempt))
            .await
            .unwrap_or_else(|error| panic!("the double's handler runs the law's turn: {error}"));
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        crashing: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        self.backend
            .run_crashed_then_redriven(
                admitted,
                into_handler_attempt(crashing),
                into_handler_attempt(redrive),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("the double crashes and redrives the law's turn: {error}")
            });
    }

    /// The replay keys of every run the double journaled that names `scope`'s
    /// session, in journal order across invocations: a turn's group children
    /// journal in invocations of their own.
    async fn recorded_replay_keys(&self, scope: &lash_core::ExecutionScope) -> Option<Vec<String>> {
        let lash_core::ExecutionScope::Turn { session_id, .. } = scope else {
            return None;
        };
        let server = self.backend.server();
        let mut keys = Vec::new();
        for invocation in server.invocations() {
            for entry in server.journal(&invocation.id).unwrap_or_default() {
                if let Some(key) = entry
                    .name
                    .as_deref()
                    .and_then(|name| name.strip_prefix("lash:"))
                    && key.contains(session_id.as_str())
                {
                    keys.push(key.to_owned());
                }
            }
        }
        Some(keys)
    }

    /// Arms the double's crash plan at the run the cut names — before its
    /// command is stored, or before its result is — and lets the double do
    /// what a deployment crash does: drop the handler of whichever invocation
    /// journals that run (the turn's own, or a group child's) and retry it,
    /// replaying its journal. Every execution of the turn before the cut
    /// fired runs `attempt`, every one after it `redrive`.
    async fn run_cut_then_redriven_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        cut: lash_conformance::JournalCut,
        attempt: lash_conformance::ConformanceTurnAttempt,
        redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        let server = self.backend.server().clone();
        let crashes_before = server.stats().crashes;
        let name = format!("lash:{}", cut.replay_key);
        server.crash_on(lash_restate_test::CrashRule::new(match cut.at {
            lash_conformance::JournalCutPoint::BeforeEffect => {
                lash_restate_test::CrashPoint::BeforeRun { name }
            }
            lash_conformance::JournalCutPoint::BeforeResult => {
                lash_restate_test::CrashPoint::BeforeRunResult { name: Some(name) }
            }
        }));
        let crashed = server.clone();
        let cut_attempt: lash_conformance::ConformanceTurnAttempt = Arc::new(move |scoped| {
            if crashed.stats().crashes > crashes_before {
                redrive(scoped)
            } else {
                attempt(scoped)
            }
        });
        self.run_turn(admitted, cut_attempt).await;
        assert!(
            server.stats().crashes > crashes_before,
            "the journal cut at {cut:?} fired"
        );
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "test fixture: `LASH_RESTATE_TEST_SEED` replays one printed seed of the server double"
)]
async fn tier(
    label: &str,
    always_replay: bool,
) -> (
    lash_restate_test::RestateTestBackend,
    lash_conformance::ToolCallIdentityTier,
) {
    let seed = std::env::var("LASH_RESTATE_TEST_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or_else(|| {
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos() & u128::from(u64::MAX))
                    .unwrap_or(0),
            )
            .unwrap_or(0)
        });
    eprintln!("tool-call identity {label}: LASH_RESTATE_TEST_SEED={seed}");
    let double = lash_restate_test::backend(
        seed,
        lash_restate_test::ServerConfig {
            always_replay,
            ..lash_restate_test::ServerConfig::default()
        },
    )
    .await
    .unwrap_or_else(|error| panic!("start the Restate server double: {error}"));
    let tier = lash_conformance::ToolCallIdentityTier {
        prefix: format!("identity-{label}-{seed}"),
        effect_host: double.lash_backend().effect_host() as Arc<dyn EffectHost>,
        stores: Arc::clone(double.engine_stores()),
        runner: Arc::new(DoubleTurnRunner {
            backend: double.clone(),
        }),
    };
    (double, tier)
}

mod in_process {
    lash_conformance::tool_call_identity_tests!({ super::tier("in-process", false).await });
}

mod double {
    lash_conformance::tool_call_identity_tests!({ super::tier("double", true).await });
}
