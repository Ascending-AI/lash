//! The `ProcessTerminal` obligation's detection bounds (ADR 0109 §1.8), on
//! SQLite and, when a database is configured, PostgreSQL.
//!
//! The terminal transaction arms the process row's obligation; the execution
//! that stored the terminal publishes it itself (the Restate double's laws own
//! that immediate attempt). These cases take the relay's side of every way the
//! immediate attempt can be lost, driving the real registry, ledger and
//! [`ProcessTerminalRelay`] under a virtual clock and a reconcile tick of
//! `T` = 10 s ±10%, and assert each bound the ADR states:
//!
//! - a lost immediate attempt is claimed by `due_at + T`;
//! - a lapsed claim is retaken by `claimed_at + claim_ttl + T`;
//! - retryable failure `n` is followed by attempt `n + 1` at
//!   `min(2^(n−1) s, 15 min)` plus at most `T`, and the row stalls after the
//!   attempt ceiling, never later and never retried again;
//! - a refused row stalls in the pass that claims it, and the rows behind it
//!   in the same page are still delivered.
//!
//! The SQLite leader failover term (`+ 20.5 s`) is the recovery lease's bound,
//! not this ledger's: the relay pass here is the one a due-claiming
//! deployment runs.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lash_core::runtime::ClockWallTime as _;
use lash_core::runtime::drive::relay::{RelayPolicy, relay_due};
use lash_core::runtime::process_terminal::ProcessTerminalRelay;
use lash_core::store::{ObligationKind, ObligationLedger, ObligationState, StallReason};
use lash_core::testing::TestClock;
use lash_core::{
    PluginError, ProcessAwaitOutput, ProcessId, ProcessRegistry, ProcessWorkSubstrate, StoreSet,
};
use lash_sansio::sync::MutexExt;

const EPOCH_MS: u64 = 1_700_000_000_000;
/// The reconcile tick: 10 s ±10%.
const TICK_MS: u64 = 10_000;
const TICK_JITTER_MS: u64 = 1_000;
const MAX_TICK_MS: u64 = TICK_MS + TICK_JITTER_MS;

/// How the scripted engine answers one process's publications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Publish,
    /// Unreachable engine: retryable.
    Unreachable,
    /// Refused for good: no retry can fix it.
    Refuse,
}

/// The engine port, scripted per process, recording when each publication
/// was attempted.
struct ScriptedEngine {
    clock: Arc<TestClock>,
    answers: Mutex<BTreeMap<ProcessId, Answer>>,
    attempts: Mutex<BTreeMap<ProcessId, Vec<u64>>>,
}

