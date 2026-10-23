use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use super::*;
use crate::runtime::obligations::relay::ObligationDelivery;
use crate::store::{
    ClaimToken, ClaimedObligation, ObligationId, ObligationKind, ObligationSettlement,
    ObligationStanding, SettleOutcome, StalledObligation, StoreError,
};
use crate::{ExecutionScope, HostArtifactPin, ReferrerClaim, ReferrerGuard};

fn journal(id: &str) -> lash_protocol_types::EffectJournalIdentity {
    ExecutionScope::runtime_operation(id)
        .journal_identity()
        .expect("an operation scope has a journal")
}

fn start_key() -> StartKey {
    crate::core_internal::StartKeyDerivation::LASH_START_PATHS
        .parse(&format!(
            "process-start-key:v1:intent:blake3:{}",
            "b".repeat(64)
        ))
        .expect("a rendered start key")
}

fn name(store: ArtifactStoreId, artifact_ref: &str) -> ArtifactName {
    ArtifactName {
        store,
        artifact_ref: artifact_ref.to_owned(),
    }
}

/// A ledger holding one cleanup body per obligation id.
#[derive(Default)]
struct Ledger {
    corrupt_reads: std::sync::atomic::AtomicBool,
    rows: Mutex<BTreeMap<ObligationId, ArtifactCleanup>>,
}

#[async_trait::async_trait]
impl ObligationLedger for Ledger {
    fn kind(&self) -> ObligationKind {
        ObligationKind::ArtifactCleanup
    }

    async fn claim_due(
        &self,
        _now_ms: u64,
        _claim_ttl_ms: u64,
        _limit: NonZeroUsize,
    ) -> Result<Vec<ClaimedObligation>, StoreError> {
        Ok(Vec::new())
    }

    async fn claim(
        &self,
        _id: &ObligationId,
        _token: &ClaimToken,
        _now_ms: u64,
        _claim_ttl_ms: u64,
    ) -> Result<Option<ClaimedObligation>, StoreError> {
        Ok(None)
    }

    async fn settle(
        &self,
        _id: &ObligationId,
        _token: &ClaimToken,
        _settlement: ObligationSettlement,
        _now_ms: u64,
    ) -> Result<SettleOutcome, StoreError> {
        Ok(SettleOutcome::Applied)
    }

    async fn rearm(&self, _id: &ObligationId, _now_ms: u64) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn list_stalled(
        &self,
        _after: Option<&ObligationId>,
        _limit: NonZeroUsize,
    ) -> Result<Vec<StalledObligation>, StoreError> {
        Ok(Vec::new())
    }

    async fn count_stalled(&self) -> Result<u64, StoreError> {
        Ok(0)
    }

    async fn standing(&self, _id: &ObligationId) -> Result<Option<ObligationStanding>, StoreError> {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl ArtifactCleanupLedger for Ledger {
    async fn arm_cleanup(
        &self,
        cleanup: &ArtifactCleanup,
        _now_ms: u64,
    ) -> Result<ObligationId, StoreError> {
        let id = ObligationKey::ArtifactCleanup {
            referrer: cleanup.referrer(),
        }
        .id();
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), cleanup.clone());
        Ok(id)
    }

    async fn nudge(&self, _referrer: &ArtifactReferrer, _now_ms: u64) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError> {
        if self.corrupt_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::StoredDataCorrupt {
                record_kind: "artifact_cleanup_obligation",
                message: "guard does not match row kind".into(),
            });
        }
        Ok(self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned())
    }
}

/// Scripted authorities: which journals are settled, what a start key
/// registered, and whether a frame is retained.
#[derive(Default)]
struct Authorities {
    settled: Mutex<Vec<String>>,
    retained: Mutex<Option<RetainedStart>>,
    frame_retained: Mutex<bool>,
}

