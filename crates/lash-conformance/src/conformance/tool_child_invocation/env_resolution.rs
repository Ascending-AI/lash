//! The live-fault laws: a group child's failure is sealed as its outcome only
//! when it is one, and a live fault is released unrecorded and re-driven
//! (FIG-3643, FIG-3644).
//!
//! A child resolves the environment its request records before it runs any
//! attempt (ADR 0099 §3). Two very different things can stop that read. The
//! recorded environment may be absent or unreadable by this build — a fact
//! about durable state that every redrive meets again, so the refusal is the
//! child's outcome and it replays as one. Or the store may fail to answer at
//! all: a pool that times out, a dropped connection, a busy database. That is
//! a fact about this attempt. Sealing it would replay the fault as the child's
//! terminal on every redrive, so the turn could only abort again; instead the
//! child's claim is released unrecorded, the waiting caller is handed the
//! fault, and the child re-runs under the same replay key once the store
//! answers.
//!
//! The laws speak about what a journal records, so a host that journals
//! nothing — a locally participating one — has nothing to seal or release and
//! answers none of them. A tier with a Lash-owned journal (`drain` is `Some`)
//! hands the fault to the caller and is re-driven by the caller's reopen;
//! Restate's engine retries the child invocation itself, so there the caller
//! sees no settlement while the fault lasts and the child's own retry settles
//! it once the fault clears.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use pretty_assertions::assert_eq;

use super::*;

/// How the faulting store answers a read of a process-execution environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum EnvRead {
    /// The inner store answers.
    Healthy,
    /// An opaque session-seam error: what the SQL stores report for a
    /// pool-acquire timeout or a lost connection.
    StoreFault,
    /// A carried park: a replay this build cannot serve, typed as the
    /// divergence a re-executed program reports (FIG-3586).
    Divergent,
    /// Nothing is stored under the reference.
    Missing,
}

/// A process-execution-env store whose reads answer as [`EnvRead`] says.
struct FaultingEnvStore {
    inner: Arc<dyn crate::ProcessExecutionEnvStore>,
    read: AtomicU8,
    reads: AtomicUsize,
}

impl FaultingEnvStore {
    fn new(inner: Arc<dyn crate::ProcessExecutionEnvStore>, read: EnvRead) -> Arc<Self> {
        Arc::new(Self {
            inner,
            read: AtomicU8::new(read as u8),
            reads: AtomicUsize::new(0),
        })
    }