impl ScriptedEngine {
    fn new(clock: Arc<TestClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            answers: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(BTreeMap::new()),
        })
    }

    fn answer(&self, process_id: &ProcessId, answer: Answer) {
        self.answers
            .lock_recover()
            .insert(process_id.clone(), answer);
    }

    fn attempts(&self, process_id: &ProcessId) -> Vec<u64> {
        self.attempts
            .lock_recover()
            .get(process_id)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl ProcessWorkSubstrate for ScriptedEngine {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> Result<lash_core::facade_support::ProcessAdmissionReport, PluginError> {
        Ok(lash_core::facade_support::ProcessAdmissionReport::default())
    }

    async fn await_process_terminal(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessTerminalWait, PluginError> {
        Err(PluginError::Session(format!(
            "the scripted engine does not await `{process_id}`"
        )))
    }

    async fn deliver_cancel(
        &self,
        _process_id: &ProcessId,
        _request: &lash_core::CancelRequest,
        _key: &str,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    async fn publish_process_terminal(
        &self,
        process_id: &ProcessId,
        _output: &ProcessAwaitOutput,
        _key: &str,
    ) -> Result<(), PluginError> {
        self.attempts
            .lock_recover()
            .entry(process_id.clone())
            .or_default()
            .push(self.clock.timestamp_ms());
        let answer = self
            .answers
            .lock_recover()
            .get(process_id)
            .copied()
            .unwrap_or(Answer::Publish);
        match answer {
            Answer::Publish => Ok(()),
            Answer::Unreachable => Err(PluginError::Session(
                "the engine's ingress is unreachable".to_owned(),
            )),
            Answer::Refuse => Err(PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineServiceUnregistered,
                "no deployment binds the process workflow",
            ))),
        }
    }
}

/// One backend's store set on the virtual clock, and the relay over it.
struct Lane {
    name: &'static str,
    clock: Arc<TestClock>,
    registry: Arc<dyn ProcessRegistry>,
    ledger: Arc<dyn ObligationLedger>,
    engine: Arc<ScriptedEngine>,
    relay: ProcessTerminalRelay,
    rng: fastrand::Rng,
    _hold: Box<dyn std::any::Any + Send>,
}

impl Lane {
    fn over(
        name: &'static str,
        stores: Arc<dyn StoreSet>,
        clock: Arc<TestClock>,
        seed: u64,
        hold: Box<dyn std::any::Any + Send>,
    ) -> Self {
        let registry = stores.process_registry();
        let ledger = stores.obligation_ledger(ObligationKind::ProcessTerminal);
        let engine = ScriptedEngine::new(Arc::clone(&clock));
        let port: Arc<dyn ProcessWorkSubstrate> = engine.clone();
        let relay = ProcessTerminalRelay::new(Arc::clone(&ledger), Arc::clone(&registry), port);
        Self {
            name,
            clock,
            registry,
            ledger,
            engine,
            relay,
            rng: fastrand::Rng::with_seed(seed),
            _hold: hold,
        }
    }

    async fn lanes(seed: u64) -> Vec<Self> {
        let clock = Arc::new(TestClock::new(EPOCH_MS));
        let sqlite = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("open the SQLite store set");
        let mut lanes = vec![Self::over(
            "sqlite",
            Arc::new(sqlite),
            clock,
            seed,
            Box::new(()),
        )];
        if let Some(database) = crate::postgres_test_isolation::isolated_database().await {
            let clock = Arc::new(TestClock::new(EPOCH_MS));
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("connect the isolated PostgreSQL database");
            let attachments = tempfile::tempdir().expect("attachment root");
            let stores = lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash::persistence::FileAttachmentStore::new(
                    attachments.path().join("attachments"),
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock.clone(),
            );
            lanes.push(Self::over(
                "postgres",
                Arc::new(stores),
                clock,
                seed,
                Box::new((database, attachments)),
            ));
        }
        lanes
    }

    fn now(&self) -> u64 {
        self.clock.timestamp_ms()
    }

    /// A process whose terminal is stored at `now` and published by no
    /// execution: its obligation is due at `now`.
    async fn terminal(&self) -> ProcessId {
        let process_id = self
            .registry
            .register_process(lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ))
            .await
            .expect("register the process")
            .id;
        self.registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                    serde_json::json!({ "ended": true }),
                )),
                lash_core::ProcessCompletionAuthority::WorkflowKey {
                    workflow_key: process_id.to_string(),
                },
            )
            .await
            .expect("store the terminal");
        process_id
    }

    async fn state(&self, process_id: &ProcessId) -> Option<ObligationState> {
        self.registry
            .terminal_publication(process_id)
            .await
            .expect("read the publication")
            .map(|publication| publication.state)
    }

    /// Advance to the next reconcile tick and run its due pass.
    async fn tick(&mut self) -> lash_core::engine::RelayPass {
        let jitter = self.rng.u64(0..=2 * TICK_JITTER_MS);
        self.clock.advance(TICK_MS - TICK_JITTER_MS + jitter);
        relay_due(
            &self.relay,
            self.clock.as_ref(),
            NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("the due pass")
    }

    /// Tick until `process_id`'s publication leaves `due`/`claimed`, at most
    /// `ticks` times.
    async fn tick_until_settled(&mut self, process_id: &ProcessId, ticks: usize) {
        for _ in 0..ticks {
            self.tick().await;
            if matches!(
                self.state(process_id).await,
                Some(ObligationState::Delivered | ObligationState::Stalled)
            ) {
                return;
            }
        }
        panic!(
            "{}: `{process_id}`'s publication never settled in {ticks} ticks",
            self.name
        );
    }
}

/// A lost immediate attempt is claimed and delivered by `due_at + T`.
#[tokio::test]
async fn a_lost_immediate_publication_is_delivered_within_one_tick() {
    for mut lane in Lane::lanes(3856).await {
        let due_at = lane.now();
        let process_id = lane.terminal().await;
        assert_eq!(lane.state(&process_id).await, Some(ObligationState::Due));
        let pass = lane.tick().await;
        assert_eq!(pass.delivered, 1, "{}: {pass:?}", lane.name);
        let attempts = lane.engine.attempts(&process_id);
        assert_eq!(attempts.len(), 1, "{}", lane.name);
        assert!(
            attempts[0] <= due_at + MAX_TICK_MS,
            "{}: claimed at +{} ms, past due_at + T",
            lane.name,
            attempts[0] - due_at
        );
        assert_eq!(
            lane.state(&process_id).await,
            Some(ObligationState::Delivered)
        );
    }
}