impl Authorities {
    fn settle(&self, journal: &lash_protocol_types::EffectJournalIdentity) {
        self.settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(journal.key().to_owned());
    }
}

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for Authorities {
    async fn frame_is_retained(&self, _frame: &crate::FrameEnvironmentId) -> Result<bool, String> {
        Ok(*self
            .frame_retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    async fn journal_replay(
        &self,
        journal: &lash_protocol_types::EffectJournalIdentity,
    ) -> Result<JournalReplay, String> {
        let settled = self
            .settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|key| key == journal.key());
        Ok(if settled {
            JournalReplay::Settled
        } else {
            JournalReplay::MayReplay
        })
    }

    async fn retained_start(&self, _key: &StartKey) -> Result<Option<RetainedStart>, String> {
        Ok(self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }
}

/// What each store was asked to apply and to acquire, and a scripted
/// failure per store.
#[derive(Default)]
struct Applied {
    env: Mutex<Vec<ResolvedArtifactCleanup>>,
    modules: Mutex<Vec<ResolvedArtifactCleanup>>,
    definitions: Mutex<Vec<ResolvedArtifactCleanup>>,
    engine: Mutex<Vec<ResolvedArtifactCleanup>>,
    /// Descriptors the definition store holds, by id.
    descriptors: Mutex<BTreeMap<String, Vec<u8>>>,
    module_failure: Mutex<Option<fn() -> ArtifactStoreError>>,
    engine_failure: Mutex<Option<fn() -> ArtifactStoreError>>,
    /// Every edge an acquisition added, in any store.
    acquired: Mutex<Vec<(ArtifactReferrer, String)>>,
    /// Artifacts no store holds bytes for.
    absent: Mutex<Vec<String>>,
    /// A referrer every store has fenced.
    fenced: Mutex<Option<ArtifactReferrer>>,
}

impl Applied {
    fn acquire(&self, claim: &ReferrerClaim, artifact_ref: &str) -> Result<(), ArtifactStoreError> {
        let fenced = self
            .fenced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if fenced.as_ref() == Some(&claim.referrer()) {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer(),
            });
        }
        if self
            .absent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|absent| absent == artifact_ref)
        {
            return Err(ArtifactStoreError::ArtifactMissing {
                artifact_ref: artifact_ref.to_owned(),
            });
        }
        self.acquired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((claim.referrer(), artifact_ref.to_owned()));
        Ok(())
    }
}

fn record(into: &Mutex<Vec<ResolvedArtifactCleanup>>, cleanup: &ResolvedArtifactCleanup) {
    into.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(cleanup.clone());
}

