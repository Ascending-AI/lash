//! Tool loss is a typed fact the host receives from the run that restored the
//! session's tool state, and `Require` is that run's explicit refusal with a
//! named contract (FIG-3367, FIG-5134, ADR 0119).

//!
//! Every law runs its turns on the core's node over SQLite memory stores
//! (FIG-5307).

use super::*;
use crate::TurnEvent;
use crate::support::TurnInput;
use lash_core::DeploymentStore;
use lash_sansio::SessionId;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts the lifecycle facts a refused run must not produce.
#[derive(Default)]
struct OpenLifecycleCounters {
    session_restored_events: AtomicUsize,
}

struct OpenLifecycleProbeFactory {
    counters: Arc<OpenLifecycleCounters>,
}

impl lash_core::facade_support::PluginFactory for OpenLifecycleProbeFactory {
    fn id(&self) -> &'static str {
        "fig3367-open-lifecycle-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(OpenLifecycleProbePlugin {
            counters: Arc::clone(&self.counters),
        }))
    }
}

impl lash_core::plugin::PluginDefinition for OpenLifecycleProbeFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("fig3367-open-lifecycle-probe")
    }
}

struct OpenLifecycleProbePlugin {
    counters: Arc<OpenLifecycleCounters>,
}

impl lash_core::facade_support::SessionPlugin for OpenLifecycleProbePlugin {
    fn id(&self) -> &'static str {
        "fig3367-open-lifecycle-probe"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let counters = Arc::clone(&self.counters);
        reg.session().on_event(
            crate::hook_key!("session-on-event-1"),
            Arc::new(move |event| {
                let counters = Arc::clone(&counters);
                Box::pin(async move {
                    if matches!(
                        event,
                        lash_core::facade_support::PluginLifecycleEvent::SessionRestored(_)
                    ) {
                        counters
                            .session_restored_events
                            .fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                })
            }),
        )?;
        Ok(())
    }
}

/// Persist a session whose checkpoint carries `tool:app_lookup`, on a core
/// over `backend` that has the tool's source, and hand back the catalog it
/// lives in.
async fn seed_session_with_a_persisted_tool(
    backend: &lash_core::Backend,
    session_id: &SessionId,
) -> Result<Arc<dyn DeploymentStore>> {
    let factory: Arc<dyn DeploymentStore> = backend.session_store_factory();
    let granting_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tools(Arc::new(AppTools))
        .build(crate::testing::runtime_lease_owner())?;
    let granted = granting_core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    granted
        .send(TurnInput::text("persist a checkpoint with tool state"))
        .output()
        .await?;
    drop(granted);
    granting_core.shutdown().await?;
    Ok(factory)
}

async fn durable_head_revision(
    factory: &dyn DeploymentStore,
    session_id: &SessionId,
) -> Result<u64> {
    Ok(
        lash_core::SessionCommitStore::load_session_head_meta(factory, session_id)
            .await?
            .expect("persisted state")
            .head_revision,
    )
}

