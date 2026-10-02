//! Live transport cuts between a spending entry, its detached send and SQL.
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
#![allow(clippy::disallowed_methods)]
use lash_core::llm::types::{LlmOutputPart, LlmResponse};
use lash_core::{RuntimeOwner, SessionId};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashCount, CrashPoint, CrashRule};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
#[path = "usage_accounting_windows/support.rs"]
mod support;
const BOUND: Duration = Duration::from_secs(60);

struct Probe;
fn definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:usage_window_probe",
        "usage_window_probe",
        "Continue to the second paid call",
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"object"}),
    )
}
#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "usage_window_probe").then(|| Arc::new(definition().contract()))
    }
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({})).into()
    }
}

fn env(name: &str) -> String {
    std::env::var(name).expect("live recipe environment")
}
fn tag(label: &str) -> String {
    format!(
        "usage-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

async fn backend(
    label: &str,
    fail_first: bool,
) -> (
    LiveRestateBackend<dyn lash_core::StoreSet>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counts = Arc::clone(&attempts);
    let failures = Arc::new(AtomicUsize::new(0));
    let failed = Arc::clone(&failures);
    let backend = LiveRestateBackend::start_with_store_set(
        LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("CW_BIND").parse().unwrap(),
            endpoint_url: env("CW_URL"),
            run_tag: tag(label),
            namespace: lash_restate::RestateNamespace::default(),
        },
        move |clock| async move {
            let inner: Arc<dyn lash_core::StoreSet> = Arc::new(
                lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                    .await
                    .unwrap(),
            );
            let usage = Arc::new(support::ProjectionStore {
                inner: inner.usage_accounting(),
                fail_first,
                attempts: counts,
                failures: failed,
            });
            Ok(Arc::new(support::Stores { inner, usage }) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("start live usage backend");
    (backend, attempts, failures)
}

fn core(
    backend: &LiveRestateBackend<dyn lash_core::StoreSet>,
    calls: Arc<AtomicUsize>,
    cut: bool,
    owner: RuntimeOwner,
) -> lash::LashCore {
    let cut_backend = backend.clone();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("usage-window")
        .complete(move |_| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let backend = cut_backend.clone();
            let owner = owner.clone();
            async move {
                assert!(index < 2, "journal replay never buys an answered call");
                if index == 1 && cut {
                    tokio::time::timeout(BOUND, async {
                        loop {
                            let runs = backend
                                .stores()
                                .usage_accounting()
                                .load_usage_run_page(
                                    &owner,
                                    lash_core::UsageRunFilter::All,
                                    None,
                                    std::num::NonZeroU32::new(10).unwrap(),
                                )
                                .await
                                .unwrap();
                            if runs.runs.iter().any(|run| {
                                run.admission.as_ref().is_some_and(|admission| {
                                    admission.requested_model == "mock-model"
                                }) && run.state.is_settled()
                            }) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .expect("first paid call projected before the cut");
                    backend.crash_on(
                        CrashRule::new(CrashPoint::BeforeFrame {
                            ty: MessageType::OneWayCallCommand,
                        })
                        .service("LashTurn")
                        .handler("run"),
                    );
                }
                Ok(LlmResponse {
                    parts: if index == 0 {
                        vec![LlmOutputPart::ToolCall {
                            call_id: "usage-window-next".into(),
                            tool_name: "usage_window_probe".into(),
                            input_json: "{}".into(),
                            replay: None,
                        }]
                    } else {
                        vec![LlmOutputPart::Text {
                            text: "done".into(),
                            response_meta: None,
                        }]
                    },
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    provider_usage: Some(serde_json::json!({"call": index})),
                    usage: lash_core::llm::types::LlmUsage {
                        input_tokens: 20,
                        output_tokens: 5,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    // Recovery runs only when a law asks for a pass, and a pass waits on its
    // deliveries for the law's own bound.
    lash::LashCore::standard_builder(
        lash_core::testing::runtime_helpers::LayeredBackend::over(backend.lash_backend())
            .with_session_work(backend.explicit_reconcile_session_work())
            .into_backend(),
    )
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .recovery_pass_budget(lash::RecoveryPassBudget {
        tick_wait: BOUND,
        ..Default::default()
    })
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .serve_test_model(
        provider,
        lash_core::ModelMetadata::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .unwrap(),
    )
    .tools(Arc::new(Probe) as Arc<dyn lash_core::ToolProvider>)
    .build(lash_core::testing::runtime_lease_owner())
    .unwrap()
}

async fn charged(
    backend: &LiveRestateBackend<dyn lash_core::StoreSet>,
    owner: &RuntimeOwner,
    expected: u64,
    unknown: u64,
) {
    let usage = tokio::time::timeout(BOUND, async {
        loop {
            let usage = backend
                .stores()
                .usage_accounting()
                .load_owner_usage(owner)
                .await
                .unwrap();
            if usage.completeness.open_runs == 0 {
                break usage;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("usage delivered or retired");
    assert_eq!(usage.completeness.unknown_runs, unknown);
    assert_eq!(usage.completeness.conflicted_runs, 0);
    assert_eq!(usage.completeness.unreported_attempts, 0);
    assert_eq!(
        usage
            .rows
            .iter()
            .map(|row| row.reported_attempts)
            .sum::<u64>(),
        expected
    );
    assert_eq!(
        usage
            .rows
            .iter()
            .map(|row| row.usage.input_tokens)
            .sum::<i64>(),
        i64::try_from(expected).unwrap() * 20
    );
    assert_eq!(
        usage
            .rows
            .iter()
            .map(|row| row.usage.output_tokens)
            .sum::<i64>(),
        i64::try_from(expected).unwrap() * 5
    );
    let facts = backend
        .stores()
        .usage_accounting()
        .load_usage_fact_page(owner, None, std::num::NonZeroU32::new(10).unwrap())
        .await
        .unwrap();
    assert_eq!(facts.facts.len(), usize::try_from(expected).unwrap());
    assert_eq!(
        facts
            .facts
            .iter()
            .map(|fact| fact.identity())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        facts.facts.len()
    );
}

async fn cut_before_send(kill: bool) {
    let (backend, _, _) = backend(if kill { "p6" } else { "p3" }, false).await;
    let mut crashed = CrashCount::new();
    assert!(backend.on_crash(crashed.listener()));
    let calls = Arc::new(AtomicUsize::new(0));
    let id = SessionId::from(tag("cut"));
    let owner = RuntimeOwner::Session(id.clone());
    let core = core(&backend, Arc::clone(&calls), true, owner.clone());
    core.session(&id)
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .unwrap();
    let session = core.session(&id).open().await.unwrap();
    let send = tokio::spawn(async move {
        session
            .send(lash::TurnInput::text("two paid calls"))
            .output()
            .await
    });
    tokio::time::timeout(BOUND, crashed.wait_until(1))
        .await
        .expect("cut before second settle send")
        .expect("observe the crash");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let invocations = backend.invocations().await.unwrap();
    let root = invocations
        .iter()
        .find(|row| row.target.starts_with("LashTurn/") && row.status != "completed")
        .expect("root at the cut");
    let journal = backend.journal_entries(&root.id).await.unwrap();
    assert!(
        journal.iter().any(|(name, bytes)| name.contains("Run")
            && String::from_utf8_lossy(bytes).contains("\"call\":1")),
        "the second paid effect entry is retained: {:?}",
        backend.journal(&root.id).await.unwrap()
    );
    let before = backend
        .stores()
        .usage_accounting()
        .load_owner_usage(&owner)
        .await
        .unwrap();
    assert_eq!(
        before.completeness.open_runs, 1,
        "unsent settlement leaves one liability"
    );
    let factory = backend.stores().session_store_factory();
    let head = factory
        .load_session_head_meta(&id)
        .await
        .unwrap()
        .unwrap()
        .head_revision;
    let lost = factory
        .non_terminal_roots_page(None, std::num::NonZeroUsize::new(2).unwrap())
        .await
        .unwrap()
        .into_iter()
        .map(|open| open.target.root)
        .collect::<Vec<_>>();
    let [lost] = lost.as_slice() else {
        panic!("one open root at the cut: {lost:?}");
    };
    if kill {
        assert!(
            backend.kill_and_await(&root.id).await.unwrap(),
            "admin kill confirms the root ended"
        );
        assert_eq!(
            backend
                .stores()
                .usage_accounting()
                .load_owner_usage(&owner)
                .await
                .unwrap()
                .completeness
                .open_runs,
            1,
            "kill preserves the unsent liability until retirement"
        );
    }
    backend.start_serving().await.unwrap();
    if kill {
        backend
            .lash_backend()
            .effect_host()
            .drain_usage_accounting(&owner)
            .await
            .unwrap();
        charged(&backend, &owner, 1, 1).await;
        let liabilities = backend
            .stores()
            .usage_accounting()
            .load_usage_run_page(
                &owner,
                lash_core::UsageRunFilter::Unresolved,
                None,
                std::num::NonZeroU32::new(10).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(liabilities.runs.len(), 1);
        assert_eq!(
            liabilities.runs[0].state.outcome(),
            Some(&lash_core::UsageRunOutcome::Unknown(
                lash_core::UsageUnknownReason::OwnerRetired
            ))
        );
        assert_eq!(
            factory
                .load_session_head_meta(&id)
                .await
                .unwrap()
                .unwrap()
                .head_revision,
            head,
            "draining accounting moves no head"
        );
        send.abort();
        let _ = send.await;
        // Nothing of the killed root outlives it: the recovery pass that
        // ends the root lost also closes its scope, which releases what the
        // aborted send left waiting on its terminal.
        let driver = backend
            .restate()
            .session_work_engine()
            .driver_slot()
            .installed()
            .expect("the core installed its driver");
        tokio::time::timeout(
            BOUND,
            driver.reconcile(
                &lash_core::engine::ReconcileCursor::default(),
                std::num::NonZeroUsize::MIN.saturating_add(63),
            ),
        )
        .await
        .expect("one recovery pass")
        .unwrap();
        let terminal = factory.root_terminal(&id, lost).await.unwrap();
        assert!(
            matches!(
                terminal.as_ref().map(|terminal| &terminal.cause),
                Some(lash_core::store::RootTerminalCause::SubstrateLost { .. })
            ),
            "the pass ends the killed root substrate-lost: {terminal:?}"
        );
        assert_eq!(
            backend
                .stores()
                .obligation_ledger(lash_core::store::ObligationKind::ScopeClose)
                .state(&lash_core::store::scope_close_obligation_id(&id, lost))
                .await
                .unwrap(),
            Some(lash_core::store::ObligationState::Delivered),
            "the pass that ended the root delivered its scope close"
        );
    } else {
        tokio::time::timeout(BOUND, send)
            .await
            .unwrap()
            .unwrap()
            .expect("replay completes the root");
        charged(&backend, &owner, 2, 0).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "journaling and replay buy no extra call"
    );
    backend.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by the usage-accounting live Restate recipe"]
async fn live_restate_usage_crash_p3_journaled_not_sent() {
    cut_before_send(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by the usage-accounting live Restate recipe"]
async fn live_restate_usage_crash_p6_killed_between_entry_and_send() {
    cut_before_send(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by the usage-accounting live Restate recipe"]
async fn live_restate_usage_crash_p4_sent_not_projected() {
    let (backend, attempts, failures) = backend("p4", true).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let id = SessionId::from(tag("projection"));
    let core = core(
        &backend,
        Arc::clone(&calls),
        false,
        RuntimeOwner::Session(id.clone()),
    );
    core.session(&id)
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "mock-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .unwrap();
    let session = core.session(&id).open().await.unwrap();
    tokio::time::timeout(
        BOUND,
        session
            .send(lash::TurnInput::text("projection retry"))
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    charged(&backend, &RuntimeOwner::Session(id), 2, 0).await;
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    assert!(
        attempts.load(Ordering::SeqCst) >= 3,
        "two model settlements and the failed pre-projection attempt"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    backend.finish().await;
}
