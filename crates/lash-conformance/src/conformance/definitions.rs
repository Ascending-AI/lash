//! Immutable process definitions under referrers (ADR 0113 §3.6), on every
//! store that registers them.
//!
//! A definition is a descriptor in the store set's definition store plus the
//! manifest it names, held as one closure by every reader. These laws drive
//! the real store, the real start staging and the real artifact-cleanup relay:
//!
//! - a definition is never reclaimed while any referrer of any kind holds it,
//!   and is reclaimed once the last one is severed, also under concurrent
//!   publishers, pinners, starters and releasers;
//! - a start by id replays exactly at every crash boundary of the design's
//!   crash table: a redrive of a recorded or registered start never mints a
//!   second process, and a start that never registered leaves none.
//!
//! A crash is the attempt stopping where the boundary names; recovery is the
//! redrive the journal would make, with the starter's journal verdict under
//! the law's control, and the relay applying whatever cleanup is due.

use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::ClockWallTime as _;
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupAuthorities, ArtifactCleanupPorts, ArtifactCleanupRelay, RetainedStart,
    SubscriptionRevisionStanding,
};
use lash_core::runtime::drive::relay::{RelayPolicy, relay_due};
use lash_core::testing::TestClock;

const DEFINITION_ENGINE: &str = "definition-law";

/// An engine whose definition value names one module, which is every
/// artifact a start of it reads.
struct ModuleDefinitionEngine;

fn module_of(payload: &serde_json::Value) -> Result<String, crate::PluginError> {
    payload
        .get("module")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| crate::PluginError::Session("the definition names no module".into()))
}

#[async_trait::async_trait]
impl crate::ProcessEngine for ModuleDefinitionEngine {
    fn kind(&self) -> &'static str {
        DEFINITION_ENGINE
    }

    async fn run(
        &self,
        _context: crate::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        Ok(
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(vec![crate::ArtifactName {
            store: crate::ArtifactStoreId::LashlangModule,
            artifact_ref: module_of(payload)?,
        }])
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the engine stores no artifact `{artifact_ref}`"
        )))
    }

    async fn resolve(
        &self,
        reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionRefusal> {
        module_of(reference.definition.as_json()).map_err(|error| {
            crate::ProcessDefinitionRefusal::UnresolvableDefinition {
                engine_kind: reference.engine_kind.clone(),
                message: error.to_string(),
            }
        })?;
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::known(serde_json::json!({"returns": "null"})),
            Vec::new(),
        ))
    }
}

/// The authorities the relay asks: journal verdicts the law sets (a journal
/// it has not settled may replay), and the record a start key holds.
struct LawAuthorities {
    registry: Arc<dyn crate::ProcessRegistry>,
    settled: Mutex<BTreeMap<String, bool>>,
}

#[async_trait::async_trait]
impl ArtifactCleanupAuthorities for LawAuthorities {
    async fn frame_is_retained(
        &self,
        _frame: &lash_core::FrameEnvironmentId,
    ) -> Result<bool, String> {
        Ok(false)
    }

    async fn journal_replay(
        &self,
        journal: &crate::EffectJournalIdentity,
    ) -> Result<crate::JournalReplay, String> {
        let settled = self
            .settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(journal.key())
            .copied()
            .unwrap_or(false);
        Ok(if settled {
            crate::JournalReplay::Settled
        } else {
            crate::JournalReplay::MayReplay
        })
    }

    async fn retained_start(&self, key: &crate::StartKey) -> Result<Option<RetainedStart>, String> {
        Ok(self
            .registry
            .get_process_by_start_key(key)
            .await
            .map_err(|error| error.to_string())?
            .map(|record| RetainedStart {
                process_id: record.id,
                env_ref: record.env_ref,
                input: record.input,
                definition_id: record.identity.definition_id,
            }))
    }

    async fn subscription_revision(
        &self,
        revision: &crate::SubscriptionRevisionId,
    ) -> Result<SubscriptionRevisionStanding, String> {
        Err(format!(
            "the law asks no subscription revision `{revision:?}`"
        ))
    }
}

