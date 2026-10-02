//! A redelivered trigger emission never writes a reclaimed occurrence or
//! delivery back (FIG-4513).
//!
//! The submission ledger answers a redelivery whose first delivery retained
//! its outcome. A first delivery that stopped between its emission and that
//! outcome leaves a row with none, so the redelivery, on a fresh invocation
//! with an empty effect journal, emits again. Once the delivery's process
//! was pruned and retention reclaimed the occurrence and the delivery, the
//! occurrence's idempotency key finds no row. The trigger store's tombstone
//! answers instead: the ingest is refused as reclaimed, and the ingress
//! reports the typed refusal and starts nothing.

use super::*;
use lash_core::testing::{EffectLayer, LayeredEffectHost};

fn ingress_of(core: &LashCore) -> Result<crate::tools::ToolIntentIngress> {
    core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))
}

/// Stops the delivery once its emission's bind has committed, as a crash
/// between the realization and the retained outcome would.
#[derive(Default)]
struct StopAfterBind {
    bound: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl EffectLayer for StopAfterBind {
    async fn execute_effect(
        &self,
        inner: &dyn lash_core::RuntimeEffectController,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        let bind = matches!(
            envelope.command,
            lash_core::RuntimeEffectCommand::AdmitTriggerDelivery { .. }
        );
        let outcome = inner.execute_effect(envelope, local_executor).await?;
        if bind {
            self.bound.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(outcome)
    }
}

/// What the stores hold that an emission could have written.
#[derive(Debug, PartialEq)]
struct Held {
    occurrences: Vec<lash_core::TriggerOccurrenceRecord>,
    deliveries: Vec<lash_core::TriggerDeliveryReservation>,
    processes: usize,
}

async fn held(
    store: &Arc<dyn lash_core::TriggerStore>,
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<Held> {
    Ok(Held {
        occurrences: store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?,
        deliveries: store.list_deliveries().await?,
        processes: registered_process_count(registry).await?,
    })
}

/// Which host with no journal to answer a redelivery the law runs on.
#[derive(Clone, Copy, Debug)]
enum Host {
    /// The ingress's own case: each invocation journals its effects, and a
    /// redelivery arrives on a new invocation whose journal is empty.
    FreshJournal,
    /// A host that journals nothing: every step's body runs in place on
    /// every delivery.
    NoJournal,
}

impl Host {
    /// The effect host of one invocation over `backend`.
    fn invocation(self, backend: &lash_core::Backend) -> Arc<dyn lash_core::EffectHost> {
        match self {
            Self::FreshJournal => Arc::new(KeyJournalController::default()),
            Self::NoJournal => backend.effect_host(),
        }
    }
}

async fn a_redelivered_emission_writes_no_reclaimed_row_back(
    host: Host,
    backend: lash_core::Backend,
) -> Result<()> {
    let sessions = backend.session_store_factory();
    let layer = Arc::new(StopAfterBind::default());
    let redelivery_host = host.invocation(&backend);
    let (core, store, _subscription, registry) = ingress_core_with_trigger_store(
        backend.clone(),
        Arc::new(LayeredEffectHost::new(
            host.invocation(&backend),
            Arc::clone(&layer) as Arc<dyn EffectLayer>,
        )),
    )
    .await?;
    let key = ingress_of(&core)?
        .key("reclaimed-emission", 0)
        .expect("a host submission handle");

    // The first delivery emits, starts and binds its delivery, and stops
    // before its outcome is retained.
    let first = {
        let ingress = ingress_of(&core)?;
        let key = key.clone();
        tokio::spawn(async move {
            ingress
                .submit(key, trigger_intent(&SessionId::from(SESSION)))
                .await
        })
    };
    layer.bound.notified().await;
    first.abort();
    assert!(
        first
            .await
            .expect_err("the first delivery stops")
            .is_cancelled()
    );
    let emitted = held(&store, &registry).await?;
    assert_eq!(emitted.occurrences.len(), 1, "the first delivery emitted");
    assert_eq!(emitted.deliveries.len(), 1, "and reserved its delivery");
    let process_id = emitted.deliveries[0]
        .process_id
        .clone()
        .expect("the first delivery bound its process");

    // The delivery's process ends and is pruned; retention then reclaims the
    // delivery and the occurrence nothing references.
    let record = registry
        .get_process(&process_id)
        .await?
        .expect("the delivery's process is retained");
    let ended = match registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({"done": true}),
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string()),
        )
        .await
    {
        Ok(ended) => ended.updated_at_ms,
        Err(lash_core::PluginError::ProcessAlreadyTerminal { .. }) => record.updated_at_ms,
        Err(error) => panic!("end the delivery's process: {error:?}"),
    };
    let pruned = registry
        .prune_terminal_processes(
            ended.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await?;
    assert!(pruned.pruned_processes >= 1, "the process is pruned");
    lash_core::facade_support::reconcile_pruned_trigger_deliveries(
        registry.as_ref(),
        store.as_ref(),
        Some(sessions.as_ref()),
    )
    .await?;
    let reclaimed = held(&store, &registry).await?;
    assert_eq!(
        (reclaimed.occurrences.len(), reclaimed.deliveries.len()),
        (0, 0),
        "retention reclaimed the occurrence and its delivery"
    );

    // The redelivery arrives on a new invocation with nothing journaled.
    let redelivery = explicit_ephemeral_facets(LashCore::standard_builder(ingress_backend(
        backend,
        Some(redelivery_host),
        None,
    )))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = redelivery.session(SESSION).created().await.open().await?;
    for attempt in ["the redelivery", "a second redelivery"] {
        let outcome = ingress_of(&redelivery)?
            .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
            .await;
        assert_eq!(
            outcome,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::TriggerOccurrenceReclaimed,
            },
            "{attempt} is refused as reclaimed"
        );
        assert_eq!(
            held(&store, &registry).await?,
            reclaimed,
            "{attempt} wrote a row or started a process"
        );
    }
    Ok(())
}

async fn sqlite_file_backend() -> (tempfile::TempDir, lash_core::Backend) {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(directory.path())
            .await
            .expect("open SQLite store set"),
    );
    (directory, lash_conformance::recording_backend_over(stores))
}

