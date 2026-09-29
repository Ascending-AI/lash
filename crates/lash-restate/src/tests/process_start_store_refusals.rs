//! Claim and settlement refusals complete their journal step under the
//! production SDK, then replay without another store write or child send.

use super::*;
use lash_core::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKey, ObligationKind, ObligationLedger,
    ObligationSettlement, ObligationStanding, SettleOutcome, StalledObligation, StoreRefusal,
};
use lash_core::{RuntimeEffectControllerError, RuntimeErrorCause, RuntimeErrorCode, StoreError};
use lash_restate_test::{RestateTestBackend, ServerConfig, protocol::MessageType};
use std::num::NonZeroUsize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Claim,
    Settle,
}

impl Step {
    fn journal_name(self) -> &'static str {
        match self {
            Self::Claim => "process-start-claim",
            Self::Settle => "process-start-settle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    WriterFenced,
    Incompatible,
    Transient,
}

impl Fault {
    fn refusal(self) -> Option<StoreRefusal> {
        match self {
            Self::WriterFenced => Some(StoreRefusal::WriterFenced {
                recorded: 2,
                writable: lash_core_store::compat::VersionRange::exactly(1),
            }),
            Self::Incompatible => Some(StoreRefusal::Incompatible {
                refusal: lash_core_store::compat::CompatRefusal::Unstamped {
                    component: "fleet_format".to_owned(),
                    writing_release: Some("start-refusal-fixture".to_owned()),
                },
            }),
            Self::Transient => None,
        }
    }

    fn error(self, attempt: usize) -> Option<StoreError> {
        match self.refusal() {
            Some(refusal) => Some(refusal.into_store_error()),
            None if attempt == 1 => Some(StoreError::Contended),
            None => None,
        }
    }
}

/// Only the selected write faults. Terminal refusals remain refused on every
/// call, so a retry or a replay that touches the store fails the count law.
struct FaultingLedger {
    inner: Arc<dyn ObligationLedger>,
    step: Step,
    fault: Fault,
    claims: AtomicUsize,
    settlements: AtomicUsize,
}

#[async_trait::async_trait]
impl ObligationLedger for FaultingLedger {
    fn kind(&self) -> ObligationKind {
        self.inner.kind()
    }

    async fn arm(
        &self,
        key: &ObligationKey,
        now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        self.inner.arm(key, now_ms).await
    }

    async fn claim_due(
        &self,
        now_ms: u64,
        claim_ttl_ms: u64,
        limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        self.inner.claim_due(now_ms, claim_ttl_ms, limit).await
    }

    async fn claim(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        now_ms: u64,
        claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        let attempt = self.claims.fetch_add(1, Ordering::SeqCst) + 1;
        if self.step == Step::Claim
            && let Some(error) = self.fault.error(attempt)
        {
            return Err(error);
        }
        self.inner.claim(id, token, now_ms, claim_ttl_ms).await
    }

    async fn settle(
        &self,
        id: &ObligationId,
        token: &ClaimToken,
        settlement: ObligationSettlement,
        now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        let attempt = self.settlements.fetch_add(1, Ordering::SeqCst) + 1;
        if self.step == Step::Settle
            && let Some(error) = self.fault.error(attempt)
        {
            return Err(error);
        }
        self.inner.settle(id, token, settlement, now_ms).await
    }

    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError> {
        self.inner.rearm(id, now_ms).await
    }

    async fn list_stalled(
        &self,
        after: Option<&ObligationId>,
        limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        self.inner.list_stalled(after, limit).await
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        self.inner.count_stalled().await
    }

    async fn standing(&self, id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        self.inner.standing(id).await
    }
}

/// The SDK keeps its default run retry behaviour. The server pauses a broken
/// handler after four attempts so a stringified refusal fails promptly.
pub(super) fn server_config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.retry.max_attempts = Some(4);
    config
}

fn assert_refusal(error: &RuntimeEffectControllerError, refusal: &StoreRefusal) {
    let code = match refusal {
        StoreRefusal::WriterFenced { .. } => RuntimeErrorCode::WriterFenced,
        StoreRefusal::Incompatible { .. } => RuntimeErrorCode::StoreIncompatible,
        _ => panic!("unexpected refusal fixture"),
    };
    assert_eq!(error.code, code);
    assert_eq!(
        error.cause,
        Some(RuntimeErrorCause::StoreRefusal {
            refusal: Box::new(refusal.clone()),
        })
    );
}