/// One store under test, its ports, its engines and its relay.
struct World {
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
    engines: crate::ProcessEngineRegistry,
    authorities: Arc<LawAuthorities>,
    relay: ArtifactCleanupRelay,
    clock: TestClock,
    /// Names this law's rows apart from every other law's on a shared store.
    tag: String,
}

#[expect(clippy::expect_used, reason = "conformance law validates each step")]
impl World {
    fn new(registry: Arc<dyn crate::ProcessRegistry>, ports: crate::ArtifactReferrerPorts) -> Self {
        let engines = crate::ProcessEngineRegistry::new()
            .with_registration(crate::ProcessEngineRegistration::accepting(Arc::new(
                ModuleDefinitionEngine,
            )))
            .with_artifact_ports(ports.clone());
        let authorities = Arc::new(LawAuthorities {
            registry: Arc::clone(&registry),
            settled: Mutex::new(BTreeMap::new()),
        });
        let relay = ArtifactCleanupRelay::new(ArtifactCleanupPorts {
            ledger: Arc::clone(ports.cleanup()),
            authorities: Arc::clone(&authorities) as Arc<dyn ArtifactCleanupAuthorities>,
            process_env: Arc::clone(ports.env()),
            modules: Arc::clone(ports.modules()),
            definitions: Arc::clone(ports.definitions()),
            engines: engines.clone(),
            attachments: Arc::clone(ports.attachments()),
            clock: Arc::new(crate::SystemClock),
        })
        .with_policy(RelayPolicy {
            base_backoff_ms: 1,
            max_backoff_ms: 1,
            ..RelayPolicy::default()
        });
        // Ahead of every row the store arms on the wall clock.
        let clock = TestClock::new(crate::SystemClock.timestamp_ms() + 3_600_000);
        let pin = crate::HostArtifactPin::mint();
        let tag = pin.as_str()[pin.as_str().len() - 12..].to_owned();
        Self {
            registry,
            ports,
            engines,
            authorities,
            relay,
            clock,
            tag,
        }
    }

    fn journal(&self, name: &str) -> crate::EffectJournalIdentity {
        crate::ExecutionScope::runtime_operation(format!("{name}-{}", self.tag))
            .journal_identity()
            .expect("a runtime-operation journal")
    }

    fn settle(&self, journal: &crate::EffectJournalIdentity) {
        self.authorities
            .settled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(journal.key().to_owned(), true);
    }

    fn key(&self, name: &str) -> crate::StartKey {
        crate::StartKey::for_host(format!("{name}-{}", self.tag))
    }

    /// A module and the draft of a definition that reads it.
    fn draft(&self, name: &str) -> (String, crate::ProcessDefinitionDraft) {
        let module_ref = format!("definition-law-module-{name}-{}", self.tag);
        let draft = crate::ProcessDefinitionDraft::new(
            DEFINITION_ENGINE,
            serde_json::json!({ "module": module_ref }),
            [crate::ArtifactName {
                store: crate::ArtifactStoreId::LashlangModule,
                artifact_ref: module_ref.clone(),
            }],
        )
        .expect("a well-formed draft");
        (module_ref, draft)
    }

    /// Publish `draft`'s module and then `draft` under `claim`, as a host
    /// publishes under its pin and realization under its execution.
    async fn publish(
        &self,
        claim: &crate::ReferrerClaim,
        module_ref: &str,
        draft: &crate::ProcessDefinitionDraft,
    ) -> Result<crate::ProcessDefinition, crate::PluginError> {
        self.ports
            .modules()
            .publish_module_artifact(claim, module_ref, module_ref.as_bytes())
            .await?;
        self.ports
            .publish_definition(&self.engines, claim, draft)
            .await
    }

    async fn acquire(
        &self,
        claim: &crate::ReferrerClaim,
        id: &crate::ProcessDefinitionId,
    ) -> crate::DefinitionAcquisition {
        self.ports
            .acquire_definition(&self.engines, claim, id)
            .await
            .expect("acquire the definition's closure")
    }