/// The restore report of a run refused under `Require`: its sender reads
/// `SendOutcome::Refused` carrying the typed `ToolSourcesUnavailable` cause.
fn assert_tool_source_refusal(outcome: &crate::SendOutcome) {
    let crate::SendOutcome::Refused { refusal, .. } = outcome else {
        panic!("Require refuses the run as its terminal answer, got {outcome:?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::ToolSourcesUnavailable,
        "the refusal is typed: {refusal:?}"
    );
    let report = refusal
        .tool_sources_unavailable()
        .expect("the refusal carries the restore report");
    assert_eq!(
        report.lost_members,
        vec![lash_core::ToolId::from("tool:app_lookup")],
        "the refusal carries the typed lost-member ids"
    );
}

/// Tolerate is the default: the run that restores the session's tool state
/// goes on, and the host reads what it lost, by tool id and by class, on the
/// run's output and on the session's observation feed (FIG-5134).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_restoring_run_delivers_the_tool_restore_report_to_the_host() -> Result<()> {
    let session_id = SessionId::from("fig-3367-tolerate");
    let backend = sqlite_memory_store_backend().await;
    let _factory = seed_session_with_a_persisted_tool(&backend, &session_id).await?;

    let grantless_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let opened = grantless_core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let cursor = opened
        .observe()
        .snapshot()
        .await
        .expect("durable snapshot")
        .cursor;

    let output = opened
        .send(TurnInput::text("run without the tool's source"))
        .output()
        .await?;
    let report = output
        .tool_restore_report()
        .expect("the run that restored persisted tool state reports what it found");
    assert_eq!(
        report.lost_members,
        vec![lash_core::ToolId::from("tool:app_lookup")],
        "the host learns which persisted catalog member has no source"
    );
    assert!(report.parked_opt_outs.is_empty());
    assert!(report.superseded_identities.is_empty());

    let lash_core::facade_support::SessionResume::Replayed { events } =
        opened.observe().resume_from_cursor(&cursor).await?
    else {
        panic!("the run's observation cursor stays replayable");
    };
    let observed = events
        .iter()
        .find_map(|event| match &event.payload {
            lash_core::SessionObservationEventPayload::TurnActivity(activity) => {
                match &activity.event {
                    TurnEvent::ToolRestoreReported { report } => Some(report),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("the session feed carries the typed restore report");
    assert_eq!(observed, report, "the feed and the output carry one report");
    drop(opened);
    Ok(())
}

/// Require refuses the run whose transition would lose a member, typed, and
/// keeps its named promises: no `SessionRestored`, no durable write, and the
/// next open still runs. The open itself builds no capabilities, so it never
/// refuses (FIG-5134).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_refuses_the_run_and_keeps_its_named_promises() -> Result<()> {
    let session_id = SessionId::from("fig-3367-require");
    let backend = sqlite_memory_store_backend().await;
    let factory = seed_session_with_a_persisted_tool(&backend, &session_id).await?;
    let head_before = durable_head_revision(factory.as_ref(), &session_id).await?;

    let counters = Arc::new(OpenLifecycleCounters::default());
    let strict_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(OpenLifecycleProbeFactory {
            counters: Arc::clone(&counters),
        }))
        .tool_source_policy(lash_core::ToolSourcePolicy::Require)
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .build(crate::testing::runtime_lease_owner())?;

    let opened = strict_core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let outcome = opened
        .send(TurnInput::text("run without the tool's source"))
        .await?
        .outcome()
        .await?;
    assert_tool_source_refusal(&outcome);
    assert_eq!(
        counters.session_restored_events.load(Ordering::SeqCst),
        0,
        "a refused run emits no SessionRestored"
    );
    assert_eq!(
        durable_head_revision(factory.as_ref(), &session_id).await?,
        head_before,
        "a refused run commits no config or state"
    );
    drop(opened);
    // The deployment drops the strict core: the next core's node runs the
    // next run.
    strict_core.shutdown().await?;

    // The refused run held nothing: a following open runs. Tolerate here,
    // because the point is what the refusal left, not the policy.
    let tolerant_core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let reopened = tolerant_core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    reopened
        .send(TurnInput::text("run after the refusal"))
        .output()
        .await?;
    drop(reopened);
    Ok(())
}

/// The node rebuilds a session's runtime for each run it claims. Under
/// Require, a rebuild that lost a catalog member refuses the run, typed and
/// naming the lost member.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_refuses_a_cold_rebuild_that_lost_a_tool_source() -> Result<()> {
    let session_id = SessionId::from("fig-3367-queued");
    let backend = sqlite_memory_store_backend().await;
    seed_session_with_a_persisted_tool(&backend, &session_id).await?;
    // The strict build is the deployment restarted over the same stores: its
    // node rebuilds the session's runtime for the run it claims.
    let strict_core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .tool_source_policy(lash_core::ToolSourcePolicy::Require)
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .build(crate::testing::runtime_lease_owner())?;

    let handle = strict_core
        .session(session_id.clone())
        .durable()
        .await?
        .send(TurnInput::text("run the strict rebuild"));
    let error = match handle.output().await {
        Ok(report) => panic!("the rebuild must fail under Require, got {report:?}"),
        Err(error) => error,
    };
    // The run's refusal reaches the sender typed and naming the lost tool
    // (FIG-5134).
    let EmbedError::Runtime(refusal) = &error else {
        panic!("the refusal is the run's typed runtime error, got {error:?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::ToolSourcesUnavailable
    );
    assert!(
        error
            .to_string()
            .contains("requires every persisted tool source"),
        "the run refuses the rebuild on the lost tool source: {error}"
    );
    assert!(
        error.to_string().contains("tool:app_lookup"),
        "the refusal names the lost tool: {error}"
    );
    Ok(())
}
