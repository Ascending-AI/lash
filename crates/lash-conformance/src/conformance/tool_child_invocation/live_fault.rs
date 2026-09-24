//! The live-fault laws: a group child's failure is its recorded outcome only
//! when it is one (FIG-3575).
//!
//! A child resolves the environment its request records before it runs any
//! attempt (ADR 0099 §3). Three things can stop that read, and each is a
//! different fact:
//!
//! - **Missing.** The recorded environment is absent, or unreadable by this
//!   build. That is a fact about durable state, which every redrive meets
//!   again, so the refusal is the child's outcome.
//! - **Store fault.** The store fails to answer: a pool that times out, or a
//!   dropped connection. That is a fact about this attempt. It is never the
//!   child's recorded outcome: nothing settles while it lasts, and the
//!   substrate runs the child again until it settles.
//! - **Park.** A replay this build cannot serve (FIG-3586) is refused the
//!   same way by every run of this build, so re-running it would spin. The
//!   park is the child's settlement, and it hands the park to the waiting
//!   turn.
//!
//! The laws are written against the effect interface. They are registered on
//! an engine that re-runs a child on a live fault, through
//! `tool_child_live_fault_tests!`.

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

/// One law's stage: a world, its scenario, and a registered opener whose
/// children resolve their environment through `env_store`.
struct Stage {
    world: ToolChildWorld,
    scenario: Scenario,
    scope: crate::ExecutionScope,
    session_id: crate::SessionId,
    env_store: Arc<FaultingEnvStore>,
    _opener: crate::runtime::effect::LiveOpenerGuard,
}

impl Stage {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn new(fixture: &ToolChildLawFixture, name: &str, read: EnvRead) -> Self {
        let session_id = crate::SessionId::from(name.to_string());
        let scope = crate::ExecutionScope::turn(
            session_id.clone(),
            crate::TurnId::from(format!("{name}-turn")),
        );
        let world = (fixture.make_world)(ToolChildWorldSpec {
            lease_ttl_ms: LIVE_LEASE_MS,
        })
        .await;
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let env_store = Arc::new(FaultingEnvStore {
            inner: Arc::clone(&scenario.process_env_store),
            read: AtomicU8::new(read as u8),
            reads: AtomicUsize::new(0),
        });
        let dyn_store = Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>;
        // Bound explicitly: a tier whose worlds share one host installed its
        // child host on an earlier registration.
        install_child_host(&world.host, &dyn_store).with_process_env_store(Arc::clone(&dyn_store));
        let opener = register_opener(
            &world.host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            Arc::clone(&scenario.registry),
            dyn_store,
            crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
                .expect("a turn scope derives an opener"),
            tokio_util::sync::CancellationToken::new(),
        );
        Self {
            world,
            scenario,
            scope,
            session_id,
            env_store,
            _opener: opener,
        }
    }

    fn scoped(&self) -> crate::ScopedEffectController<'_> {
        self.world
            .host
            .scoped(crate::admit(self.scope.clone()))
            .unwrap_or_else(|error| panic!("the group scope binds: {error}"))
    }

    async fn open(&self, group_key: &str) -> crate::EffectGroupHandle {
        let group = single_leaf_group(
            &self.scope,
            &self.session_id,
            group_key,
            &self.scenario.env_ref,
            LEAF_PLAIN,
            ToolChildCompletionRouting::Inline,
            recorded_cancellation_authority(&self.world.host, &crate::admit(self.scope.clone()))
                .await,
        );
        self.scoped()
            .controller()
            .open_effect_group(group)
            .await
            .unwrap_or_else(|error| panic!("the group `{group_key}` opens: {error}"))
    }

    async fn next(&self, handle: &mut crate::EffectGroupHandle) -> crate::GroupSettlement {
        tokio::time::timeout(
            SETTLE_BUDGET,
            self.scoped()
                .controller()
                .await_next_settlement(handle, tokio_util::sync::CancellationToken::new()),
        )
        .await
        .unwrap_or_else(|_| panic!("the child reported nothing inside the settle budget"))
        .unwrap_or_else(|error| panic!("the settlement is served: {error}"))
    }

    async fn close(&self, handle: crate::EffectGroupHandle) {
        self.scoped()
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
            .await
            .unwrap_or_else(|error| panic!("the group closes: {error}"));
    }

    fn body_runs(&self) -> usize {
        self.scenario.observation.executions_of("law_plain").len()
    }
}

/// A missing environment is the child's recorded outcome. A store fault while
/// resolving it is not: nothing settles while the fault lasts, the substrate
/// runs the child again, and the child settles once the store answers, having
/// run its body once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_process_env_store_fault_is_never_the_childs_recorded_outcome(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let stage = Stage::new(fixture, &format!("{prefix}-env-fault"), EnvRead::Missing).await;

    // Missing: the refusal is the child's outcome.
    let mut handle = stage.open(&format!("{prefix}-env-fault-missing")).await;
    let refused = stage
        .next(&mut handle)
        .await
        .outcome
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
    stage.close(handle).await;

    // Store fault: never recorded; the child settles once the store answers.
    stage.env_store.answer(EnvRead::StoreFault);
    let reads_before = stage.env_store.reads();
    let mut handle = stage.open(&format!("{prefix}-env-fault-live")).await;
    let absent = tokio::time::timeout(
        ABSENCE_BUDGET,
        stage
            .scoped()
            .controller()
            .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new()),
    )
    .await;
    assert!(
        absent.is_err(),
        "nothing may be recorded for a child whose run hit a live fault, got {absent:?}"
    );
    assert!(
        stage.env_store.reads() >= reads_before + 2,
        "the substrate ran the child again while the fault lasted ({} reads since {reads_before})",
        stage.env_store.reads()
    );
    assert_eq!(
        stage.body_runs(),
        0,
        "no attempt ran without an environment"
    );
    stage.env_store.answer(EnvRead::Healthy);
    let settlement = stage.next(&mut handle).await;
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
    else {
        panic!("the re-run child settles its tool invocation: {settlement:?}")
    };
    assert!(
        format!("{:?}", outcome.record.output).contains("plain"),
        "the child settles with the plain leaf's output"
    );
    assert_eq!(stage.body_runs(), 1, "the settling run ran the body once");
    stage.close(handle).await;
}

/// A child whose replay this build cannot serve settles with its park, handed
/// to the waiting turn as the park itself, and is not run again: every run by
/// this build would refuse the same replay (FIG-3586).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_divergent_child_settles_with_its_park(fixture: &ToolChildLawFixture, prefix: &str) {
    let stage = Stage::new(fixture, &format!("{prefix}-env-park"), EnvRead::Divergent).await;
    let mut handle = stage.open(&format!("{prefix}-env-park-group")).await;
    let parked = stage
        .next(&mut handle)
        .await
        .outcome
        .expect_err("a divergent child settles with its park");
    assert_eq!(
        parked.code,
        crate::RuntimeErrorCode::LashlangCellReplayDivergence,
        "the park is handed to the waiter as the divergence itself"
    );
    assert_eq!(
        parked.turn_failure_cause(),
        crate::TurnFailureCause::Parked,
        "the waiting turn parks on it rather than recording a failure"
    );
    assert_eq!(
        stage.env_store.reads(),
        1,
        "a park is not re-run: the child read its environment once"
    );
    assert_eq!(stage.body_runs(), 0, "a parked child runs no attempt");
    stage.close(handle).await;
}
