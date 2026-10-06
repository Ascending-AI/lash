//! Publication of the immutable definition returned by a committed create attempt.

use std::sync::Arc;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::store::{HistoryAnchor, HistoryBudget};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};

use super::{Staged, crash_and_restart, send, session_name};
use crate::crash_matrix::catalog_audit;
use crate::crash_matrix::invariants::{CustomCheck, Expected};
use crate::crash_matrix::world::{CoreBuild, CrashWorld};
use crate::crash_matrix::{CrashPoint, Seam};

/// The run a realized create journals its definition under.
const CREATE_REALIZATION_STEP: &str = "process-definition:lash:tool-intent:";

const CODE: &str = "const made = await processes.create({ source: 'const answer = async () => 31;', dialect: 'typescript' }); finish(made.id);";

fn core() -> CoreBuild {
    Arc::new(|backend, owner| {
        let provider = lash_core::testing::TestProvider::builder()
            .kind("definition-create-crash")
            .complete(|_request: LlmRequest| async {
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: format!("<typescript>\n{CODE}\n</typescript>"),
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            })
            .build()
            .into_handle();
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
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .recovery_lease(super::recovery_lease())
            .serve_test_llm_profile(provider, super::process::llm_profile_spec()?)
            .build(owner)
            .map_err(|error| error.to_string())
    })
}

fn published(session: lash_core::SessionId) -> CustomCheck {
    Arc::new(move |world| {
        let session = session.clone();
        Box::pin(async move {
            let page = match world
                .backend()
                .session_store_factory()
                .load_ancestors(
                    &session,
                    HistoryAnchor::Head,
                    HistoryBudget {
                        max_nodes: std::num::NonZeroU32::MIN.saturating_add(511),
                        max_bytes: std::num::NonZeroU64::MIN.saturating_add(32 * 1024 * 1024 - 1),
                    },
                )
                .await
            {
                Ok(page) => page,
                Err(error) => return vec![format!("create has no committed frame: {error}")],
            };
            // A committed answer is a finished cell's inline value, read
            // through the stamped RLM event decoder.
            let mut outputs = Vec::new();
            for node in page.nodes {
                let lash_core::SessionNodePayload::Event {
                    event: lash_core::SessionHistoryRecord::Protocol(event),
                } = node.record.payload
                else {
                    continue;
                };
                match lash_protocol_rlm::decode_rlm_protocol_event(&event) {
                    Ok(Some(lash_rlm_types::RlmProtocolEvent::RlmTrajectoryEntry(entry))) => {
                        if let lash_rlm_types::CellOutcome::Finished(
                            lash_core::OutputValue::Inline(output),
                        ) = entry.outcome
                        {
                            outputs.push(output);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return vec![format!("a committed RLM event is corrupt: {error}")];
                    }
                }
            }
            let [output] = outputs.as_slice() else {
                return vec![format!(
                    "create committed {} answers, expected one",
                    outputs.len()
                )];
            };
            let id = match lash_core::ProcessDefinitionId::from_tagged_json(output) {
                Ok(id) => id,
                Err(error) => return vec![format!("create exposed no definition ID: {error}")],
            };
            let backend = world.backend();
            let bytes = match backend.definition_store().get_process_definition(&id).await {
                Ok(Some(bytes)) => bytes,
                other => return vec![format!("committed definition is missing: {other:?}")],
            };
            let draft = match lash_core::ProcessDefinitionDraft::from_store_bytes(&id, &bytes) {
                Ok(draft) => draft,
                Err(error) => {
                    return vec![format!(
                        "definition identity differs from published descriptor: {error}"
                    )];
                }
            };
            let mut violations = Vec::new();
            for artifact in draft.artifacts() {
                match backend
                    .module_artifacts()
                    .get_module_artifact(&artifact.artifact_ref)
                    .await
                {
                    Ok(Some(_)) => {}
                    other => {
                        violations.push(format!("frame lost definition dependency: {other:?}"))
                    }
                }
            }
            violations
        })
    })
}

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, core()).await?;
    world.restart().await?;
    let cut = match point {
        CrashPoint::MidJournalStep => EngineCut::BeforeRunResultEnding {
            suffix: ":attempt:1".into(),
        },
        // The create is a protected intent: its realization journals the
        // definition step under the intent's key.
        CrashPoint::AfterStateCommit => EngineCut::BeforeRunStarting {
            prefix: CREATE_REALIZATION_STEP.into(),
        },
        CrashPoint::AfterDeliveryBeforeSettle => EngineCut::BeforeRunResultStarting {
            prefix: CREATE_REALIZATION_STEP.into(),
        },
        other => return Err(format!("create has no {other:?} cut")),
    };
    world.crash_on(CrashRule::new(cut).within_attempts(1));
    let session = session_name(Seam::DefinitionCreate, seed);
    send(&world, &session, "create").await?;
    let origin_ms = crash_and_restart(&world).await?;
    Ok(Staged {
        world,
        origin_ms,
        notes: vec![format!("create publication cut={point:?}")],
        expected: Expected {
            custom: vec![("definition_create", published(session))],
            audits: vec![("catalog_names", catalog_audit::no_catalog_name())],
            ..Default::default()
        },
    })
}