    /// Whether the descriptor and its module are both stored, and neither
    /// without the other.
    async fn held(&self, module_ref: &str, id: &crate::ProcessDefinitionId) -> bool {
        let descriptor = self
            .ports
            .definitions()
            .get_process_definition(id)
            .await
            .expect("read the descriptor")
            .is_some();
        let module = self
            .ports
            .modules()
            .get_module_artifact(module_ref)
            .await
            .expect("read the module")
            .is_some();
        assert_eq!(
            descriptor, module,
            "a descriptor and its manifest live and go together"
        );
        descriptor
    }

    fn stores<'a>(
        &'a self,
        starter: &'a crate::EffectJournalIdentity,
    ) -> crate::ProcessStartStores<'a> {
        crate::ProcessStartStores {
            registry: self.registry.as_ref(),
            env_store: Some(self.ports.env()),
            engines: Some(&self.engines),
            engines_required: true,
            session_catalog: None,
            session_turn_default: None,
            session_turn_admission: None,
            executor: "definition conformance start",
            starter,
            trigger_route: None,
        }
    }

    /// One start of `id` under `key`: the realization a journaled start runs.
    async fn start(
        &self,
        key: &crate::StartKey,
        id: &crate::ProcessDefinitionId,
        starter: &crate::EffectJournalIdentity,
    ) -> Result<crate::RegisteredProcessStart, crate::RuntimeEffectControllerError> {
        let registration = crate::ProcessRegistration::new(
            crate::ProcessInput::Definition {
                signature_claim: None,
                definition_id: id.clone(),
                args: serde_json::Map::from_iter([("n".to_owned(), serde_json::json!(1))]),
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_start_key(Some(key.clone()));
        let env_ref = crate::publish_process_execution_env(
            self.ports.env().as_ref(),
            &crate::ReferrerClaim::guarded(crate::ReferrerGuard::Journal(starter.clone())),
            &env_spec(),
        )
        .await?;
        crate::register_process_start(
            &self.stores(starter),
            registration.with_execution_env_ref(Some(env_ref)),
            &[],
        )
        .await
    }

    async fn processes_under(&self, key: &crate::StartKey) -> Option<crate::ProcessId> {
        self.registry
            .get_process_by_start_key(key)
            .await
            .expect("read the key's record")
            .map(|record| record.id)
    }

    async fn end(&self, referrer: crate::ArtifactReferrer) {
        self.ports.end(referrer).await.expect("arm the end");
    }

    /// Run the relay until nothing is due: a guard whose authority has not
    /// ended its referrer is deferred and asked again on the next pass.
    async fn drain(&self) {
        for _ in 0..8 {
            self.clock.advance(1_000);
            let pass = relay_due(
                &self.relay,
                &self.clock,
                std::num::NonZeroUsize::new(256).expect("a nonzero page"),
            )
            .await
            .expect("relay a due pass");
            assert_eq!(pass.stalled, 0, "no cleanup stalls: {pass:?}");
            if pass.claimed == 0 {
                return;
            }
        }
    }
}

fn env_spec() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
    )
}

#[expect(clippy::expect_used, reason = "fixture builds valid claims")]
fn unguarded(referrer: crate::ArtifactReferrer) -> crate::ReferrerClaim {
    crate::ReferrerClaim::unguarded(referrer).expect("an unguarded claim")
}

fn pin() -> (crate::ArtifactReferrer, crate::ReferrerClaim) {
    let referrer = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    (referrer.clone(), unguarded(referrer))
}

fn execution(journal: &crate::EffectJournalIdentity) -> crate::ReferrerClaim {
    crate::ReferrerClaim::guarded(crate::ReferrerGuard::Journal(journal.clone()))
}

fn start_claim(
    key: &crate::StartKey,
    starter: &crate::EffectJournalIdentity,
) -> crate::ReferrerClaim {
    crate::ReferrerClaim::guarded(crate::ReferrerGuard::Start {
        start_key: key.clone(),
        starter: starter.clone(),
    })
}

fn assert_code(error: &crate::RuntimeEffectControllerError, code: crate::RuntimeErrorCode) {
    assert_eq!(error.code, code, "{error:?}");
}

/// The five referrer kinds a definition's reader can be, each ended its own
/// way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    HostPin,
    Execution,
    Frame,
    Start,
    ProcessRecord,
}