    fn answer(&self, read: EnvRead) {
        self.read.store(read as u8, Ordering::Release);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl crate::ProcessExecutionEnvStore for FaultingEnvStore {
    async fn publish_process_execution_env(
        &self,
        owner: &crate::ArtifactOwner,
        env_ref: &crate::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> Result<(), crate::PluginError> {
        self.inner
            .publish_process_execution_env(owner, env_ref, bytes)
            .await
    }

    async fn transfer_process_execution_env(
        &self,
        from: &crate::ArtifactOwner,
        to: &crate::ArtifactOwner,
        env_ref: &crate::ProcessExecutionEnvRef,
    ) -> Result<(), crate::PluginError> {
        self.inner
            .transfer_process_execution_env(from, to, env_ref)
            .await
    }

    async fn release_process_execution_env(
        &self,
        owner: &crate::ArtifactOwner,
        env_ref: &crate::ProcessExecutionEnvRef,
    ) -> Result<(), crate::PluginError> {
        self.inner
            .release_process_execution_env(owner, env_ref)
            .await
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &crate::ArtifactOwner,
    ) -> Result<(), crate::PluginError> {
        self.inner.retire_process_execution_env_owner(owner).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &crate::ProcessExecutionEnvRef,
    ) -> Result<Option<Vec<u8>>, crate::PluginError> {
        self.reads.fetch_add(1, Ordering::AcqRel);
        match self.read.load(Ordering::Acquire) {
            read if read == EnvRead::StoreFault as u8 => Err(crate::PluginError::Session(
                "pool timed out while waiting for an open connection".to_string(),
            )),
            read if read == EnvRead::Divergent as u8 => {
                Err(crate::PluginError::RuntimeEffectController(
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::LashlangCellReplayDivergence,
                        "the recorded environment was written by another build",
                    ),
                ))
            }
            read if read == EnvRead::Missing as u8 => Ok(None),
            _ => self.inner.get_process_execution_env(env_ref).await,
        }
    }
}

/// One law's stage: a journaling world, its scenario and a registered opener
/// whose children resolve their environment through `env_store`.
struct Stage {
    world: ToolChildWorld,
    scenario: Scenario,
    scope: crate::ExecutionScope,
    session_id: crate::SessionId,
    env_store: Arc<FaultingEnvStore>,
    _opener: crate::runtime::effect::LiveOpenerGuard,
}

impl Stage {
    /// Builds the stage, or answers `None` for a host that journals nothing.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn new(fixture: &ToolChildLawFixture, name: &str, read: EnvRead) -> Option<Self> {
        let session_id = crate::SessionId::from(name.to_string());
        let scope = crate::ExecutionScope::turn(
            session_id.clone(),
            crate::TurnId::from(format!("{name}-turn")),
        );
        let world = (fixture.make_world)(ToolChildWorldSpec {
            lease_ttl_ms: LIVE_LEASE_MS,
        })
        .await;
        if world
            .host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds")
            .controller()
            .effect_journaling()
            != crate::runtime::effect::EffectJournaling::Journaled
        {
            return None;
        }
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let env_store = FaultingEnvStore::new(Arc::clone(&scenario.process_env_store), read);
        // Bound explicitly: a tier whose worlds share one host installed its
        // child host on an earlier registration.
        install_child_host(
            &world.host,
            &(Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>),
        )
        .with_process_env_store(Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>);
        let opener = register_opener(
            &world.host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&scenario.registry),
            Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>,
            crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
                .expect("a turn scope derives an opener"),
            tokio_util::sync::CancellationToken::new(),
        );
        Some(Self {
            world,
            scenario,
            scope,
            session_id,
            env_store,
            _opener: opener,
        })
    }

    /// Whether the tier keeps a Lash-owned journal: its caller is handed the
    /// fault and re-drives by reopening. Restate's engine retries instead.
    fn caller_redrives(&self) -> bool {
        self.world.drain.is_some()
    }

