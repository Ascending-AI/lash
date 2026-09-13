//! Slow-but-alive substrate faults.
//!
//! The classic lease-starvation shape: a worker holds a live session execution
//! lease, starts a store operation, and the operation takes longer than the
//! lease TTL while the worker itself stays alive and still writing. Nothing
//! crashes and no connection drops; only time passes.
//!
//! The delay is injected on the simulator's virtual clock
//! ([`crate::clock::SimClock`]), which is the same clock the SQLite store reads
//! for lease expiry, so the fault is deterministic and costs no wall time. The
//! injector is a [`RuntimePersistenceDecorator`], so it wraps the real store
//! rather than replacing any of its semantics.
//!
//! Two oracles judge the fault, both against production behavior:
//!
//! * the lease-loss refusal fires — the delayed commit is refused with
//!   [`StoreError::SessionExecutionLeaseExpired`], not silently accepted; and
//! * no partial write survives — reopening the store shows exactly the durable
//!   prefix that preceded the delayed commit, and the refused operation is
//!   still committable afterwards under fresh authority (FIG-2841's residue
//!   law).
//!
//! Postgres is deliberately not driven here: its lease clock is the database's
//! own `transaction_timestamp()`, not a client clock, so a virtual client clock
//! cannot advance it. That is the same property ADR 0009 records for clock
//! skew, and `crates/lash-postgres-store/tests/postgres_clock_contract.rs`
//! covers it directly.

use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

use lash_core::store::{RuntimeCommitReceipt, RuntimePersistenceDecorator};
use lash_core::{
    LeaseOwnerIdentity, OperationId, RuntimeCommit, RuntimePersistence, RuntimeSessionState,
    SessionExecutionLease, SessionExecutionLeaseClaimOutcome, SessionPolicy, SessionRelation,
    SessionStoreCreateRequest, SessionStoreFactory, StoreError,
};
use lash_sansio::SessionId;
use serde::Serialize;
use serde_json::{Value, json};

use crate::backend_contention::{LEASE_SEMANTIC_TTL_MS, LEASE_TTL_MS};
use crate::clock::SimClock;

/// Store operation a slow-but-alive arm can delay.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlowAliveOperation {
    /// `SessionCommitStore::commit_runtime_state`, the operation that publishes
    /// a turn and is fenced by the caller's session execution lease.
    CommitRuntimeState,
}

/// One deterministic slow-but-alive arm.
///
/// `occurrence` is one-based and counts only operations of `operation` reached
/// after the arm is installed. `delay_ms` advances the virtual clock before the
/// real store operation runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SlowAliveArm {
    pub operation: SlowAliveOperation,
    pub delay_ms: u64,
    pub occurrence: u64,
}

impl SlowAliveArm {
    pub const fn new(operation: SlowAliveOperation, delay_ms: u64, occurrence: u64) -> Self {
        Self {
            operation,
            delay_ms,
            occurrence,
        }
    }
}

/// Evidence that an armed delay ran before a real store operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SlowAliveObservation {
    pub operation: SlowAliveOperation,
    pub occurrence: u64,
    pub delay_ms: u64,
    pub clock_ms_before: u64,
    pub clock_ms_after: u64,
}

#[derive(Debug, Default)]
struct SlowAliveState {
    armed: Vec<SlowAliveArm>,
    occurrences: u64,
    observations: Vec<SlowAliveObservation>,
}

/// Store-operation delay injector on the simulator's virtual clock.
#[derive(Debug)]
pub struct StoreOperationDelayInjector {
    clock: Arc<SimClock>,
    state: Mutex<SlowAliveState>,
}