const HOLDERS: [Holder; 5] = [
    Holder::HostPin,
    Holder::Execution,
    Holder::Frame,
    Holder::Start,
    Holder::ProcessRecord,
];

/// ADR 0113 §3.6: a definition is not reclaimed while any referrer holds it.
///
/// For each referrer kind in turn, a definition is held by a host pin, an
/// execution, a frame, a pending start (staged under `Start(key)`, whose
/// starter's journal may still replay) and a registered process. Every other
/// holder ends and the relay drains: the descriptor and its module stay.
/// Then the last holder ends, and they go.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn definition_is_not_reclaimed_while_any_referrer_holds_it(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    for last in HOLDERS {
        let name = format!("{last:?}").to_lowercase();
        let (module_ref, draft) = world.draft(&format!("held-{name}"));
        let (host_pin, pin_claim) = pin();
        let definition = world
            .publish(&pin_claim, &module_ref, &draft)
            .await
            .expect("publish under the pin");
        let id = definition.id.clone();
        assert_eq!(id, draft.id());

        let exec_journal = world.journal(&format!("held-exec-{name}"));
        assert!(matches!(
            world.acquire(&execution(&exec_journal), &id).await,
            crate::DefinitionAcquisition::Held(_)
        ));
        let frame = crate::ArtifactReferrer::FrameEnvironment(crate::FrameEnvironmentId::new(
            crate::SessionId::from(format!("held-session-{name}-{}", world.tag)),
            crate::FrameNodeId::new("frame-1").expect("a frame node id"),
        ));
        assert!(matches!(
            world.acquire(&unguarded(frame.clone()), &id).await,
            crate::DefinitionAcquisition::Held(_)
        ));
        // A pending start: staged under its key, its starter still running.
        let pending_key = world.key(&format!("held-pending-{name}"));
        let pending_starter = world.journal(&format!("held-pending-starter-{name}"));
        assert!(matches!(
            world
                .acquire(&start_claim(&pending_key, &pending_starter), &id)
                .await,
            crate::DefinitionAcquisition::Held(_)
        ));
        // A registered start: its guard carries the closure onto its record.
        let started_key = world.key(&format!("held-started-{name}"));
        let started = world
            .start(
                &started_key,
                &id,
                &world.journal(&format!("held-starter-{name}")),
            )
            .await
            .expect("start by id");
        assert_eq!(
            started.record.identity.definition_id.as_ref(),
            Some(&id),
            "the record names the definition it was admitted from"
        );
        world.drain().await;
        assert!(world.held(&module_ref, &id).await);

        let end_holder = |holder: Holder| {
            let world = &world;
            let host_pin = host_pin.clone();
            let frame = frame.clone();
            let process = crate::ArtifactReferrer::ProcessRecord(started.record.id.clone());
            let exec_journal = exec_journal.clone();
            let pending_starter = pending_starter.clone();
            async move {
                match holder {
                    Holder::HostPin => world.end(host_pin).await,
                    Holder::Execution => world.settle(&exec_journal),
                    Holder::Frame => world.end(frame).await,
                    // The starter settled with no record under its key.
                    Holder::Start => world.settle(&pending_starter),
                    Holder::ProcessRecord => world.end(process).await,
                }
                world.drain().await;
            }
        };
        for holder in HOLDERS.into_iter().filter(|holder| *holder != last) {
            end_holder(holder).await;
            assert!(
                world.held(&module_ref, &id).await,
                "{last:?} still holds the definition after {holder:?} ended"
            );
        }
        assert!(
            world.processes_under(&pending_key).await.is_none(),
            "the pending start never registered"
        );
        end_holder(last).await;
        assert!(
            !world.held(&module_ref, &id).await,
            "the definition is reclaimed once {last:?}, its last referrer, ended"
        );
        // An id alone holds nothing.
        let missing = world
            .ports
            .acquire_definition(&world.engines, &pin().1, &id)
            .await
            .expect_err("nothing holds the definition");
        assert!(
            matches!(&missing, crate::PluginError::Runtime(error)
                if error.code == crate::RuntimeErrorCode::DefinitionMissing),
            "{missing:?}"
        );
    }
}