    fn scoped(&self) -> crate::ScopedEffectController<'_> {
        self.world
            .host
            .scoped(crate::admit(self.scope.clone()))
            .unwrap_or_else(|error| panic!("the group scope binds: {error}"))
    }

    async fn group(&self, group_key: &str, tool_id: &str) -> crate::RuntimeEffectGroup {
        single_leaf_group(
            &self.scope,
            &self.session_id,
            group_key,
            &self.scenario.env_ref,
            tool_id,
            ToolChildCompletionRouting::Inline,
            recorded_cancellation_authority(&self.world.host, &crate::admit(self.scope.clone()))
                .await,
        )
    }

    async fn open(&self, group_key: &str, tool_id: &str) -> crate::EffectGroupHandle {
        self.scoped()
            .controller()
            .open_effect_group(self.group(group_key, tool_id).await)
            .await
            .unwrap_or_else(|error| panic!("the group `{group_key}` opens: {error}"))
    }

    /// The next settlement, or the error the await answered, inside the
    /// settle budget.
    async fn next(
        &self,
        handle: &mut crate::EffectGroupHandle,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        tokio::time::timeout(
            SETTLE_BUDGET,
            self.scoped()
                .controller()
                .await_next_settlement(handle, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap_or_else(|_| panic!("the child reported nothing inside the settle budget"))
    }

    /// Asserts that no settlement arrives while the fault lasts: nothing is
    /// sealed for a child whose run aborted.
    async fn assert_nothing_settles(&self, handle: &mut crate::EffectGroupHandle, what: &str) {
        let served = tokio::time::timeout(
            ABSENCE_BUDGET,
            self.scoped()
                .controller()
                .await_next_settlement(handle, tokio_util::sync::CancellationToken::new()),
        )
        .await;
        assert!(
            served.is_err(),
            "{what}: nothing may be recorded for a child whose run aborted, got {served:?}"
        );
    }

    /// Asserts the engine ran the child again since `reads` were counted:
    /// the aborted run failed retryably rather than settling. Asked once per
    /// fault rather than per absence window, because the engine's retry
    /// backoff grows and a later window may see no attempt.
    fn assert_retried_since(&self, reads: usize, what: &str) {
        assert!(
            self.env_store.reads() >= reads + 2,
            "{what}: the engine retries the child while the fault lasts ({} reads since {reads})",
            self.env_store.reads()
        );
    }

    /// The caller's half of a live abort on a Lash-owned journal: the await
    /// answers the unrecorded fault itself — never a settlement — with
    /// `cause`, and the leaf body never ran.
    async fn expect_unrecorded_abort(
        &self,
        handle: &mut crate::EffectGroupHandle,
        cause: crate::TurnFailureCause,
        what: &str,
    ) -> crate::RuntimeEffectControllerError {
        let error = match self.next(handle).await {
            Err(error) => error,
            Ok(settlement) => panic!(
                "{what}: an aborted child is released unrecorded, never settled: {settlement:?}"
            ),
        };
        assert_eq!(
            error.turn_failure_cause(),
            cause,
            "{what}: the caller is handed the abort as it was raised: {error}"
        );
        assert!(
            !error.journaled,
            "{what}: the abort is not a recorded outcome"
        );
        error
    }

    fn assert_settled_plain(&self, settlement: &crate::GroupSettlement, what: &str) {
        let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
        else {
            panic!("{what}: the re-driven child settles its tool invocation: {settlement:?}")
        };
        assert!(
            format!("{:?}", outcome.record.output).contains("plain"),
            "{what}: the child settles with the plain leaf's output"
        );
    }
}

/// FIG-3643/FIG-3644, the core law: a child's recorded failure replays as its
/// outcome on every redrive, while a store fault during its environment
/// resolution is never recorded — the caller aborts on it, and the child
/// re-runs and settles once the store answers.
///
/// The sealed half: a child whose recorded environment is missing is refused
/// with its request-version outcome, and every reopen serves that same
/// recorded failure without running anything, even after the store is
/// healthy — the record answers, not the store.
///
/// The live half: a pool timeout while the child reads its environment is a
/// live fault. On a Lash-owned journal the caller's await is handed the fault
/// itself — no settlement — and the reopen that re-drives the turn runs the
/// child under the same replay key to its settlement. On Restate nothing
/// settles while the fault lasts, the engine retrying the child, and the
/// child's own retry settles it once the store answers.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_process_env_store_fault_is_a_live_fault_not_the_childs_outcome(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let Some(stage) = Stage::new(fixture, &format!("{prefix}-env-fault"), EnvRead::Missing).await
    else {
        return;
    };

    // The sealed half.
    let sealed_key = format!("{prefix}-env-fault-sealed");
    let mut handle = stage.open(&sealed_key, LEAF_PLAIN).await;
    let settlement = stage
        .next(&mut handle)
        .await
        .expect("a refused child settles with its recorded refusal");
    let refused = settlement
        .outcome
        .clone()
        .expect_err("a child whose environment is missing is refused");
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildRequestVersion,
        "a missing environment is the request's, not the attempt's: {refused}"
    );
    assert_eq!(
        refused.turn_failure_cause(),
        crate::TurnFailureCause::Outcome,
        "the refusal is the child's outcome"
    );
    stage
        .scoped()
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the sealed group closes");
    stage.env_store.answer(EnvRead::Healthy);
    if stage.caller_redrives() {
        for redrive in ["first", "second"] {
            let mut handle = stage.open(&sealed_key, LEAF_PLAIN).await;
            let replayed = stage
                .next(&mut handle)
                .await
                .expect("the recorded refusal is served on reopen");
            let error = replayed
                .outcome
                .clone()
                .expect_err("the recorded refusal replays, whatever the store says now");
            assert_eq!(
                (error.code.clone(), error.message.clone()),
                (refused.code.clone(), refused.message.clone()),
                "the {redrive} redrive replays the identical recorded failure"
            );
            assert!(error.journaled, "the {redrive} redrive reads the record");
            stage
                .scoped()
                .controller()
                .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                .await
                .expect("the replayed group closes");
        }
    }
    assert!(
        stage
            .scenario
            .observation
            .executions_of("law_plain")
            .is_empty(),
        "a refused child runs no attempt, first run or replay"
    );

    // The live half.
    let live_key = format!("{prefix}-env-fault-live");
    stage.env_store.answer(EnvRead::StoreFault);
    let reads_before = stage.env_store.reads();
    let mut handle = stage.open(&live_key, LEAF_PLAIN).await;
    let settlement = if stage.caller_redrives() {
        stage
            .expect_unrecorded_abort(
                &mut handle,
                crate::TurnFailureCause::LiveFault,
                "a store fault during environment resolution",
            )
            .await;
        assert!(
            stage.env_store.reads() > reads_before,
            "the precondition: the child read its environment through the faulting store"
        );
        assert!(
            stage
                .scenario
                .observation
                .executions_of("law_plain")
                .is_empty(),
            "no attempt runs under an environment the child could not resolve"
        );
        // The store answers again, and the turn's redrive reopens the
        // identical group: the child was released, not sealed, so it runs.
        stage.env_store.answer(EnvRead::Healthy);
        let mut reopened = stage.open(&live_key, LEAF_PLAIN).await;
        let settlement = stage
            .next(&mut reopened)
            .await
            .expect("the re-driven child settles");
        handle = reopened;
        settlement
    } else {
        stage
            .assert_nothing_settles(&mut handle, "a store fault on Restate")
            .await;
        stage.assert_retried_since(reads_before, "a store fault on Restate");
        stage.env_store.answer(EnvRead::Healthy);
        stage
            .next(&mut handle)
            .await
            .expect("the engine's retry settles the child")
    };
    stage.assert_settled_plain(&settlement, "the live half");
    assert_eq!(
        stage.scenario.observation.executions_of("law_plain").len(),
        1,
        "the re-driven child ran its body exactly once"
    );
    stage
        .scoped()
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the settled group closes");
}