impl StoreOperationDelayInjector {
    pub(crate) fn new(clock: Arc<SimClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            state: Mutex::new(SlowAliveState::default()),
        })
    }

    /// Replace the current plan with these one-shot arms.
    pub fn arm_many(&self, arms: impl IntoIterator<Item = SlowAliveArm>) {
        let mut state = self.state.lock_recover();
        state.armed = arms.into_iter().collect();
        state.occurrences = 0;
    }

    pub fn observations(&self) -> Vec<SlowAliveObservation> {
        self.state.lock_recover().observations.clone()
    }

    pub fn remaining_arms(&self) -> Vec<SlowAliveArm> {
        self.state.lock_recover().armed.clone()
    }

    /// Consume the arm matching this operation occurrence and advance the
    /// virtual clock by its delay.
    async fn delay(&self, operation: SlowAliveOperation) {
        let (armed, occurrence, clock_ms_before) = {
            let mut state = self.state.lock_recover();
            state.occurrences += 1;
            let occurrence = state.occurrences;
            let position = state
                .armed
                .iter()
                .position(|arm| arm.operation == operation && arm.occurrence == occurrence);
            match position {
                Some(position) => {
                    let arm = state.armed.remove(position);
                    (Some(arm), occurrence, self.clock.logical_ms())
                }
                None => (None, occurrence, self.clock.logical_ms()),
            }
        };
        let Some(arm) = armed else {
            return;
        };
        // The worker is alive throughout: only the virtual clock moves.
        self.clock.advance_by(arm.delay_ms).await;
        let clock_ms_after = self.clock.logical_ms();
        self.state
            .lock_recover()
            .observations
            .push(SlowAliveObservation {
                operation,
                occurrence,
                delay_ms: arm.delay_ms,
                clock_ms_before,
                clock_ms_after,
            });
    }
}

/// The real store, wrapped so armed operations run slowly on the virtual clock.
struct SlowAliveStore {
    inner: Arc<dyn RuntimePersistence>,
    injector: Arc<StoreOperationDelayInjector>,
}

#[async_trait::async_trait]
impl RuntimePersistenceDecorator for SlowAliveStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        self.injector
            .delay(SlowAliveOperation::CommitRuntimeState)
            .await;
        self.inner.commit_runtime_state(commit).await
    }
}

/// One judged slow-but-alive scenario.
#[derive(Clone, Debug, Serialize)]
pub struct SlowAliveScenario {
    pub schema: &'static str,
    pub backend: &'static str,
    pub status: &'static str,
    pub session_id: SessionId,
    pub lease_ttl_ms: u64,
    pub delay_ms: u64,
    pub durable_prefix_revision: u64,
    pub observations: Vec<SlowAliveObservation>,
    pub oracles: Vec<SlowAliveOracle>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SlowAliveOracle {
    pub oracle_id: &'static str,
    pub status: &'static str,
    pub assertion: &'static str,
    pub evidence: Value,
}

/// Runs the slow-but-alive scenario against the real SQLite substrate on the
/// simulator's virtual clock.
///
/// `delay_ms` is the store-operation delay injected between the lease claim and
/// the commit. A delay under the lease TTL is the control: the same commit must
/// succeed.
pub async fn run_slow_alive_scenario(
    root: &std::path::Path,
    delay_ms: u64,
) -> Result<SlowAliveScenario, String> {
    let clock = SimClock::new();
    let factory: Arc<dyn SessionStoreFactory> = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new(root.join("sqlite-store"))
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    );
    let session_id = SessionId::from(format!("lash-sim-slow-alive-{delay_ms:016x}"));
    let store = factory
        .create_store(&request(&session_id))
        .await
        .map_err(|error| format!("create slow-alive store: {error}"))?;