/// ADR 0113 §3.6: a definition is reclaimed after its last referrer, however
/// its referrers interleave.
///
/// Eight hosts each publish the same definition under their own pin (equal
/// content, one id), half of them start it by id and end the process, and
/// every one releases its pin, all concurrently with the relay. While a pin
/// is unreleased its definition is always readable. Once all have released
/// and the relay drains, the descriptor and the module are gone, no cleanup
/// stalled, a released pin stays fenced, and a fresh pin publishes the same
/// content again.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn definition_is_eventually_reclaimed_after_its_last_referrer(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = Arc::new(World::new(registry, ports));
    let (module_ref, draft) = world.draft("concurrent");
    let id = draft.id();
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let relay_loop = {
        let world = Arc::clone(&world);
        let running = Arc::clone(&running);
        tokio::spawn(async move {
            while running.load(std::sync::atomic::Ordering::SeqCst) {
                world.drain().await;
                tokio::task::yield_now().await;
            }
        })
    };
    let mut hosts = Vec::new();
    for host in 0..8_usize {
        let world = Arc::clone(&world);
        let module_ref = module_ref.clone();
        let draft = draft.clone();
        hosts.push(tokio::spawn(async move {
            let (pin, claim) = pin();
            let published = world
                .publish(&claim, &module_ref, &draft)
                .await
                .expect("equal content publishes to the one id");
            assert_eq!(published.id, draft.id());
            assert!(
                world.held(&module_ref, &published.id).await,
                "a definition is readable while its pin holds it"
            );
            if host % 2 == 0 {
                let key = world.key(&format!("concurrent-start-{host}"));
                let starter = world.journal(&format!("concurrent-starter-{host}"));
                let started = world
                    .start(&key, &published.id, &starter)
                    .await
                    .expect("start by id while pinned");
                world.settle(&starter);
                world
                    .end(crate::ArtifactReferrer::ProcessRecord(
                        started.record.id.clone(),
                    ))
                    .await;
            }
            assert!(
                world.held(&module_ref, &published.id).await,
                "a definition is readable until its pin is released"
            );
            world.end(pin.clone()).await;
            (pin, claim)
        }));
    }
    let mut released = Vec::new();
    for host in hosts {
        released.push(host.await.expect("a host joins"));
    }
    running.store(false, std::sync::atomic::Ordering::SeqCst);
    relay_loop.await.expect("the relay loop joins");
    world.drain().await;
    world.drain().await;
    assert!(
        !world.held(&module_ref, &id).await,
        "the definition is reclaimed after its last referrer"
    );
    assert_eq!(
        world
            .ports
            .cleanup()
            .count_stalled()
            .await
            .expect("count stalled cleanups"),
        0
    );
    let (released_pin, released_claim) = released.pop().expect("a released pin");
    let refusal = world
        .publish(&released_claim, &module_ref, &draft)
        .await
        .expect_err("a released pin stays fenced");
    assert_eq!(
        crate::artifact_referrer_ended(&refusal),
        Some(&released_pin),
        "{refusal:?}"
    );
    let (_, fresh) = pin();
    world
        .publish(&fresh, &module_ref, &draft)
        .await
        .expect("a fresh pin publishes the same content again");
    assert!(world.held(&module_ref, &id).await);
}

/// Crash table, "before create-attempt commit": no publication is visible,
/// and a retry may compile again.
///
/// A start of an id nothing published is refused `DefinitionMissing` and
/// leaves no process under its key. Once the retried create publishes, a
/// start of the id registers exactly one process.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_before_create_attempt_commit(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("before-create");
    let id = draft.id();
    assert!(!world.held(&module_ref, &id).await, "nothing was published");
    let refused_key = world.key("before-create-refused");
    let starter = world.journal("before-create-starter");
    let refused = world
        .start(&refused_key, &id, &starter)
        .await
        .expect_err("an unpublished definition does not start");
    assert_code(&refused, crate::RuntimeErrorCode::DefinitionMissing);
    assert_eq!(world.processes_under(&refused_key).await, None);

    // The retry compiles again and its realization publishes under its
    // execution.
    let create = world.journal("before-create-retry");
    world
        .publish(&execution(&create), &module_ref, &draft)
        .await
        .expect("the retried create publishes");
    let key = world.key("before-create-start");
    let started = world.start(&key, &id, &starter).await.expect("start by id");
    assert_eq!(
        started.disposition,
        crate::ProcessRegistrationOutcome::Created
    );
    assert_eq!(world.processes_under(&key).await, Some(started.record.id));
    assert_eq!(world.processes_under(&refused_key).await, None);
}

