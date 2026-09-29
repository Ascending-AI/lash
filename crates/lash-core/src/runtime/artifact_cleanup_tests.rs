use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use super::*;
use crate::store::{
    ClaimToken, ClaimedObligation, ObligationKind, ObligationSettlement, ObligationStanding,
    SettleOutcome, StalledObligation, StoreError,
};
use crate::{ArtifactCleanupPlan, ExecutionScope, HostArtifactPin, ReferrerClaim};

fn journal(id: &str) -> lash_sansio::EffectJournalIdentity {
    ExecutionScope::runtime_operation(id)
        .journal_identity()
        .expect("an operation scope has a journal")
}

fn start_key() -> StartKey {
    StartKey::parse(&format!(
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
    rows: Mutex<BTreeMap<ObligationId, ArtifactCleanup>>,
}

#[async_trait::async_trait]
impl ObligationLedger for Ledger {
    fn kind(&self) -> ObligationKind {
        ObligationKind::ArtifactCleanup
    }

    async fn arm(
        &self,
        _key: &ObligationKey,
        _now_ms: u64,
    ) -> Result<Option<ObligationId>, StoreError> {
        Ok(None)
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
        _cleanup: &ArtifactCleanup,
        _now_ms: u64,
    ) -> Result<ObligationId, StoreError> {
        Err(StoreError::Backend("not armed in these tests".to_owned()))
    }

    async fn nudge(&self, _referrer: &ArtifactReferrer, _now_ms: u64) -> Result<bool, StoreError> {
        Ok(false)
    }

    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError> {
        Ok(self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned())
    }
}

/// Scripted authorities: which journals are settled, what a start key
/// registered, and where revisions stand.
#[derive(Default)]
struct Authorities {
    settled: Mutex<Vec<String>>,
    retained: Mutex<Option<RetainedStart>>,
    subscription: Mutex<Option<SubscriptionRevisionStanding>>,
    definition_current: Mutex<bool>,
}

impl Authorities {
    fn settle(&self, journal: &lash_sansio::EffectJournalIdentity) {
        self.settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(journal.key().to_owned());
    }
}

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for Authorities {
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
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

    async fn subscription_revision(
        &self,
        _revision: &SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String> {
        Ok(self
            .subscription
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(SubscriptionRevisionStanding {
                current: false,
                unbound_deliveries: false,
            }))
    }

    async fn definition_revision_current(
        &self,
        _revision: &DefinitionRevisionId,
    ) -> Result<bool, String> {
        Ok(*self
            .definition_current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

/// What each store was asked to apply, and a scripted failure per store.
#[derive(Default)]
struct Applied {
    env: Mutex<Vec<ResolvedArtifactCleanup>>,
    modules: Mutex<Vec<ResolvedArtifactCleanup>>,
    engine: Mutex<Vec<ResolvedArtifactCleanup>>,
    module_failure: Mutex<Option<fn() -> ArtifactStoreError>>,
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

#[async_trait::async_trait]
impl ProcessExecutionEnvStore for EnvStore {
    async fn publish_process_execution_env(
        &self,
        _claim: &ReferrerClaim,
        _env_ref: &ProcessExecutionEnvRef,
        _bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_process_execution_env(
        &self,
        _claim: &ReferrerClaim,
        _env_ref: &ProcessExecutionEnvRef,
    ) -> Result<(), ArtifactStoreError> {
        Ok(())
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        record(&self.0.env, cleanup);
        Ok(())
    }

    async fn get_process_execution_env(
        &self,
        _env_ref: &ProcessExecutionEnvRef,
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
        _claim: &ReferrerClaim,
        _module_ref: &str,
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

const ENGINE_KIND: &str = "cleanup-test-engine";

/// An engine whose start payload names one module and one artifact of its
/// own store, and which records what it is asked to end.
struct Engine(Arc<Applied>);

#[async_trait::async_trait]
impl crate::ProcessEngine for Engine {
    fn kind(&self) -> &'static str {
        ENGINE_KIND
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        unreachable!("cleanup never runs a process")
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<ArtifactName>, PluginError> {
        Ok(vec![
            name(ArtifactStoreId::LashlangModule, "mod-start"),
            name(ArtifactStoreId::Engine(ENGINE_KIND.to_owned()), "own-start"),
        ])
    }

    async fn end_artifact_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), PluginError> {
        record(&self.0.engine, cleanup);
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &ReferrerClaim,
        _artifact_ref: &str,
    ) -> Result<(), PluginError> {
        Ok(())
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
        engines,
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
        let id = ObligationId::new(format!("core:{}", cleanup.referrer.canonical_id()));
        let key = ObligationKey::ArtifactCleanup {
            referrer: cleanup.referrer.clone(),
        };
        self.ledger
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), cleanup);
        (id, key)
    }

    async fn deliver(&self, cleanup: ArtifactCleanup) -> Result<(), DeliveryFailure> {
        let (id, key) = self.arm(cleanup);
        self.relay.deliver(&id, &key, 1).await
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
        env.is_empty() && modules.is_empty() && engine.is_empty()
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
            .relay
            .deliver(&ObligationId::new("core:gone"), &key, 1)
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
        carry(name(ArtifactStoreId::LashlangModule, "mod-b")),
        carry(name(ArtifactStoreId::ProcessEnv, "env-a")),
        carry(name(ArtifactStoreId::LashlangModule, "mod-a")),
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
    let guard = ReferrerClaim::guarded(referrer.clone(), ArtifactCleanupPlan::AwaitJournal)
        .expect("an execution guard")
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
        env_ref: Some(ProcessExecutionEnvRef::new("env-retained")),
        input: Arc::new(ProcessInput::Engine {
            kind: ENGINE_KIND.to_owned(),
            payload: serde_json::json!({}),
        }),
    });
    let start = ArtifactReferrer::Start(start_key());
    let guard = ArtifactCleanup {
        referrer: start.clone(),
        plan: ArtifactCleanupPlan::AwaitStart {
            starter: journal("starter"),
        },
        gate: None,
    };
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
            vec![carry(name(ArtifactStoreId::LashlangModule, "mod-start"))]
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

/// ADR 0113 §3.3, "Lost": with no record, the start ends once its starter's
/// journal is settled, and carries nothing.
#[tokio::test]
async fn a_start_that_never_registered_ends_when_its_starter_settles() {
    let harness = harness();
    let starter = journal("starter");
    let start = ArtifactReferrer::Start(start_key());
    let guard = ArtifactCleanup {
        referrer: start.clone(),
        plan: ArtifactCleanupPlan::AwaitStart {
            starter: starter.clone(),
        },
        gate: None,
    };
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

/// ADR 0113 §3.4: a revision is held while it is current or a delivery
/// reserved under it is unbound, and ends only once its creator settles.
#[tokio::test]
async fn a_subscription_revision_waits_for_currency_bindings_and_its_creator() {
    let harness = harness();
    let creator = journal("register");
    let revision = ArtifactReferrer::SubscriptionRevision(
        SubscriptionRevisionId::new("sub".to_owned(), "inc".to_owned(), 2).expect("revision"),
    );
    let guard = ArtifactCleanup {
        referrer: revision,
        plan: ArtifactCleanupPlan::AwaitSubscriptionRevision {
            creator: creator.clone(),
        },
        gate: None,
    };
    harness.authorities.settle(&creator);
    for standing in [
        SubscriptionRevisionStanding {
            current: true,
            unbound_deliveries: false,
        },
        SubscriptionRevisionStanding {
            current: false,
            unbound_deliveries: true,
        },
    ] {
        *harness
            .authorities
            .subscription
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(standing);
        assert_eq!(
            harness.deliver(guard.clone()).await,
            Err(DeliveryFailure::NotYet),
            "{standing:?}"
        );
    }
    *harness
        .authorities
        .subscription
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    assert_eq!(harness.deliver(guard).await, Ok(()));
}

/// ADR 0113 §3.6: a definition revision ends when it is no longer its slot's
/// current resolvable revision and its creator settled.
#[tokio::test]
async fn a_definition_revision_ends_once_replaced_and_its_creator_settles() {
    let harness = harness();
    let creator = journal("register-definition");
    let guard = ArtifactCleanup {
        referrer: ArtifactReferrer::DefinitionRevision(
            DefinitionRevisionId::new("lash.process-definition:ns:name".to_owned(), 1)
                .expect("revision"),
        ),
        plan: ArtifactCleanupPlan::AwaitDefinitionRevision {
            creator: creator.clone(),
        },
        gate: None,
    };
    *harness
        .authorities
        .definition_current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    harness.authorities.settle(&creator);
    assert_eq!(
        harness.deliver(guard.clone()).await,
        Err(DeliveryFailure::NotYet)
    );
    *harness
        .authorities
        .definition_current
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    assert_eq!(harness.deliver(guard).await, Ok(()));
}

/// ADR 0113 §2.5: a carry whose bytes are gone stalls the row; any other
/// store failure is retried; neither is acknowledged.
#[tokio::test]
async fn a_missing_carry_is_refused_and_a_store_fault_is_retried() {
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
        harness.relay.deliver(&id, &key, 1).await,
        Err(DeliveryFailure::Undecodable(error)) if error.contains("synthetic_next")
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
async fn a_guard_that_does_not_fit_its_referrer_is_undecodable() {
    let harness = harness();
    let mismatched = ArtifactCleanup {
        referrer: host_pin(),
        plan: ArtifactCleanupPlan::AwaitJournal,
        gate: None,
    };
    assert!(matches!(
        harness.deliver(mismatched).await,
        Err(DeliveryFailure::Undecodable(_))
    ));
    assert!(harness.nothing_applied());
}