    // Durable prefix: the work that must survive the refused commit.
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(SessionPolicy::new(lash_core::TurnBudget::Unbounded))
    };
    let prefix = stamped_commit(&state, "slow-alive-prefix")?;
    let prefix_result = store
        .commit_runtime_state(prefix)
        .await
        .map_err(|error| format!("slow-alive prefix commit: {error}"))?;
    state.apply_persisted_commit_result(prefix_result);
    let durable_prefix_revision = state.head_revision;

    // The worker claims the lane and stays alive for the whole scenario.
    let owner = LeaseOwnerIdentity::opaque("slow-alive-worker", "slow-alive-worker:001");
    let lease = claim(&store, &session_id, &owner, LEASE_TTL_MS).await?;

    let injector = StoreOperationDelayInjector::new(Arc::clone(&clock));
    injector.arm_many([SlowAliveArm::new(
        SlowAliveOperation::CommitRuntimeState,
        delay_ms,
        1,
    )]);
    let slow_store = SlowAliveStore {
        inner: Arc::clone(&store),
        injector: Arc::clone(&injector),
    };

    let mut target_state = state.clone();
    target_state.turn_index = 1;
    let target = stamped_commit(&target_state, "slow-alive-target")?;
    let outcome = lash_core::SessionCommitStore::commit_runtime_state(
        &slow_store,
        target
            .clone()
            .borrowing_session_execution_lease(lease.fence()),
    )
    .await;

    let observations = injector.observations();
    if observations.len() != 1 || observations[0].delay_ms != delay_ms {
        return Err(format!(
            "the armed store-operation delay did not run exactly once: {observations:?}"
        ));
    }
    if !injector.remaining_arms().is_empty() {
        return Err("the slow-but-alive arm was not consumed".to_string());
    }

    let mut oracles = Vec::new();
    let lease_lost = delay_ms >= LEASE_TTL_MS;
    match (&outcome, lease_lost) {
        (Err(StoreError::SessionExecutionLeaseExpired { .. }), true) => {
            oracles.push(SlowAliveOracle {
                oracle_id: "sim.oracle.slow-alive-lease-loss-refusal.v1",
                status: "passed",
                assertion: "a store operation that outlives the lease TTL while the worker stays alive is refused with SessionExecutionLeaseExpired",
                evidence: json!({
                    "lease_ttl_ms": LEASE_TTL_MS,
                    "delay_ms": delay_ms,
                    "clock_ms_before": observations[0].clock_ms_before,
                    "clock_ms_after": observations[0].clock_ms_after,
                    "store_error_variant": "SessionExecutionLeaseExpired",
                }),
            });
        }
        (Ok(_), false) => {
            oracles.push(SlowAliveOracle {
                oracle_id: "sim.oracle.slow-alive-control-commits.v1",
                status: "passed",
                assertion: "the same delayed commit inside the lease TTL is published, so the refusal above is caused by the elapsed lease and nothing else",
                evidence: json!({
                    "lease_ttl_ms": LEASE_TTL_MS,
                    "delay_ms": delay_ms,
                }),
            });
        }
        (Err(error), true) => {
            return Err(format!(
                "delayed commit past the lease TTL returned {error:?}, expected SessionExecutionLeaseExpired"
            ));
        }
        (Ok(result), true) => {
            return Err(format!(
                "delayed commit past the lease TTL published revision {}",
                result.head_revision
            ));
        }
        (Err(error), false) => {
            return Err(format!(
                "control commit inside the lease TTL failed: {error}"
            ));
        }
    }

    // Residue: reopen and read what the substrate actually holds.
    drop(slow_store);
    drop(store);
    let reopened = factory
        .open_existing_store(&request(&session_id))
        .await
        .map_err(|error| format!("reopen slow-alive store: {error}"))?
        .ok_or_else(|| "slow-alive session disappeared".to_string())?;
    let read = reopened
        .load_session()
        .await
        .map_err(|error| format!("load slow-alive session: {error}"))?
        .ok_or_else(|| "slow-alive session state disappeared".to_string())?;
    let expected_revision = if lease_lost {
        durable_prefix_revision
    } else {
        durable_prefix_revision + 1
    };
    if read.head_revision != expected_revision {
        return Err(format!(
            "reopened head revision {} but expected {expected_revision}",
            read.head_revision
        ));
    }
    if lease_lost {
        // The refused operation left no residue at all: under fresh authority
        // the very same commit still publishes, exactly once.
        let successor =
            LeaseOwnerIdentity::opaque("slow-alive-successor", "slow-alive-successor:001");
        // The predecessor's TTL has already elapsed on the virtual clock.
        let successor_lease =
            claim(&reopened, &session_id, &successor, LEASE_SEMANTIC_TTL_MS).await?;
        let retried = reopened
            .commit_runtime_state(target.borrowing_session_execution_lease(successor_lease.fence()))
            .await
            .map_err(|error| format!("retry the refused commit under fresh authority: {error}"))?;
        if retried.head_revision != durable_prefix_revision + 1 {
            return Err(format!(
                "retry published revision {} but the refused commit must advance the prefix exactly once",
                retried.head_revision
            ));
        }
        oracles.push(SlowAliveOracle {
            oracle_id: "sim.oracle.slow-alive-no-partial-write.v1",
            status: "passed",
            assertion: "the refused delayed commit leaves the durable prefix untouched and no partial residue: the same operation still publishes exactly once under fresh authority",
            evidence: json!({
                "durable_prefix_revision": durable_prefix_revision,
                "reopened_head_revision": read.head_revision,
                "retried_head_revision": retried.head_revision,
                "refused_commit_published": false,
            }),
        });
    }

    Ok(SlowAliveScenario {
        schema: "lash.sim.slow-alive-fault.v1",
        backend: "sqlite",
        status: "passed",
        session_id,
        lease_ttl_ms: LEASE_TTL_MS,
        delay_ms,
        durable_prefix_revision,
        observations,
        oracles,
    })
}

