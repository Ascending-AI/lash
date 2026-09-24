//! Tool loss at open is a typed fact the host receives, and `Require` is an
//! explicit refusal with a named contract (FIG-3367, ADR 0097).

use super::*;
use lash_sansio::SessionId;

/// Counts the lifecycle facts a refused open must not produce.
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
        reg.session().on_event(Arc::new(move |event| {
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
        }));
        Ok(())
    }
}

/// Persist a session whose checkpoint carries `tool:app_lookup`, on a core that
/// has the tool's source, and hand back the catalog it lives in.
async fn seed_session_with_a_persisted_tool(
    session_id: &SessionId,
) -> Result<(
    Arc<lash_sqlite_store::SqliteBackend>,
    Arc<dyn SessionStoreFactory>,
)> {
    let backend = memory_backend().await;
    let factory: Arc<dyn SessionStoreFactory> = backend.session_store_factory();
    let granting_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tools(Arc::new(AppTools))
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let granted = granting_core.session(session_id.clone()).open().await?;
    granted
        .turn(TurnInput::text("persist a checkpoint with tool state"))
        .run()
        .await?;
    Box::pin(granted.close()).await?;
    Ok((backend, factory))
}

async fn durable_head_revision(
    factory: &dyn SessionStoreFactory,
    session_id: &SessionId,
) -> Result<u64> {
    let store = factory
        .open_existing_store_by_id(session_id)
        .await
        .expect("open the persisted store")
        .expect("the session exists");
    Ok(
        lash_core::store::load_persisted_session_state(store.as_ref())
            .await?
            .expect("persisted state")
            .head_revision,
    )
}

/// Tolerate is the default: the session opens and the host can read what it
/// lost, by tool id and by class.
#[tokio::test]
async fn open_delivers_the_tool_restore_report_to_the_host() -> Result<()> {
    let session_id = SessionId::from("fig-3367-tolerate");
    let (backend, _) = seed_session_with_a_persisted_tool(&session_id).await?;

    let grantless_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let opened = grantless_core.session(session_id.clone()).open().await?;

    let report = opened
        .tool_restore_report()
        .await
        .expect("an open that installed persisted tool state reports what it found");
    assert_eq!(
        report.lost_members,
        vec![lash_core::ToolId::from("tool:app_lookup")],
        "the host learns which persisted catalog member has no source"
    );
    assert!(report.parked_opt_outs.is_empty());
    assert!(report.superseded_identities.is_empty());
    Box::pin(opened.close()).await?;
    Ok(())
}

/// Require refuses, with the report on the typed error, and keeps its named
/// promises: no `SessionRestored`, no durable write, lease released.
#[tokio::test]
async fn require_refuses_the_open_and_keeps_its_named_promises() -> Result<()> {
    let session_id = SessionId::from("fig-3367-require");
    let (backend, factory) = seed_session_with_a_persisted_tool(&session_id).await?;
    let head_before = durable_head_revision(factory.as_ref(), &session_id).await?;

    let counters = Arc::new(OpenLifecycleCounters::default());
    let strict_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(OpenLifecycleProbeFactory {
        counters: Arc::clone(&counters),
    }))
    .tool_source_policy(lash_core::ToolSourcePolicy::Require)
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    let refusal = match strict_core.session(session_id.clone()).open().await {
        Ok(_) => panic!("Require must refuse an open whose persisted member has no source"),
        Err(error) => error,
    };
    match &refusal {
        EmbedError::Session(lash_core::SessionError::ToolSourcesUnavailable {
            session_id: refused,
            report,
        }) => {
            assert_eq!(refused, &session_id);
            assert_eq!(
                report.lost_members,
                vec![lash_core::ToolId::from("tool:app_lookup")],
                "the refusal carries the typed lost-member ids"
            );
        }
        other => panic!("expected a typed tool-source refusal, got {other:?}"),
    }
    assert_eq!(
        counters.session_restored_events.load(Ordering::SeqCst),
        0,
        "a refused open emits no SessionRestored"
    );
    assert_eq!(
        durable_head_revision(factory.as_ref(), &session_id).await?,
        head_before,
        "a refused open commits no config or state"
    );

    // The lease the refused open claimed was released: a following open takes
    // it. Tolerate here, because the point is the lease, not the policy.
    let tolerant_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;
    let reopened = tolerant_core.session(session_id.clone()).open().await?;
    Box::pin(reopened.close()).await?;
    Ok(())
}

/// The per-open override states the policy for one session on a core that
/// tolerates loss everywhere else.
#[tokio::test]
async fn a_per_open_override_states_the_policy_for_one_session() -> Result<()> {
    let session_id = SessionId::from("fig-3367-per-open");
    let (backend, _) = seed_session_with_a_persisted_tool(&session_id).await?;

    let tolerant_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    let refusal = match tolerant_core
        .session(session_id.clone())
        .tool_source_policy(lash_core::ToolSourcePolicy::Require)
        .open()
        .await
    {
        Ok(_) => panic!("the per-open override must refuse this open"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &refusal,
            EmbedError::Session(lash_core::SessionError::ToolSourcesUnavailable { .. })
        ),
        "expected a typed tool-source refusal, got {refusal:?}"
    );

    // The same core, without the override, still opens.
    let opened = tolerant_core.session(session_id.clone()).open().await?;
    assert!(
        opened
            .tool_restore_report()
            .await
            .is_some_and(|report| report.has_lost_members()),
        "the core's own policy is unchanged by one session's override"
    );
    Box::pin(opened.close()).await?;
    Ok(())
}

/// The queued-work driver rebuilds a session runtime per batch. Under Require,
/// a rebuild that lost a catalog member is a **terminal** failure, not a
/// transient one the driver would retry forever.
#[tokio::test]
async fn require_makes_a_queued_work_rebuild_a_terminal_failure() -> Result<()> {
    use crate::core::queued_work::native_queued_work_handle_for_tests;
    use crate::runtime::{QueuedWorkRunErrorClass, QueuedWorkRunRequest};
    use lash_core::facade_support::QueuedWorkRunHandle as _;

    let session_id = SessionId::from("fig-3367-queued");
    let (backend, factory) = seed_session_with_a_persisted_tool(&session_id).await?;

    let strict_core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .tool_source_policy(lash_core::ToolSourcePolicy::Require)
    .without_queued_work()
    .build(crate::testing::runtime_lease_owner())?;

    // The driver the core would run for this session.
    let handle = native_queued_work_handle_for_tests(&strict_core, Arc::clone(&factory));

    let error = match handle
        .run_queued_work(QueuedWorkRunRequest {
            session_id: Some(session_id.clone()),
            reason: "fig-3367-queued-rebuild".to_string(),
            trace_idle: false,
        })
        .await
    {
        Ok(progress) => panic!("the rebuild must fail under Require, got {progress:?}"),
        Err(error) => error,
    };
    assert_eq!(
        error.class,
        QueuedWorkRunErrorClass::Terminal,
        "a lost tool source is not something a retry fixes: {error}"
    );
    assert!(
        error.to_string().contains("tool:app_lookup"),
        "the terminal failure names the lost tool: {error}"
    );
    Ok(())
}