fn taken(from: &Mutex<Vec<ResolvedArtifactCleanup>>) -> Vec<ResolvedArtifactCleanup> {
    std::mem::take(
        &mut *from
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

struct EnvStore(Arc<Applied>);

// A fleet with no recorded writer ranges: nothing here admits a new
// environment.
lash_core_store::impl_current_fleet_format!(EnvStore);

#[async_trait::async_trait]
impl ProcessEnvStore for EnvStore {
    async fn publish_process_env(
        &self,
        _claim: &ReferrerClaim,
        _env_ref: &ProcessEnvRef,
        _bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_process_env(
        &self,
        claim: &ReferrerClaim,
        env_ref: &ProcessEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        self.0.acquire(claim, env_ref.as_str())
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        record(&self.0.env, cleanup);
        Ok(())
    }

    async fn get_process_env(
        &self,
        _env_ref: &ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        Ok(None)
    }
}

struct Modules(Arc<Applied>);

#[async_trait::async_trait]
impl ModuleArtifactStore for Modules {
    async fn publish_module_artifact(
        &self,
        _claim: &ReferrerClaim,
        _module_ref: &str,
        _bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        self.0.acquire(claim, module_ref)
    }

    async fn hold_module_artifacts(
        &self,
        _claim: &ReferrerClaim,
        _artifacts: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn end_module_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        if let Some(failure) = *self
            .0
            .module_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(failure());
        }
        record(&self.0.modules, cleanup);
        Ok(())
    }

    async fn get_module_artifact(
        &self,
        _module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        Ok(None)
    }
}

struct Definitions(Arc<Applied>);

#[async_trait::async_trait]
impl ProcessDefinitionStore for Definitions {
    async fn publish_process_definition(
        &self,
        _claim: &ReferrerClaim,
        _id: &ProcessDefinitionId,
        _descriptor: &[u8],
        _manifest: &[ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &ProcessDefinitionId,
        _manifest: &[ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.0.acquire(claim, id.as_str())
    }

    async fn end_process_definition_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        record(&self.0.definitions, cleanup);
        Ok(())
    }

    async fn get_process_definition(
        &self,
        id: &ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        Ok(self
            .0
            .descriptors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id.as_str())
            .cloned())
    }
}

const ENGINE_KIND: &str = "cleanup-test-engine";

/// An engine whose start payload names one module and one artifact of its
/// own store, and which records what it is asked to end.
struct Engine(Arc<Applied>);

#[async_trait::async_trait]
impl lash_execution_contracts::process::ProcessEngine for Engine {
    fn settled_ran(
        &self,
        _kind: &lash_execution_contracts::process::EngineStepKind,
        _output: &lash_core_store::tool_run::SettledOutput,
    ) -> lash_execution_contracts::process::SettledRan {
        Default::default()
    }

    fn trace_class(&self) -> lash_execution_contracts::process::ProcessTraceClass {
        lash_execution_contracts::process::ProcessTraceClass::Internal
    }

    async fn inspect_link(
        &self,
        link: lash_vocabulary::LinkId,
        selector: &lash_execution_contracts::process::linking::ReachSelector,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(lash_execution_contracts::process::linking::LinkInspection {
            link,
            document: lash_vocabulary::DocumentId::from_bytes([0; 32]),
            selector: selector.clone(),
            operations: Default::default(),
            machine: Default::default(),
            artifacts: Vec::new(),
            signature: None,
        })
    }
    async fn inspect_start(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<
        lash_execution_contracts::process::linking::LinkInspection,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Err(
            lash_execution_contracts::process::linking::EnginePortError::Unsupported {
                operation: "generic fixture has no linked code".into(),
            },
        )
    }
    fn capabilities(&self) -> lash_vocabulary::claims::NodeRequirements {
        Default::default()
    }

    async fn check_args(
        &self,
        _signature: &lash_execution_contracts::process::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_execution_contracts::process::ArgsMode,
    ) -> std::result::Result<(), lash_execution_contracts::process::ArgsMismatch> {
        Err(
            lash_execution_contracts::process::ArgsMismatch::UnsupportedSignature {
                engine_kind: self.kind().into(),
            },
        )
    }

    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<ArtifactName>, PluginError> {
        if payload.get("program") == Some(&serde_json::json!("p")) {
            return Ok(vec![
                name(ArtifactStoreId::module(), "mod-definition"),
                name(ArtifactStoreId::ProcessEnv, "env-definition"),
            ]);
        }
        Ok(vec![
            name(ArtifactStoreId::module(), "mod-start"),
            name(ArtifactStoreId::Engine(ENGINE_KIND.to_owned()), "own-start"),
        ])
    }

    async fn end_artifact_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        if let Some(failure) = *self
            .0
            .engine_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(failure());
        }
        record(&self.0.engine, cleanup);
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        claim: &ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), PluginError> {
        self.0
            .acquire(claim, artifact_ref)
            .map_err(PluginError::from)
    }

    fn state_format(&self) -> lash_execution_contracts::process::EngineStateFormat {
        lash_execution_contracts::process::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn check_settings(
        &self,
        _payload: &serde_json::Value,
        settings: Option<&lash_execution_contracts::process::EngineSettings>,
    ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
        match settings {
            None => Ok(()),
            Some(_) => Err(
                lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                    "process engine `{}` takes no settings",
                    self.kind()
                )),
            ),
        }
    }

    fn default_settings(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<
        Option<lash_execution_contracts::process::EngineSettings>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(None)
    }

    fn advance(
        &self,
        _state: lash_execution_contracts::process::EngineState,
        _event: lash_execution_contracts::process::EngineEvent,
    ) -> Result<
        (
            lash_execution_contracts::process::EngineState,
            lash_execution_contracts::process::EngineAction,
        ),
        lash_execution_contracts::process::ProcessInfraError,
    > {
        unreachable!("cleanup never advances a process")
    }

    async fn resolve(
        &self,
        _reference: &lash_execution_contracts::process::ProcessDefinitionRef,
    ) -> Result<
        lash_execution_contracts::process::ProcessDefinitionResolution,
        lash_execution_contracts::process::ProcessDefinitionError,
    > {
        Ok(
            lash_execution_contracts::process::ProcessDefinitionResolution::new(
                lash_execution_contracts::process::ProcessSignature::Unknown,
            ),
        )
    }
}

struct Harness {
    relay: ArtifactCleanupRelay,
    ledger: Arc<Ledger>,
    authorities: Arc<Authorities>,
    applied: Arc<Applied>,
}

fn harness() -> Harness {
    let ledger = Arc::new(Ledger::default());
    let authorities = Arc::new(Authorities::default());
    let applied = Arc::new(Applied::default());
    let engines = ProcessEngineRegistry::new().with_registration(
        crate::ProcessEngineRegistration::accepting(Arc::new(Engine(Arc::clone(&applied)))),
    );
    let relay = ArtifactCleanupRelay::new(ArtifactCleanupPorts {
        ledger: Arc::clone(&ledger) as Arc<dyn ArtifactCleanupLedger>,
        authorities: Arc::clone(&authorities) as Arc<dyn ArtifactCleanupAuthorities>,
        process_env: Arc::new(EnvStore(Arc::clone(&applied))),
        modules: Arc::new(Modules(Arc::clone(&applied))),
        definitions: Arc::new(Definitions(Arc::clone(&applied))),
        turn_preludes: Arc::new(crate::testing::UnavailableTurnPreludeStore),
        engines,
        attachments: Arc::new(crate::attachments::NoopAttachmentReferrers),
        clock: Arc::new(crate::runtime::SystemClock),
    });
    Harness {
        relay,
        ledger,
        authorities,
        applied,
    }
}