/// Crash table, "after attempt commit, before or during publication": the
/// journal retains the descriptor and the id, and realization publishes
/// again idempotently under its execution, guard included.
///
/// A redrive publishes the same bytes to the same id. The execution's guard
/// holds the definition while its journal may replay, however often the
/// relay runs, and a start of the id registers exactly one process.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_after_attempt_commit_before_publication(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("before-publication");
    let create = world.journal("before-publication-create");
    let first = world
        .publish(&execution(&create), &module_ref, &draft)
        .await
        .expect("the first realization publishes");
    // The deployment died during publication; the redrive publishes again.
    let redriven = world
        .publish(&execution(&create), &module_ref, &draft)
        .await
        .expect("the redrive publishes the same content");
    assert_eq!(first, redriven);
    world.drain().await;
    assert!(
        world.held(&module_ref, &first.id).await,
        "the execution's guard holds the definition while its journal may replay"
    );
    let key = world.key("before-publication-start");
    let starter = world.journal("before-publication-starter");
    let started = world
        .start(&key, &first.id, &starter)
        .await
        .expect("start by id");
    let redrive = world
        .start(&key, &first.id, &starter)
        .await
        .expect("a redriven start");
    assert_eq!(redrive.record.id, started.record.id);
    assert_eq!(
        redrive.disposition,
        crate::ProcessRegistrationOutcome::Existing
    );
    world.settle(&create);
    world.drain().await;
    assert!(
        world.held(&module_ref, &first.id).await,
        "the started process holds the definition after the create settles"
    );
}

/// Crash table, "after publication, before output or frame commit": the
/// recorded output replays, the frame acquires its edges with its state, and
/// the journal protects the gap.
///
/// Between the publication and the frame's acquisition the relay drains while
/// the create's journal may replay: the definition stays. The frame then
/// holds it past the create's settlement, and a start of it registers one
/// process.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_after_publication_before_frame_commit(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("before-frame");
    let create = world.journal("before-frame-create");
    let definition = world
        .publish(&execution(&create), &module_ref, &draft)
        .await
        .expect("realization publishes");
    // Crash: the frame commit is lost. The journal protects the gap.
    world.drain().await;
    assert!(world.held(&module_ref, &definition.id).await);
    // The replay reads the recorded id and commits the frame with its edges.
    let frame = crate::ArtifactReferrer::FrameEnvironment(crate::FrameEnvironmentId::new(
        crate::SessionId::from(format!("before-frame-session-{}", world.tag)),
        crate::FrameNodeId::new("frame-1").expect("a frame node id"),
    ));
    assert!(matches!(
        world
            .acquire(&unguarded(frame.clone()), &definition.id)
            .await,
        crate::DefinitionAcquisition::Held(_)
    ));
    world.settle(&create);
    world.drain().await;
    assert!(
        world.held(&module_ref, &definition.id).await,
        "the frame holds the definition once the create settles"
    );
    let key = world.key("before-frame-start");
    let starter = world.journal("before-frame-starter");
    let started = world
        .start(&key, &definition.id, &starter)
        .await
        .expect("start by id from the frame's value");
    assert_eq!(world.processes_under(&key).await, Some(started.record.id));
}