/// FIG-3644 composed with FIG-3586: a child whose replay this build cannot
/// serve parks on every redrive and is never sealed, and the same child,
/// once its refusal is a live fault instead, is re-driven — and settles on
/// the redrive that meets a store that answers.
///
/// A park is not a recorded outcome either: sealing one would make the build
/// that wrote the journal, redeployed, replay the refusal instead of serving
/// the child. So every redrive by the refusing build aborts with the park
/// itself, the journal holds nothing for the child, and nothing runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_divergent_child_parks_every_redrive_and_a_live_fault_re_drives(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let Some(stage) = Stage::new(fixture, &format!("{prefix}-env-park"), EnvRead::Divergent).await
    else {
        return;
    };
    let group_key = format!("{prefix}-env-park-group");
    let mut handle = stage.open(&group_key, LEAF_PLAIN).await;
    let settlement = if stage.caller_redrives() {
        for redrive in ["first run", "first redrive", "second redrive"] {
            if redrive != "first run" {
                handle = stage.open(&group_key, LEAF_PLAIN).await;
            }
            let parked = stage
                .expect_unrecorded_abort(&mut handle, crate::TurnFailureCause::Parked, redrive)
                .await;
            assert_eq!(
                parked.code,
                crate::RuntimeErrorCode::LashlangCellReplayDivergence,
                "the {redrive} parks on the divergence itself"
            );
        }
        stage.env_store.answer(EnvRead::StoreFault);
        handle = stage.open(&group_key, LEAF_PLAIN).await;
        stage
            .expect_unrecorded_abort(
                &mut handle,
                crate::TurnFailureCause::LiveFault,
                "the redrive that meets a store fault",
            )
            .await;
        stage.env_store.answer(EnvRead::Healthy);
        handle = stage.open(&group_key, LEAF_PLAIN).await;
        stage
            .next(&mut handle)
            .await
            .expect("the redrive that meets a healthy store settles")
    } else {
        stage
            .assert_nothing_settles(&mut handle, "a divergent child on Restate")
            .await;
        stage.assert_retried_since(0, "a divergent child on Restate");
        stage.env_store.answer(EnvRead::StoreFault);
        stage
            .assert_nothing_settles(&mut handle, "a store fault on Restate")
            .await;
        stage.env_store.answer(EnvRead::Healthy);
        stage
            .next(&mut handle)
            .await
            .expect("the engine's retry settles the child")
    };
    stage.assert_settled_plain(&settlement, "the re-driven child");
    assert_eq!(
        stage.scenario.observation.executions_of("law_plain").len(),
        1,
        "neither a park nor a live fault ran the body; the settling run ran it once"
    );
    stage
        .scoped()
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the settled group closes");
}