fn request(session_id: &SessionId) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    }
}

fn stamped_commit(
    state: &RuntimeSessionState,
    operation_suffix: &str,
) -> Result<RuntimeCommit, String> {
    RuntimeCommit::persisted_state_for_test(state, &[])
        .with_operation(OperationId::turn(
            &state.session_id,
            format!("slow-alive-{operation_suffix}"),
            "final",
        ))
        .map(|(commit, _)| commit)
        .map_err(|error| format!("stamp slow-alive commit: {error}"))
}

async fn claim(
    store: &Arc<dyn RuntimePersistence>,
    session_id: &SessionId,
    owner: &LeaseOwnerIdentity,
    lease_ttl_ms: u64,
) -> Result<SessionExecutionLease, String> {
    match store
        .try_claim_session_execution_lease(session_id, owner, "slow-alive-executor", lease_ttl_ms)
        .await
        .map_err(|error| format!("claim slow-alive lease: {error}"))?
    {
        SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => Ok(acquisition.lease),
        SessionExecutionLeaseClaimOutcome::Busy { holder } => {
            Err(format!("slow-alive lease unexpectedly busy: {holder:?}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fault: the commit outlives the lease TTL while the worker is alive.
    /// Both oracles must hold, and the control below must behave differently.
    #[tokio::test]
    async fn slow_but_alive_commit_loses_the_lease_and_leaves_no_partial_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let armed = run_slow_alive_scenario(tmp.path(), LEASE_TTL_MS + 1)
            .await
            .expect("slow-but-alive scenario");
        assert_eq!(armed.status, "passed");
        assert_eq!(armed.observations.len(), 1);
        assert_eq!(
            armed.observations[0].clock_ms_after - armed.observations[0].clock_ms_before,
            LEASE_TTL_MS + 1
        );
        let oracle_ids = armed
            .oracles
            .iter()
            .map(|oracle| oracle.oracle_id)
            .collect::<Vec<_>>();
        assert_eq!(
            oracle_ids,
            vec![
                "sim.oracle.slow-alive-lease-loss-refusal.v1",
                "sim.oracle.slow-alive-no-partial-write.v1",
            ]
        );
        assert!(armed.oracles.iter().all(|oracle| oracle.status == "passed"));
    }

    /// The control: the identical delay injector, below the TTL, publishes.
    /// Removing the delay must change the outcome, or the oracle above would
    /// pass for a reason other than the fault.
    #[tokio::test]
    async fn a_delay_inside_the_lease_ttl_still_publishes_the_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let control = run_slow_alive_scenario(tmp.path(), LEASE_TTL_MS - 1)
            .await
            .expect("slow-but-alive control");
        assert_eq!(
            control
                .oracles
                .iter()
                .map(|oracle| oracle.oracle_id)
                .collect::<Vec<_>>(),
            vec!["sim.oracle.slow-alive-control-commits.v1"],
        );
    }

    #[test]
    fn every_slow_alive_oracle_id_is_declared_in_the_inventory() {
        for id in [
            "sim.oracle.slow-alive-lease-loss-refusal.v1",
            "sim.oracle.slow-alive-no-partial-write.v1",
            "sim.oracle.slow-alive-control-commits.v1",
        ] {
            assert!(
                crate::trace::oracle_observation_class(id).is_some(),
                "`{id}` is missing from the declared oracle inventory"
            );
        }
    }
}
