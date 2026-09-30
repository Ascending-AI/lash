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
use crate::crash_matrix::deployment::HostSite;
use crate::crash_matrix::invariants::{CustomCheck, Expected};
use crate::crash_matrix::world::{CoreBuild, CrashWorld};
use crate::crash_matrix::{CrashPoint, Seam};

const KIND: &str = "definition-carry-engine";

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
        if fences.contains(claim.referrer()) {
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
        if !held.contains(claim.referrer()) {
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
        PluginSpecFactory::new(KIND, Arc::new(|_| Ok(PluginSpec::new()))).build(context)
    }
}

fn core(store: Arc<EngineStore>, id: lash_core::ProcessDefinitionId) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let tag = id.to_tagged_json();
        let provider = lash_core::testing::TestProvider::builder().kind(KIND).complete(move |request: lash_core::llm::types::LlmRequest| {
            let tag = tag.clone();
            async move {
                let text = serde_json::to_string(&request).map_err(|error| lash_core::llm::transport::LlmTransportError::new(error.to_string()))?;
                let code = if text.contains("cleanup complete") { "finish('cleaned');".into() }
                    else if text.contains("input:uncarry;") { "await control.continue_as({task: 'cleanup complete'});".into() }
                    else if text.contains("activation complete") { "finish(definition_id);".into() }
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
        lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
            .plugin(Arc::new(Factory(store.clone())))
            .plugin(Arc::new(
                lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                    lash_core::lifetime::session_or_starter,
                ),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .recovery_lease(super::recovery_lease())
            .provider(provider)
            .model(super::process::model_spec()?)
            .build(owner)
            .map_err(|error| error.to_string())
    })
}

fn carried_then_reclaimed(
    store: Arc<EngineStore>,
    id: lash_core::ProcessDefinitionId,
    pin: lash_core::HostArtifactPin,
    session: lash_core::SessionId,
) -> CustomCheck {
    let carried = Arc::new(AtomicBool::new(false));
    Arc::new(move |world| {
        let (store, id, session, carried, pin) = (
            store.clone(),
            id.clone(),
            session.clone(),
            carried.clone(),
            pin.clone(),
        );
        Box::pin(async move {
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
                        .process_definitions()
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
                .process_definitions()
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
    let world = CrashWorld::new(seed, core(store.clone(), id.clone()), false).await?;
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
    Ok(Staged {
        world,
        origin_ms,
        notes: vec![format!("engine manifest frame cut={site:?}")],
        expected: Expected {
            custom: vec![(
                "definition_carry",
                carried_then_reclaimed(store, id, pin, session),
            )],
            ..Default::default()
        },
    })
}