/// FIG-3644, ADR 0042 at-least-once: a child that aborts on a live journal
/// fault between two attempts is re-driven under its own replay key, so the
/// attempt it already recorded replays and only the unrecorded one runs.
///
/// The retry leaf's first attempt records a retryable failure; the journal's
/// claim of the second attempt then fails — a `ControllerAborted` child. The
/// child is released, not sealed with the store's error, and the caller is
/// handed that fault. The reopen re-runs the child: the first attempt replays
/// from its record without re-entering the tool, the second attempt runs
/// once, and the child settles under the call id it was admitted with. A
/// keyed tool therefore sees each recorded attempt exactly once.
///
/// Only a tier with a Lash-owned journal can arm the fault; Restate's journal
/// is the engine's, and its retry replays the same way by construction.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_re_driven_child_replays_its_recorded_attempts_under_its_replay_key(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let Some(stage) = Stage::new(
        fixture,
        &format!("{prefix}-attempt-fault"),
        EnvRead::Healthy,
    )
    .await
    else {
        return;
    };
    let Some(faults) = stage.world.journal_faults.clone() else {
        return;
    };
    let group_key = format!("{prefix}-attempt-fault-group");
    let call_id = format!("{group_key}-call-0");
    let second_attempt = crate::runtime::causal::child_effect_invocation(
        &stage.scope,
        &parent_invocation(&stage.scope),
        "law:attempt",
        format!("{call_id}:attempt:2"),
    );
    faults.fail_next(
        lash_core::facade_support::effect_replay_driver::EffectJournalFaultPoint::Claim,
        second_attempt.replay_key(),
    );

    let mut handle = stage.open(&group_key, LEAF_RETRY).await;
    let aborted = stage
        .expect_unrecorded_abort(
            &mut handle,
            crate::TurnFailureCause::LiveFault,
            "a journal fault on the second attempt's claim",
        )
        .await;
    assert!(
        faults.fired(),
        "the precondition: the second attempt's claim met the armed fault"
    );
    assert_eq!(
        aborted.code,
        faults.store_code(),
        "the caller is handed the journal's own store error"
    );
    let attempts = |observation: &LawObservation| {
        observation
            .executions_of("law_retry")
            .iter()
            .map(|execution| execution.attempt)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        attempts(&stage.scenario.observation),
        vec![1],
        "only the first attempt ran before the abort"
    );

    let mut reopened = stage.open(&group_key, LEAF_RETRY).await;
    let settlement = stage
        .next(&mut reopened)
        .await
        .expect("the re-driven child settles");
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
    else {
        panic!("the re-driven child settles its tool invocation: {settlement:?}")
    };
    assert!(
        format!("{:?}", outcome.record.output).contains("retry"),
        "the child settles with the retry leaf's second-attempt output"
    );
    assert_eq!(
        outcome.record.call_id.as_deref(),
        Some(call_id.as_str()),
        "the re-driven child answers under the call id it was admitted with"
    );
    assert_eq!(
        attempts(&stage.scenario.observation),
        vec![1, 2],
        "the recorded first attempt replayed; only the second ran, once"
    );
    stage
        .scoped()
        .controller()
        .close_effect_group(reopened, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the settled group closes");
}