pub(super) async fn start_store_fault_law<S: lash_core::StoreSet + ?Sized>(
    backend: &RestateTestBackend<S>,
    step: Step,
    fault: Fault,
) {
    let stores = backend.engine_stores();
    let registry = stores.process_registry();
    let ledger = Arc::new(FaultingLedger {
        inner: stores.obligation_ledger(ObligationKind::ProcessStart),
        step,
        fault,
        claims: AtomicUsize::new(0),
        settlements: AtomicUsize::new(0),
    });
    let returned = Arc::new(Mutex::new(Vec::new()));
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let registry = Arc::clone(&registry);
        let env_store = stores.process_env_store();
        let ledger = Arc::clone(&ledger);
        let clock = stores.clock();
        let returned = Arc::clone(&returned);
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let env_store = Arc::clone(&env_store);
            let ledger = Arc::clone(&ledger) as Arc<dyn ObligationLedger>;
            let clock = Arc::clone(&clock);
            let returned = Arc::clone(&returned);
            Box::pin(async move {
                let spec = lash_core::ProcessExecutionEnvSpec::new(
                    lash_core::PluginOptions::empty(),
                    recovery_session_policy(),
                );
                let result = scoped
                    .execute_effect(
                        start_recovery_effect("start-store-fault", &spec),
                        registry_local_executor(registry)
                            .with_process_env_store(env_store)
                            .with_process_starts(ledger, clock),
                    )
                    .await;
                returned.lock_recover().push(result.map(|_| ()));
                assert!(!crash, "redrive after the start step recorded its answer");
            })
        })
    };
    tokio::time::timeout(
        Duration::from_secs(30),
        backend.run_crashed_then_redriven(
            lash_core::AdmittedScope::turn("session", "turn"),
            attempt(true),
            attempt(false),
        ),
    )
    .await
    .expect("the SDK retry law must finish within its budget")
    .expect("a terminal store refusal must complete rather than pause in the retry loop");

    let returned = returned.lock_recover().clone();
    assert_eq!(returned.len(), 2, "the live start and its redrive returned");
    for result in &returned {
        match fault.refusal() {
            Some(refusal) => assert_refusal(
                result
                    .as_ref()
                    .expect_err("the recorded refusal reaches the caller"),
                &refusal,
            ),
            None => assert!(
                result.is_ok(),
                "the transient fault retries to success: {result:?}"
            ),
        }
    }
    let transient = fault == Fault::Transient;
    assert_eq!(
        ledger.claims.load(Ordering::SeqCst),
        if transient && step == Step::Claim {
            2
        } else {
            1
        },
        "redrive must never re-execute the recorded claim"
    );
    assert_eq!(
        ledger.settlements.load(Ordering::SeqCst),
        if !transient && step == Step::Claim {
            0
        } else if transient && step == Step::Settle {
            2
        } else {
            1
        },
        "a refused settlement is attempted exactly once and replayed"
    );

    let handler = backend
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .expect("the SDK handler ran");
    assert_eq!(handler.status, "completed");
    assert_eq!(
        handler.retry_count,
        if transient { 2 } else { 1 },
        "only the deliberate crash and transient fault retry the handler"
    );
    let journal = backend
        .server()
        .journal(&handler.id)
        .expect("the handler journal");
    let sends = journal
        .iter()
        .filter(|entry| entry.ty == MessageType::OneWayCallCommand)
        .count();
    assert_eq!(
        sends,
        usize::from(transient || step == Step::Settle),
        "a claim refusal sends nothing; a settlement refusal submits once, including redrive"
    );
    let step_runs = journal
        .iter()
        .filter(|entry| {
            entry.ty == MessageType::RunCommand
                && entry
                    .name
                    .as_deref()
                    .is_some_and(|name| name.ends_with(&format!(".{}:v1", step.journal_name())))
        })
        .count();
    assert_eq!(step_runs, 1, "one journal command owns the faulted step");
    if let Some(refusal) = fault.refusal() {
        let refused_completions: Vec<_> = journal
            .iter()
            .filter_map(|entry| entry.run_completion())
            .filter_map(Result::ok)
            .filter_map(|value| {
                serde_json::from_slice::<Result<serde_json::Value, RuntimeEffectControllerError>>(
                    &value,
                )
                .ok()
            })
            .filter_map(Result::err)
            .collect();
        assert_eq!(
            refused_completions.len(),
            1,
            "the refusal is a completed inner result"
        );
        assert_refusal(&refused_completions[0], &refusal);
    }
}

const SEEDS: std::ops::Range<u64> = 0x4204_0000..0x4204_0014;

async fn sqlite_law(step: Step, fault: Fault) {
    for seed in SEEDS {
        let backend = lash_restate_test::backend(seed, server_config())
            .await
            .expect("the Restate SDK over SQLite");
        start_store_fault_law(&backend, step, fault).await;
        eprintln!("start-store-fault SQLite/Restate {step:?} {fault:?} seed={seed:x} PASS");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_fenced_at_start_claim_is_recorded_and_replayed() {
    sqlite_law(Step::Claim, Fault::WriterFenced).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_at_start_claim_is_recorded_and_replayed() {
    sqlite_law(Step::Claim, Fault::Incompatible).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writer_fenced_at_start_settlement_is_recorded_and_replayed() {
    sqlite_law(Step::Settle, Fault::WriterFenced).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incompatible_at_start_settlement_is_recorded_and_replayed() {
    sqlite_law(Step::Settle, Fault::Incompatible).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_start_claim_fault_retries_without_recording_the_fault() {
    sqlite_law(Step::Claim, Fault::Transient).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_start_settlement_fault_retries_without_recording_the_fault() {
    sqlite_law(Step::Settle, Fault::Transient).await;
}