impl Harness {
    fn arm(&self, cleanup: ArtifactCleanup) -> (ObligationId, ObligationKey) {
        let key = ObligationKey::ArtifactCleanup {
            referrer: cleanup.referrer(),
        };
        let id = key.id();
        self.ledger
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), cleanup);
        (id, key)
    }

    async fn deliver(&self, cleanup: ArtifactCleanup) -> Result<(), DeliveryFailure> {
        let (id, key) = self.arm(cleanup);
        self.deliver_row(&id, &key).await
    }

    /// The relay's first attempt at the row `id` and `key` name.
    async fn deliver_row(
        &self,
        id: &ObligationId,
        key: &ObligationKey,
    ) -> Result<(), DeliveryFailure> {
        self.relay
            .deliver(ObligationDelivery {
                id,
                key,
                token: &ClaimToken::new("artifact-cleanup-harness"),
                attempt: 1,
                started_ms: 0,
            })
            .await
    }

    /// What every store applied since the last look: (env, modules, engine).
    fn applied(
        &self,
    ) -> (
        Vec<ResolvedArtifactCleanup>,
        Vec<ResolvedArtifactCleanup>,
        Vec<ResolvedArtifactCleanup>,
    ) {
        (
            taken(&self.applied.env),
            taken(&self.applied.modules),
            taken(&self.applied.engine),
        )
    }

    fn nothing_applied(&self) -> bool {
        let (env, modules, engine) = self.applied();
        env.is_empty()
            && modules.is_empty()
            && engine.is_empty()
            && taken(&self.applied.definitions).is_empty()
    }
}

fn resolved(referrer: &ArtifactReferrer, carries: Vec<ArtifactCarry>) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer: referrer.clone(),
        carries,
    }
}

fn host_pin() -> ArtifactReferrer {
    ArtifactReferrer::HostPin(HostArtifactPin::mint())
}

#[tokio::test]
async fn a_row_another_relay_settled_is_delivered_without_touching_a_store() {
    let harness = harness();
    let referrer = host_pin();
    let key = ObligationKey::ArtifactCleanup {
        referrer: referrer.clone(),
    };
    assert_eq!(
        harness
            .deliver_row(&ObligationId::new("core:gone"), &key)
            .await,
        Ok(())
    );
    assert!(harness.nothing_applied());
}

/// ADR 0113 §2.5 step 4: each store receives the referrer and only its own
/// carries, in artifact order; the engine registry receives the engine-store
/// share.
#[tokio::test]
async fn an_ended_referrer_hands_every_store_its_own_carries() {
    let harness = harness();
    let ended = host_pin();
    let to = ArtifactReferrer::ProcessRecord(ProcessId::fixture("successor"));
    let carry = |artifact: ArtifactName| ArtifactCarry {
        artifact,
        to: to.clone(),
    };
    let carries = vec![
        carry(name(ArtifactStoreId::module(), "mod-b")),
        carry(name(ArtifactStoreId::ProcessEnv, "env-a")),
        carry(name(ArtifactStoreId::module(), "mod-a")),
        carry(name(ArtifactStoreId::Engine(ENGINE_KIND.to_owned()), "own")),
    ];
    assert_eq!(
        harness
            .deliver(ArtifactCleanup::ended(ended.clone(), carries.clone(), None))
            .await,
        Ok(())
    );
    let (env, modules, engine) = harness.applied();
    assert_eq!(env, vec![resolved(&ended, vec![carries[1].clone()])]);
    assert_eq!(
        modules,
        vec![resolved(
            &ended,
            vec![carries[2].clone(), carries[0].clone()]
        )]
    );
    assert_eq!(engine, vec![resolved(&ended, vec![carries[3].clone()])]);
}