/// A claim whose relay died is retaken by `claimed_at + claim_ttl + T`.
#[tokio::test]
async fn a_lapsed_claim_is_retaken_within_its_ttl_and_one_tick() {
    let policy = RelayPolicy::default();
    for mut lane in Lane::lanes(3857).await {
        let process_id = lane.terminal().await;
        // A relay claims the row and dies before it delivers or settles.
        let claimed_at = lane.now();
        let claimed = lane
            .ledger
            .claim_due(
                claimed_at,
                policy.claim_ttl_ms,
                NonZeroUsize::new(64).expect("non-zero"),
            )
            .await
            .expect("claim the due row");
        assert_eq!(claimed.len(), 1, "{}", lane.name);
        lane.tick_until_settled(&process_id, 16).await;
        let attempts = lane.engine.attempts(&process_id);
        assert_eq!(attempts.len(), 1, "{}", lane.name);
        assert!(
            attempts[0] >= claimed_at + policy.claim_ttl_ms,
            "{}: a live claim is never retaken",
            lane.name
        );
        assert!(
            attempts[0] <= claimed_at + policy.claim_ttl_ms + MAX_TICK_MS,
            "{}: retaken at +{} ms, past claimed_at + claim_ttl + T",
            lane.name,
            attempts[0] - claimed_at
        );
        assert_eq!(
            lane.state(&process_id).await,
            Some(ObligationState::Delivered)
        );
    }
}

/// Retryable failures back off `min(2^(n−1) s, 15 min)` plus at most `T`,
/// and the row stalls at the attempt ceiling — never later, never retried
/// again.
#[tokio::test]
async fn retryable_failures_back_off_and_stall_at_the_ceiling() {
    let policy = RelayPolicy::default();
    let ceiling = policy.attempt_ceiling.get();
    for mut lane in Lane::lanes(3858).await {
        let process_id = lane.terminal().await;
        lane.engine.answer(&process_id, Answer::Unreachable);
        let armed_at = lane.now();
        // 16 attempts at the default policy span ≈ 1 h 47 min.
        lane.tick_until_settled(&process_id, 2_000).await;
        let attempts = lane.engine.attempts(&process_id);
        assert_eq!(
            attempts.len(),
            usize::try_from(ceiling).expect("small"),
            "{}: stalled after exactly the ceiling",
            lane.name
        );
        assert!(attempts[0] <= armed_at + MAX_TICK_MS, "{}", lane.name);
        for (index, pair) in attempts.windows(2).enumerate() {
            let attempt = u32::try_from(index + 1).expect("small");
            let backoff = policy.backoff_ms(attempt);
            let gap = pair[1] - pair[0];
            assert!(
                gap >= backoff && gap <= backoff + MAX_TICK_MS,
                "{}: attempt {} came {gap} ms after attempt {attempt}; the backoff is {backoff} ms",
                lane.name,
                attempt + 1,
            );
        }
        assert_eq!(
            lane.state(&process_id).await,
            Some(ObligationState::Stalled)
        );
        let stalled = lane
            .ledger
            .list_stalled(None, NonZeroUsize::new(8).expect("non-zero"))
            .await
            .expect("list stalled");
        assert_eq!(stalled.len(), 1, "{}", lane.name);
        assert_eq!(stalled[0].reason, StallReason::AttemptsExhausted);
        assert_eq!(stalled[0].attempts, ceiling);
        // A stalled row is never attempted again until re-armed.
        for _ in 0..8 {
            lane.clock.advance(policy.max_backoff_ms);
            lane.tick().await;
        }
        assert_eq!(
            lane.engine.attempts(&process_id).len(),
            usize::try_from(ceiling).expect("small"),
            "{}",
            lane.name
        );
        // Re-armed, it is delivered on the next tick.
        lane.engine.answer(&process_id, Answer::Publish);
        assert!(
            lane.ledger
                .rearm(&stalled[0].id, lane.now())
                .await
                .expect("re-arm")
        );
        lane.tick().await;
        assert_eq!(
            lane.state(&process_id).await,
            Some(ObligationState::Delivered)
        );
    }
}

/// A refused row stalls in the pass that claims it, and the rows behind it in
/// the same page are still delivered.
#[tokio::test]
async fn a_refused_publication_stalls_in_its_pass_without_blocking_the_page() {
    for mut lane in Lane::lanes(3859).await {
        let refused = lane.terminal().await;
        lane.engine.answer(&refused, Answer::Refuse);
        let mut behind = Vec::new();
        for _ in 0..4 {
            lane.clock.advance(1);
            behind.push(lane.terminal().await);
        }
        let pass = lane.tick().await;
        assert_eq!(
            (pass.claimed, pass.delivered, pass.stalled),
            (5, 4, 1),
            "{}: {pass:?}",
            lane.name
        );
        assert_eq!(lane.state(&refused).await, Some(ObligationState::Stalled));
        let stalled = lane
            .ledger
            .list_stalled(None, NonZeroUsize::new(8).expect("non-zero"))
            .await
            .expect("list stalled");
        assert_eq!(stalled.len(), 1, "{}", lane.name);
        assert_eq!(stalled[0].reason, StallReason::Refused);
        assert_eq!(stalled[0].attempts, 1, "stalled on its first attempt");
        for process_id in &behind {
            assert_eq!(
                lane.state(process_id).await,
                Some(ObligationState::Delivered),
                "{}",
                lane.name
            );
        }
        assert_eq!(lane.engine.attempts(&refused).len(), 1);
    }
}
