//! A definition closure split between SQL and an engine-owned artifact store.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{
    PluginFactory, PluginSessionContext, PluginSpec, PluginSpecFactory, SessionPlugin,
};
use lash_core::sync::MutexExt;
use lash_core::{ArtifactReferrer, ArtifactStoreError, ArtifactStoreId, ReferrerClaim};

use super::{Staged, crash_and_restart, send, session_name};
use crate::crash_matrix::catalog_audit;
use crate::crash_matrix::deployment::HostSite;
use crate::crash_matrix::invariants::{CustomCheck, Expected};
use crate::crash_matrix::world::{CoreBuild, CrashWorld};
use crate::crash_matrix::{CrashPoint, Seam};

const KIND: &str = "definition-carry-engine";

/// Keeps the carry turn open across a recovery tick after the crash
/// (FIG-4359): the successor frame's first model call waits here until the
/// case's check has seen a tick run since the restart. A live server replays
/// the crashed turn on wall time, so its recovery pass can meet the turn
/// still open — the predecessor's end gated on the turn's journal, the
/// turn's execution referrer waiting for it — and defer both; the double
/// settles every invocation before each tick and never would. The hold gives
/// both engines that interleaving.
struct OpenTurnHold {
    released: tokio::sync::watch::Sender<bool>,
}

impl OpenTurnHold {
    fn new() -> Self {
        Self {
            released: tokio::sync::watch::Sender::new(false),
        }
    }

    async fn wait(&self) {
        let mut released = self.released.subscribe();
        // The sender lives as long as the hold, so the wait ends only on
        // the release.
        let _ = released.wait_for(|released| *released).await;
    }

    fn release(&self) {
        self.released.send_replace(true);
    }
}

#[derive(Default)]
struct EngineStore {
    edges: Mutex<BTreeMap<String, Vec<ArtifactReferrer>>>,
    fences: Mutex<Vec<ArtifactReferrer>>,
}

impl EngineStore {
    fn frames(&self) -> Vec<ArtifactReferrer> {
        let mut frames: Vec<_> = self
            .edges
            .lock_recover()
            .values()
            .flatten()
            .filter(|edge| matches!(edge, ArtifactReferrer::FrameEnvironment(_)))
            .cloned()
            .collect();
        frames.sort_by_key(ArtifactReferrer::canonical_id);
        frames.dedup();
        frames
    }

    fn complete(&self, frame: &ArtifactReferrer) -> bool {
        let edges = self.edges.lock_recover();
        ["first", "second"]
            .iter()
            .all(|name| edges.get(*name).is_some_and(|held| held.contains(frame)))
    }
}

struct Engine(Arc<EngineStore>);

#[async_trait::async_trait]
impl lash_core::ProcessEngine for Engine {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        unreachable!("the carry fixture only retains definitions")
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(["first", "second"]
            .into_iter()
            .map(|artifact_ref| lash_core::ArtifactName {
                store: ArtifactStoreId::Engine(KIND.into()),
                artifact_ref: artifact_ref.into(),
            })
            .collect())
    }

    async fn acquire_engine_artifact(
        &self,
        claim: &ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), lash_core::PluginError> {
        let fences = self.0.fences.lock_recover();
        if fences.contains(&claim.referrer()) {
            return Err(ArtifactStoreError::ReferrerEnded {
                referrer: claim.referrer().clone(),
            }
            .into());
        }
        let mut edges = self.0.edges.lock_recover();
        let held =
            edges
                .get_mut(artifact_ref)
                .ok_or_else(|| ArtifactStoreError::ArtifactMissing {
                    artifact_ref: artifact_ref.into(),
                })?;
        if !held.contains(&claim.referrer()) {
            held.push(claim.referrer().clone());
        }
        Ok(())
    }

    async fn end_artifact_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        let mut fences = self.0.fences.lock_recover();
        let mut edges = self.0.edges.lock_recover();
        for carry in &cleanup.carries {
            let held = edges.get_mut(&carry.artifact.artifact_ref).ok_or_else(|| {
                ArtifactStoreError::CarryArtifactMissing {
                    artifact_ref: carry.artifact.artifact_ref.clone(),
                    to: carry.to.clone(),
                }
            })?;
            if !fences.contains(&carry.to) && !held.contains(&carry.to) {
                held.push(carry.to.clone());
            }
        }
        if !fences.contains(&cleanup.referrer) {
            fences.push(cleanup.referrer.clone());
        }
        for held in edges.values_mut() {
            held.retain(|referrer| referrer != &cleanup.referrer);
        }
        edges.retain(|_, held| !held.is_empty());
        Ok(())
    }
}