/// ADR 0113 §4.1: a switch's cleanup severs nothing while the switching
/// turn may still replay.
#[tokio::test]
async fn a_gate_that_may_replay_defers_and_a_settled_gate_applies() {
    let harness = harness();
    let gate = journal("switching-turn");
    let cleanup = ArtifactCleanup::ended(host_pin(), Vec::new(), Some(gate.clone()));
    assert_eq!(
        harness.deliver(cleanup.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    assert!(harness.nothing_applied());
    harness.authorities.settle(&gate);
    assert_eq!(harness.deliver(cleanup).await, Ok(()));
    let (env, modules, engine) = harness.applied();
    assert_eq!((env.len(), modules.len(), engine.len()), (1, 1, 1));
}

#[tokio::test]
async fn an_execution_guard_ends_when_its_journal_settles() {
    let harness = harness();
    let scope = journal("cell");
    let referrer = ArtifactReferrer::Execution(scope.clone());
    let guard = ReferrerClaim::guarded(ReferrerGuard::Journal(scope.clone()))
        .guard_cleanup()
        .expect("a guarded claim arms a cleanup");
    assert_eq!(
        harness.deliver(guard.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    assert!(harness.nothing_applied());
    harness.authorities.settle(&scope);
    assert_eq!(harness.deliver(guard).await, Ok(()));
    let (env, _, _) = harness.applied();
    assert_eq!(env, vec![resolved(&referrer, Vec::new())]);
}

/// ADR 0113 §3.3, §4.3: a registered start carries the retained record's
/// content onto its record — never this attempt's — before the key is
/// fenced.
#[tokio::test]
async fn a_registered_start_carries_the_retained_record_onto_it() {
    let harness = harness();
    let process_id = ProcessId::fixture("retained");
    *harness
        .authorities
        .retained
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RetainedStart {
        process_id: process_id.clone(),
        env_ref: Some(ProcessEnvRef::new("env-retained")),
        input: Arc::new(ProcessInput::Engine {
            kind: ENGINE_KIND.to_owned(),
            payload: serde_json::json!({}),
            launch: lash_execution_contracts::testing::fixture_engine_launch(),
        }),
        definition_id: None,
    });
    let start = ArtifactReferrer::Start(start_key());
    let guard = ArtifactCleanup::Await(ReferrerGuard::Start {
        start_key: start_key(),
        starter: journal("starter"),
    });
    assert_eq!(harness.deliver(guard).await, Ok(()));
    let to = ArtifactReferrer::ProcessRecord(process_id);
    let carry = |artifact: ArtifactName| ArtifactCarry {
        artifact,
        to: to.clone(),
    };
    let (env, modules, engine) = harness.applied();
    assert_eq!(
        env,
        vec![resolved(
            &start,
            vec![carry(name(ArtifactStoreId::ProcessEnv, "env-retained"))]
        )]
    );
    assert_eq!(
        modules,
        vec![resolved(
            &start,
            vec![carry(name(ArtifactStoreId::module(), "mod-start"))]
        )]
    );
    assert_eq!(
        engine,
        vec![resolved(
            &start,
            vec![carry(name(
                ArtifactStoreId::Engine(ENGINE_KIND.to_owned()),
                "own-start"
            ))]
        )]
    );
}

/// ADR 0113 §3.6: a start by id carries its definition's descriptor and its
/// whole manifest onto its record, beside the retained record's own content,
/// and every store, the definition store included, ends the start's key.
#[tokio::test]
async fn a_start_by_id_carries_its_descriptor_and_manifest_onto_its_record() {
    let harness = harness();
    let draft = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        ENGINE_KIND,
        serde_json::json!({"program": "p"}),
        [
            name(ArtifactStoreId::module(), "mod-definition"),
            name(ArtifactStoreId::ProcessEnv, "env-definition"),
        ],
    )
    .expect("a well-formed draft");
    let id = draft.id();
    harness
        .applied
        .descriptors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id.as_str().to_owned(), draft.to_store_bytes());
    let process_id = ProcessId::fixture("by-id");
    *harness
        .authorities
        .retained
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RetainedStart {
        process_id: process_id.clone(),
        env_ref: Some(ProcessEnvRef::new("env-retained")),
        input: Arc::new(ProcessInput::Engine {
            kind: ENGINE_KIND.to_owned(),
            payload: serde_json::json!({}),
            launch: lash_execution_contracts::testing::fixture_engine_launch(),
        }),
        definition_id: Some(id.clone()),
    });
    let start = ArtifactReferrer::Start(start_key());
    let guard = ArtifactCleanup::Await(ReferrerGuard::Start {
        start_key: start_key(),
        starter: journal("starter"),
    });
    assert_eq!(harness.deliver(guard).await, Ok(()));
    let to = ArtifactReferrer::ProcessRecord(process_id);
    let carry = |artifact: ArtifactName| ArtifactCarry {
        artifact,
        to: to.clone(),
    };
    assert_eq!(
        taken(&harness.applied.definitions),
        vec![resolved(
            &start,
            vec![carry(name(ArtifactStoreId::ProcessDefinition, id.as_str()))]
        )]
    );
    let (env, modules, _) = harness.applied();
    assert_eq!(
        env,
        vec![resolved(
            &start,
            vec![
                carry(name(ArtifactStoreId::ProcessEnv, "env-definition")),
                carry(name(ArtifactStoreId::ProcessEnv, "env-retained")),
            ]
        )]
    );
    assert_eq!(
        modules,
        vec![resolved(
            &start,
            vec![
                carry(name(ArtifactStoreId::module(), "mod-definition")),
                carry(name(ArtifactStoreId::module(), "mod-start")),
            ]
        )]
    );
}