/// Crash table, "before start admission": the start acquires the closure
/// under `Start(key)`, validates, then admits; a failure creates no process.
///
/// A start that staged the closure and died before registering keeps it
/// while its starter may replay, even with the publishing pin released; its
/// redrive registers exactly one process, which then holds the definition. A
/// start whose starter settles without registering leaves no process, and
/// its staged closure goes with `Start(key)`.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_before_start_admission(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("before-admission");
    let (pin, pin_claim) = pin();
    let definition = world
        .publish(&pin_claim, &module_ref, &draft)
        .await
        .expect("publish under the pin");
    let id = definition.id;

    // Redriven: staged, crashed, redriven.
    let key = world.key("before-admission-redriven");
    let starter = world.journal("before-admission-starter");
    assert!(matches!(
        world.acquire(&start_claim(&key, &starter), &id).await,
        crate::DefinitionAcquisition::Held(_)
    ));
    // Abandoned: staged, crashed, and its starter settled with no redrive.
    let abandoned_key = world.key("before-admission-abandoned");
    let abandoned_starter = world.journal("before-admission-abandoned-starter");
    assert!(matches!(
        world
            .acquire(&start_claim(&abandoned_key, &abandoned_starter), &id)
            .await,
        crate::DefinitionAcquisition::Held(_)
    ));
    world.end(pin).await;
    world.drain().await;
    assert!(
        world.held(&module_ref, &id).await,
        "the staged starts hold the definition while their starters may replay"
    );
    let started = world.start(&key, &id, &starter).await.expect("the redrive");
    assert_eq!(
        started.disposition,
        crate::ProcessRegistrationOutcome::Created
    );
    world.settle(&abandoned_starter);
    world.drain().await;
    assert_eq!(world.processes_under(&abandoned_key).await, None);
    assert_eq!(
        world.processes_under(&key).await,
        Some(started.record.id.clone())
    );
    assert!(
        world.held(&module_ref, &id).await,
        "the registered process holds the definition"
    );
    world
        .end(crate::ArtifactReferrer::ProcessRecord(started.record.id))
        .await;
    world.drain().await;
    assert!(
        !world.held(&module_ref, &id).await,
        "the abandoned start's staging went with its key"
    );

    // A refusal creates no process: a definition nothing holds.
    let refused_key = world.key("before-admission-refused");
    let refused = world
        .start(
            &refused_key,
            &id,
            &world.journal("before-admission-refused"),
        )
        .await
        .expect_err("nothing holds the definition");
    assert_code(&refused, crate::RuntimeErrorCode::DefinitionMissing);
    assert_eq!(world.processes_under(&refused_key).await, None);
}

/// Crash table, "after registration, before result recording or delivery":
/// the retained start binding yields the same minted process id.
///
/// The start registered and its result was lost. A redrive, before and after
/// the relay settled the key, answers the same process as `Existing`, and the
/// key holds exactly that one process, which holds the definition.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_after_registration_before_result(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("after-registration");
    let (pin, pin_claim) = pin();
    let id = world
        .publish(&pin_claim, &module_ref, &draft)
        .await
        .expect("publish under the pin")
        .id;
    let key = world.key("after-registration");
    let starter = world.journal("after-registration-starter");
    let registered = world.start(&key, &id, &starter).await.expect("start by id");
    // The result was lost: redrive before the relay ran.
    let before_relay = world.start(&key, &id, &starter).await.expect("redrive");
    world.drain().await;
    // And after the relay settled the key.
    let after_relay = world.start(&key, &id, &starter).await.expect("redrive");
    for redrive in [&before_relay, &after_relay] {
        assert_eq!(redrive.record.id, registered.record.id);
        assert_eq!(
            redrive.disposition,
            crate::ProcessRegistrationOutcome::Existing
        );
    }
    assert_eq!(
        world.processes_under(&key).await,
        Some(registered.record.id.clone())
    );
    world.end(pin).await;
    world.settle(&starter);
    world.drain().await;
    assert!(
        world.held(&module_ref, &id).await,
        "the one process holds the definition"
    );
}