struct Factory(Arc<EngineStore>);

impl PluginFactory for Factory {
    fn id(&self) -> &'static str {
        KIND
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::new(Engine(self.0.clone())),
        )])
    }

    fn build(
        &self,
        context: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, lash_core::PluginError> {
        PluginSpecFactory::new(
            lash_core::plugin::PluginDeclaration::initial(KIND),
            Arc::new(|_| Ok(PluginSpec::new())),
        )
        .build(context)
    }
}

fn core(
    store: Arc<EngineStore>,
    id: lash_core::ProcessDefinitionId,
    hold: Arc<OpenTurnHold>,
) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let tag = id.to_tagged_json();
        let hold = hold.clone();
        let provider = lash_core::testing::TestProvider::builder().kind(KIND).complete(move |request: lash_core::llm::types::LlmRequest| {
            let (tag, hold) = (tag.clone(), hold.clone());
            async move {
                let text = serde_json::to_string(&request).map_err(|error| lash_core::llm::transport::LlmTransportError::new(error.to_string()))?;
                let code = if text.contains("cleanup complete") { "finish('cleaned');".into() }
                    else if text.contains("input:uncarry;") { "await control.continue_as({task: 'cleanup complete'});".into() }
                    else if text.contains("activation complete") { hold.wait().await; "finish(definition_id);".into() }
                    else { format!("const kept = await processes.get({{definition_id: {tag}}}); await control.continue_as({{task: 'activation complete', seed: {{definition_id: kept.id}}}});") };
                Ok::<_, lash_core::llm::transport::LlmTransportError>(lash_core::llm::types::LlmResponse {parts: vec![lash_core::llm::types::LlmOutputPart::Text {text: format!("<typescript>\n{code}\n</typescript>"), response_meta: None}], ..Default::default()})
            }
        }).build().into_handle();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
            &backend,
        );
        lash::LashCore::rlm_builder(backend, factory)
            .plugin(Arc::new(Factory(store.clone())))
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .recovery_lease(super::recovery_lease())
            .serve_test_model(provider, super::process::model_spec()?)
            .build(owner)
            .map_err(|error| error.to_string())
    })
}