/// A PostgreSQL backend on the required service URL.
async fn postgres_backend() -> Result<
    Option<(
        lash_postgres_store::testing::IsolatedDatabase,
        tempfile::TempDir,
        lash_core::Backend,
    )>,
> {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url()).await?;
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    Ok(Some((
        database,
        attachments,
        lash_conformance::recording_backend_over(stores),
    )))
}

macro_rules! reclaimed_emission_laws {
    ($($host:ident => $memory:ident, $file:ident, $postgres:ident;)*) => {$(
        #[tokio::test]
        async fn $memory() -> Result<()> {
            Box::pin(a_redelivered_emission_writes_no_reclaimed_row_back(
                Host::$host,
                sqlite_memory_store_backend().await,
            ))
            .await
        }

        #[tokio::test]
        async fn $file() -> Result<()> {
            let (_directory, backend) = sqlite_file_backend().await;
            Box::pin(a_redelivered_emission_writes_no_reclaimed_row_back(
                Host::$host,
                backend,
            ))
            .await
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
        async fn $postgres() -> Result<()> {
            let Some((_database, _attachments, backend)) = postgres_backend().await? else {
                return Ok(());
            };
            Box::pin(a_redelivered_emission_writes_no_reclaimed_row_back(
                Host::$host,
                backend,
            ))
            .await
        }
    )*};
}

reclaimed_emission_laws! {
    FreshJournal =>
        a_fresh_journal_redelivery_writes_no_reclaimed_row_back_on_sqlite_memory,
        a_fresh_journal_redelivery_writes_no_reclaimed_row_back_in_sqlite,
        a_fresh_journal_redelivery_writes_no_reclaimed_row_back_in_postgres;
    NoJournal =>
        a_journal_less_redelivery_writes_no_reclaimed_row_back_on_sqlite_memory,
        a_journal_less_redelivery_writes_no_reclaimed_row_back_in_sqlite,
        a_journal_less_redelivery_writes_no_reclaimed_row_back_in_postgres;
}