/// Crash table, "after recorded start, including after prune": the recorded
/// receipt replays before any artifact lookup, and no start is issued again.
///
/// The start's recorded result is what the journal keeps. After the process
/// is pruned and the definition reclaimed, the recorded result still decodes
/// to the same process, and even a start re-issued under the key cannot mint
/// a second process: nothing holds the definition any more.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn start_by_id_replays_exactly_at_after_recorded_start(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("after-recorded");
    let (pin, pin_claim) = pin();
    let id = world
        .publish(&pin_claim, &module_ref, &draft)
        .await
        .expect("publish under the pin")
        .id;
    let key = world.key("after-recorded");
    let starter = world.journal("after-recorded-starter");
    let started = world.start(&key, &id, &starter).await.expect("start by id");
    let recorded = serde_json::to_string(&started).expect("record the result");
    world.settle(&starter);
    world.drain().await;

    let terminal = world
        .registry
        .complete_process(
            &started.record.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::workflow_key(started.record.id.to_string()),
        )
        .await
        .expect("complete the process");
    world
        .registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the process");
    world
        .end(crate::ArtifactReferrer::ProcessRecord(
            started.record.id.clone(),
        ))
        .await;
    world.end(pin).await;
    world.drain().await;
    assert!(
        !world.held(&module_ref, &id).await,
        "the pruned process and the released pin were the last referrers"
    );

    let replayed: crate::RegisteredProcessStart =
        serde_json::from_str(&recorded).expect("the recorded result replays");
    assert_eq!(replayed, started);
    assert_eq!(world.processes_under(&key).await, None);
    let reissued = world
        .start(&key, &id, &world.journal("after-recorded-reissued"))
        .await
        .expect_err("a re-issued start finds nothing to start");
    assert_code(&reissued, crate::RuntimeErrorCode::DefinitionMissing);
    assert_eq!(
        world.processes_under(&key).await,
        None,
        "no second process under the key"
    );
}

/// ADR 0113 §3.6: publication verifies the canonical bytes even on an
/// existing id.
///
/// Equal content under another pin publishes to the same id and changes
/// nothing. Other bytes under that id are an immutable-content refusal and
/// leave the stored descriptor as it was. A draft whose manifest artifact is
/// not stored, or whose manifest disagrees with its engine, writes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn definition_publication_verifies_bytes_on_an_existing_id(
    registry: Arc<dyn crate::ProcessRegistry>,
    ports: crate::ArtifactReferrerPorts,
) {
    let world = World::new(registry, ports);
    let (module_ref, draft) = world.draft("immutable");
    let (_, first) = pin();
    let published = world
        .publish(&first, &module_ref, &draft)
        .await
        .expect("publish");
    let (_, second) = pin();
    assert_eq!(
        world
            .publish(&second, &module_ref, &draft)
            .await
            .expect("equal content publishes again"),
        published
    );
    let manifest = draft.artifacts().to_vec();
    let conflict = world
        .ports
        .definitions()
        .publish_process_definition(&second, &published.id, br#"{"conflict":true}"#, &manifest)
        .await
        .expect_err("other bytes under an existing id");
    assert!(
        matches!(&conflict, crate::ArtifactStoreError::Immutable { artifact_ref }
            if artifact_ref == published.id.as_str()),
        "{conflict:?}"
    );
    assert_eq!(
        world
            .ports
            .definitions()
            .get_process_definition(&published.id)
            .await
            .expect("read the descriptor"),
        Some(draft.to_store_bytes())
    );

    let (_, unpublished) = world.draft("unpublished-module");
    let missing = world
        .ports
        .publish_definition(&world.engines, &second, &unpublished)
        .await
        .expect_err("a manifest artifact nobody published");
    assert!(
        matches!(&missing, crate::PluginError::Runtime(error)
            if error.code == crate::RuntimeErrorCode::ArtifactMissing),
        "{missing:?}"
    );
    assert_eq!(
        world
            .ports
            .definitions()
            .get_process_definition(&unpublished.id())
            .await
            .expect("read the descriptor"),
        None
    );

    let disagreeing = crate::ProcessDefinitionDraft::new(
        DEFINITION_ENGINE,
        serde_json::json!({ "module": module_ref }),
        Vec::<crate::ArtifactName>::new(),
    )
    .expect("a well-formed draft");
    let refused = world
        .ports
        .publish_definition(&world.engines, &second, &disagreeing)
        .await
        .expect_err("a manifest that disagrees with its engine");
    assert!(
        matches!(&refused, crate::PluginError::Runtime(error)
            if error.code == crate::RuntimeErrorCode::DefinitionRefused),
        "{refused:?}"
    );
    assert_eq!(
        world
            .ports
            .definitions()
            .get_process_definition(&disagreeing.id())
            .await
            .expect("read the descriptor"),
        None
    );
}