impl Harness {
    fn retain(&self, process_id: &ProcessId) {
        *self
            .authorities
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(RetainedStart {
            process_id: process_id.clone(),
            env_ref: Some(ProcessEnvRef::new("env-retained")),
            input: Arc::new(ProcessInput::Engine {
                kind: ENGINE_KIND.to_owned(),
                payload: serde_json::json!({}),
                launch: lash_execution_contracts::testing::fixture_engine_launch(),
            }),
            definition_id: None,
        });
    }

    fn acquired(&self) -> Vec<(ArtifactReferrer, String)> {
        std::mem::take(
            &mut *self
                .applied
                .acquired
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// FIG-4130, ADR 0113 §3.3: a refusal's end of `Start(key)` carries nothing,
/// yet a record the key registered meanwhile has its content held under its
/// `ProcessRecord` before the key's edges are severed. A name with no stored
/// bytes was never `Start(key)`'s to keep, and is skipped.
#[tokio::test]
async fn an_ended_start_holds_the_key_record_content_before_it_severs() {
    let harness = harness();
    let process_id = ProcessId::fixture("registered-meanwhile");
    harness.retain(&process_id);
    harness
        .applied
        .absent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push("mod-start".to_owned());
    let start = ArtifactReferrer::Start(start_key());
    assert_eq!(
        harness
            .deliver(ArtifactCleanup::ended(start.clone(), Vec::new(), None))
            .await,
        Ok(())
    );
    let record = ArtifactReferrer::ProcessRecord(process_id);
    assert_eq!(
        harness.acquired(),
        vec![
            (record.clone(), "env-retained".to_owned()),
            (record, "own-start".to_owned()),
        ]
    );
    let (env, modules, engine) = harness.applied();
    assert_eq!(env, vec![resolved(&start, Vec::new())]);
    assert_eq!(modules, vec![resolved(&start, Vec::new())]);
    assert_eq!(engine, vec![resolved(&start, Vec::new())]);
}

/// A pruned record's own cleanup owns what it held: the end holds nothing
/// under it and severs the key as usual.
#[tokio::test]
async fn an_ended_start_holds_nothing_under_a_pruned_record() {
    let harness = harness();
    let process_id = ProcessId::fixture("pruned");
    harness.retain(&process_id);
    *harness
        .applied
        .fenced
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(ArtifactReferrer::ProcessRecord(process_id));
    let start = ArtifactReferrer::Start(start_key());
    assert_eq!(
        harness
            .deliver(ArtifactCleanup::ended(start.clone(), Vec::new(), None))
            .await,
        Ok(())
    );
    assert_eq!(harness.acquired(), Vec::new());
    let (env, modules, engine) = harness.applied();
    assert_eq!((env.len(), modules.len(), engine.len()), (1, 1, 1));
}

/// ADR 0113 §3.3, "Lost": with no record, the start ends once its starter's
/// journal is settled, and carries nothing.
#[tokio::test]
async fn a_start_that_never_registered_ends_when_its_starter_settles() {
    let harness = harness();
    let starter = journal("starter");
    let start = ArtifactReferrer::Start(start_key());
    let guard = ArtifactCleanup::Await(ReferrerGuard::Start {
        start_key: start_key(),
        starter: starter.clone(),
    });
    assert_eq!(
        harness.deliver(guard.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    assert!(harness.nothing_applied());
    harness.authorities.settle(&starter);
    assert_eq!(harness.deliver(guard).await, Ok(()));
    let (env, modules, engine) = harness.applied();
    assert_eq!(env, vec![resolved(&start, Vec::new())]);
    assert_eq!(modules, vec![resolved(&start, Vec::new())]);
    assert_eq!(engine, vec![resolved(&start, Vec::new())]);
}

/// ADR 0113 §2.5: a carry whose bytes are gone stalls the row; any other
/// store failure is retried; neither is acknowledged.
#[tokio::test]
async fn a_missing_carry_is_refused_and_a_store_fault_is_retried() {
    let engine_harness = harness();
    *engine_harness
        .applied
        .engine_failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(|| ArtifactStoreError::Backend("connection reset".to_owned()));
    let ended = ArtifactCleanup::ended(host_pin(), Vec::new(), None);
    assert!(matches!(
        engine_harness.deliver(ended).await,
        Err(DeliveryFailure::Retryable(_))
    ));

    let harness = harness();
    let ended = ArtifactCleanup::ended(host_pin(), Vec::new(), None);
    *harness
        .applied
        .module_failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(|| ArtifactStoreError::CarryArtifactMissing {
            artifact_ref: "mod".to_owned(),
            to: ArtifactReferrer::ProcessRecord(ProcessId::fixture("to")),
        });
    assert!(matches!(
        harness.deliver(ended.clone()).await,
        Err(DeliveryFailure::Refused(_))
    ));
    *harness
        .applied
        .module_failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(|| ArtifactStoreError::Backend("connection reset".to_owned()));
    assert!(matches!(
        harness.deliver(ended).await,
        Err(DeliveryFailure::Retryable(_))
    ));
    let (_, _, engine) = harness.applied();
    assert!(engine.is_empty(), "no store after the failing one is asked");
}

#[tokio::test]
async fn cleanup_never_counts_an_undecodable_edge_as_absent() {
    let harness = harness();
    let cleanup = ArtifactCleanup::ended(host_pin(), Vec::new(), None);
    let (id, key) = harness.arm(cleanup);
    *harness
        .applied
        .module_failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(|| ArtifactStoreError::Incompatible {
            refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                surface: "artifact referrer edge kind".to_owned(),
                label: "synthetic_next".to_owned(),
            },
        });
    assert!(matches!(
        harness.deliver_row(&id, &key).await,
        Err(DeliveryFailure::Undecodable(error))
            if error.code == crate::RuntimeErrorCode::StoreIncompatible
                && error.message.contains("synthetic_next")
    ));
    assert!(
        harness
            .ledger
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    );
    let (_, modules, engine) = harness.applied();
    assert!(modules.is_empty(), "the module store did not apply cleanup");
    assert!(engine.is_empty(), "later stores were not asked");
}

#[tokio::test]
async fn a_prepared_frame_is_reclaimed_only_after_its_failed_commit_cannot_replay() {
    let harness = harness();
    let scope = journal("failed-frame-prepare");
    let predecessor = ArtifactReferrer::FrameEnvironment(crate::FrameEnvironmentId::new(
        lash_vocabulary::identity::SessionId::from("carry"),
        crate::FrameNodeId::new("old").unwrap(),
    ));
    let successor_frame = crate::FrameEnvironmentId::new(
        lash_vocabulary::identity::SessionId::from("carry"),
        crate::FrameNodeId::new("never-committed").unwrap(),
    );
    let successor = ArtifactReferrer::FrameEnvironment(successor_frame.clone());
    let claim = ReferrerClaim::guarded(ReferrerGuard::Frame {
        frame: successor_frame,
        creator: scope.clone(),
    });
    let ports = crate::ArtifactReferrerPorts::new(
        Arc::new(Modules(harness.applied.clone())),
        Arc::new(EnvStore(harness.applied.clone())),
        Arc::new(Definitions(harness.applied.clone())),
        Arc::new(crate::attachments::NoopAttachmentReferrers),
        harness.ledger.clone(),
        Arc::new(crate::testing::TestClock::new(1)),
    );
    let draft = ProcessDefinitionDraft::new(
        ENGINE_KIND,
        serde_json::Value::Null,
        [
            name(ArtifactStoreId::module(), "mod-start"),
            name(ArtifactStoreId::Engine(ENGINE_KIND.into()), "own-start"),
        ],
    )
    .unwrap();
    harness
        .applied
        .descriptors
        .lock()
        .unwrap()
        .insert(draft.id().to_string(), draft.to_store_bytes());
    let engines = ProcessEngineRegistry::new()
        .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
            Engine(harness.applied.clone()),
        )))
        .with_artifact_ports(ports.clone());
    let transition = crate::store::FrameTransition {
        ended: match predecessor.clone() {
            ArtifactReferrer::FrameEnvironment(frame) => frame,
            _ => unreachable!(),
        },
        successor: match successor.clone() {
            ArtifactReferrer::FrameEnvironment(frame) => frame,
            _ => unreachable!(),
        },
        carries: vec![name(
            ArtifactStoreId::ProcessDefinition,
            draft.id().as_str(),
        )],
        gate: scope.clone(),
    };
    crate::runtime::frame_definition_carry::prepare(&engines, Some(&transition))
        .await
        .unwrap();
    let acquired = std::mem::take(&mut *harness.applied.acquired.lock().unwrap());
    assert_eq!(
        acquired,
        vec![(successor.clone(), "own-start".into())],
        "only the engine share prepares before SQL activation"
    );
    let guard = claim.guard_cleanup().unwrap();
    assert_eq!(
        harness.deliver(guard.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    assert!(harness.nothing_applied());
    harness.authorities.settle(&scope);
    harness.deliver(guard).await.unwrap();
    let (_, _, engine) = harness.applied();
    assert_eq!(engine, vec![resolved(&successor, vec![])]);
    assert_ne!(
        engine[0].referrer, predecessor,
        "the predecessor remains usable after a failed successor commit"
    );
}

#[tokio::test]
async fn a_committed_prepared_frame_retains_its_engine_share_until_ended_without_a_carry() {
    let harness = harness();
    let scope = journal("committed-frame-prepare");
    let frame_id = crate::FrameEnvironmentId::new(
        lash_vocabulary::identity::SessionId::from("carry"),
        crate::FrameNodeId::new("committed").unwrap(),
    );
    let frame = ArtifactReferrer::FrameEnvironment(frame_id.clone());
    let guard = ReferrerClaim::guarded(ReferrerGuard::Frame {
        frame: frame_id,
        creator: scope.clone(),
    })
    .guard_cleanup()
    .unwrap();
    *harness.authorities.frame_retained.lock().unwrap() = true;
    harness.authorities.settle(&scope);
    assert_eq!(harness.deliver(guard).await, Err(DeliveryFailure::NotYet));
    assert!(harness.nothing_applied());
    harness
        .deliver(ArtifactCleanup::ended(frame.clone(), vec![], None))
        .await
        .unwrap();
    let (_, _, engine) = harness.applied();
    assert_eq!(engine, vec![resolved(&frame, vec![])]);
}

#[tokio::test]
async fn corrupt_cleanup_read_stalls_without_applying() {
    let harness = harness();
    harness
        .ledger
        .corrupt_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let result = harness
        .deliver(ArtifactCleanup::ended(host_pin(), vec![], None))
        .await;
    assert!(
        matches!(result, Err(DeliveryFailure::Undecodable(_))),
        "corruption cannot retry: {result:?}"
    );
    assert!(harness.nothing_applied());
}

/// L-PIN: after interrupted preparation and reopen, the session's graph
/// retirement guard protects module bytes until its fork releases the graph.
#[tokio::test]
async fn session_package_cleanup_waits_for_the_retained_graph_after_reopen() {
    use lash_core_store::store::{AttachmentReferrers, SessionCatalogStore, SessionCommitStore};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-pins.db");
    let session_id = crate::SessionId::fixture("retained-package-parent");
    let child_id = crate::SessionId::fixture("retained-package-fork");
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::Session(session_id.clone())).unwrap();
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &path,
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .unwrap();
    let store = stores.session_store_factory();
    let request = crate::testing::store_fixtures::root_session_request(&session_id);
    store.admit_session(&request).await.unwrap();
    let mut state = lash_core_store::session_state::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core_store::session_state::RuntimeSessionState::fixture(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ))
    };
    state.ensure_agent_frame_initialized();
    let receipt = store
        .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state))
        .await
        .unwrap();
    store
        .fork_session(
            &lash_core_store::session_store_factory_types::ForkSessionRequest {
                session_id: child_id.clone(),
                source_session_id: session_id.clone(),
                head_revision: receipt.head_revision,
                relation: lash_core_store::session_identity::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                config: request.config,
                retention: lash_core_store::store::Retention::UntilGc,
            },
        )
        .await
        .unwrap();
    store
        .publish_module_artifact(&claim, "interrupted-package-preparation", b"partial")
        .await
        .unwrap();
    drop(store);
    drop(stores);
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &path,
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .unwrap();
    let store = stores.session_store_factory();
    let mut harness = harness();
    harness.relay.ports.attachments = store.clone();
    harness.relay.ports.modules = store.clone();
    let cleanup = ArtifactCleanup::Await(ReferrerGuard::SessionGraphRetired(session_id.clone()));
    assert_eq!(
        harness.deliver(cleanup.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    store.delete_session(&session_id).await.unwrap();
    assert_eq!(
        store.session_referrer_state(&session_id).await.unwrap(),
        crate::SessionReferrerState::DeletedRetained
    );
    assert_eq!(
        harness.deliver(cleanup.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    assert_eq!(
        store
            .get_module_artifact("interrupted-package-preparation")
            .await
            .unwrap(),
        Some(b"partial".to_vec())
    );
    let late = store
        .acquire_module_artifact(&claim, "interrupted-package-preparation")
        .await;
    assert!(matches!(
        late,
        Err(ArtifactStoreError::PackageRefused(
            lash_durable::packages::PackageRefusal::SessionNotLive(_)
        ))
    ));
    store.delete_session(&child_id).await.unwrap();
    assert_eq!(
        store.session_referrer_state(&session_id).await.unwrap(),
        crate::SessionReferrerState::DeletedRetired
    );
    assert_eq!(harness.deliver(cleanup.clone()).await, Ok(()));
    assert_eq!(harness.deliver(cleanup).await, Ok(()));
    assert!(
        store
            .get_module_artifact("interrupted-package-preparation")
            .await
            .unwrap()
            .is_none()
    );
}