/// Drive only artifact cleanup past its guarded deferral once this root's
/// journal settles. The recovery interval retains its ordinary clock and
/// lapsed-claim bound; storage reclamation gets its own deterministic pass.
async fn deferred_cleanup(
    world: &CrashWorld,
    session: &lash_core::SessionId,
    root: &str,
) -> Result<(), String> {
    use lash_core::runtime::artifact_cleanup::{
        ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
    };
    use lash_core::runtime::drive::relay::{RelayPolicy, relay_due};
    let backend = world.backend();
    let journal = lash_core::ExecutionScope::turn(session.clone(), root)
        .journal_identity()
        .map_err(|error| error.to_string())?;
    if backend
        .effect_host()
        .journal_replay(&journal)
        .await
        .map_err(|error| error.to_string())?
        != lash_core::JournalReplay::Settled
    {
        return Ok(());
    }
    let administration = world.core()?.session_administration().await;
    let relay = ArtifactCleanupRelay::new(ArtifactCleanupPorts {
        ledger: backend.artifact_cleanup(),
        authorities: Arc::new(StoreSetAuthorities {
            effect_host: backend.effect_host(),
            sessions: backend.session_store_factory(),
            processes: backend.process_registry(),
            triggers: backend.trigger_store(),
        }),
        process_env: backend.process_env_store(),
        modules: backend.module_artifacts(),
        definitions: backend.definition_store(),
        engines: administration.process_engines().clone(),
        attachments: backend.attachment_referrers(),
        clock: backend.clock(),
    });
    let clock = lash_core::testing::TestClock::new(
        world
            .now_ms()
            .saturating_add(RelayPolicy::default().max_backoff_ms),
    );
    relay_due(
        &relay,
        &clock,
        std::num::NonZeroUsize::MIN.saturating_add(63),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// The successor keeps its complete share until the predecessor's end is
/// delivered, then everything is reclaimed after an uncarried switch. The
/// carry turn is held open until the first check after a recovery tick
/// since the restart, `ticks_at_restart` of them having run before it.
fn carried_then_reclaimed(
    store: Arc<EngineStore>,
    id: lash_core::ProcessDefinitionId,
    pin: lash_core::HostArtifactPin,
    session: lash_core::SessionId,
    hold: Arc<OpenTurnHold>,
    ticks_at_restart: usize,
) -> CustomCheck {
    let carried = Arc::new(AtomicBool::new(false));
    Arc::new(move |world| {
        let (store, id, session, carried, pin, hold) = (
            store.clone(),
            id.clone(),
            session.clone(),
            carried.clone(),
            pin.clone(),
            hold.clone(),
        );
        Box::pin(async move {
            if world.ticks_run() > ticks_at_restart {
                hold.release();
            }
            let root = if carried.load(Ordering::SeqCst) {
                "uncarry"
            } else {
                "carry"
            };
            if let Err(error) = deferred_cleanup(world, &session, root).await {
                return vec![error];
            }
            if !carried.load(Ordering::SeqCst) {
                let frames = store.frames();
                let [frame] = frames.as_slice() else {
                    return vec![format!("waiting for exactly one carried frame: {frames:?}")];
                };
                if !store.complete(frame) {
                    return vec!["successor lacks an engine dependency".into()];
                }
                if !matches!(
                    world
                        .backend()
                        .definition_store()
                        .get_process_definition(&id)
                        .await,
                    Ok(Some(_))
                ) {
                    return vec!["successor lacks the SQL descriptor".into()];
                }
                let core = match world.core() {
                    Ok(core) => core,
                    Err(error) => return vec![error],
                };
                if let Err(error) = core.host_artifacts().release(pin).await {
                    return vec![error.to_string()];
                };
                carried.store(true, Ordering::SeqCst);
                if let Err(error) = send(world, &session, "uncarry").await {
                    return vec![error];
                }
                return vec!["waiting for Ended with no carry".into()];
            }
            if !store.edges.lock_recover().is_empty() {
                return vec!["engine artifacts retained after the uncarried switch".into()];
            }
            match world
                .backend()
                .definition_store()
                .get_process_definition(&id)
                .await
            {
                Ok(None) => Vec::new(),
                other => vec![format!(
                    "SQL descriptor retained after the uncarried switch: {other:?}"
                )],
            }
        })
    })
}

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let store = Arc::new(EngineStore::default());
    store.edges.lock_recover().extend(
        ["first", "second"]
            .into_iter()
            .map(|name| (name.into(), Vec::new())),
    );
    let draft = lash_core::ProcessDefinitionDraft::new(
        KIND,
        serde_json::Value::Null,
        ["first", "second"]
            .into_iter()
            .map(|artifact_ref| lash_core::ArtifactName {
                store: ArtifactStoreId::Engine(KIND.into()),
                artifact_ref: artifact_ref.into(),
            }),
    )
    .map_err(|error| error.to_string())?;
    let id = draft.id();
    let hold = Arc::new(OpenTurnHold::new());
    let world = CrashWorld::new(seed, core(store.clone(), id.clone(), hold.clone()), false).await?;
    world.restart().await?;
    let pin = lash_core::HostArtifactPin::mint();
    world
        .core()?
        .host_artifacts()
        .publish_definition(&pin, &draft)
        .await
        .map_err(|error| error.to_string())?;
    let site = match point {
        CrashPoint::MidJournalStep => HostSite::FrameCommitBefore,
        CrashPoint::AfterStateCommit => HostSite::FrameCommitAfter,
        other => return Err(format!("frame carry has no {other:?} cut")),
    };
    world.faults().crash_once(site);
    let session = session_name(Seam::DefinitionCarry, seed);
    send(&world, &session, "carry").await?;
    let tripped = world.trip().wait(super::TRIP_WAIT).await;
    if tripped.is_some() {
        let frames = store.frames();
        if frames.len() != 2 || !frames.iter().all(|frame| store.complete(frame)) {
            world.finish().await;
            return Err(format!(
                "prepare did not retain both predecessor and successor: {frames:?}"
            ));
        }
    }
    let origin_ms = crash_and_restart(&world).await?;
    let ticks_at_restart = world.ticks_run();
    Ok(Staged {
        world,
        origin_ms,
        notes: vec![format!("engine manifest frame cut={site:?}")],
        expected: Expected {
            custom: vec![(
                "definition_carry",
                carried_then_reclaimed(store, id, pin, session, hold, ticks_at_restart),
            )],
            audits: vec![("catalog_names", catalog_audit::no_catalog_name())],
            ..Default::default()
        },
    })
}
