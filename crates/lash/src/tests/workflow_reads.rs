//! FIG-5563: a host reads a definition's workflow through the facade alone,
//! or the typed reason it cannot. Reads that answer a document are the laws
//! of `tests/workflow_laws.rs`.

use super::*;
use crate::workflow::{WorkflowRead, WorkflowUnavailable};

/// Registers the fixture engine as an engine author would with no document
/// provider.
struct UndocumentedEngineFactory;

struct UndocumentedEnginePlugin;

impl lash_core::plugin::SessionPlugin for UndocumentedEnginePlugin {
    fn id(&self) -> &'static str {
        "undocumented-engine-plugin"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for UndocumentedEngineFactory {
    fn id(&self) -> &'static str {
        "undocumented-engine-factory"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError>
    {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::new(lash_core::testing::FixtureProcessEngine),
        )])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(UndocumentedEnginePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for UndocumentedEngineFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("undocumented-engine-factory")
    }
}

/// An engine registered with no document provider has no workflow to read:
/// the answer names the engine, and is not an error or a missing artifact. A
/// definition nothing holds is the other typed absence.
#[tokio::test]
async fn an_engine_without_a_document_provider_reads_unsupported() {
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .plugin(Arc::new(UndocumentedEngineFactory))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    let draft = lash_core::ProcessDefinitionDraft::new(
        "testing-fixture",
        serde_json::json!({ "program": "undocumented" }),
        [],
    )
    .expect("the fixture descriptor");
    let definition = artifacts
        .publish_definition(&pin, &draft)
        .await
        .expect("publish the fixture definition");
    assert_eq!(
        artifacts
            .definition_graph(&definition.id)
            .await
            .expect("the read answers"),
        WorkflowRead::Unsupported {
            engine_kind: "testing-fixture".into(),
        }
    );

    let unheld = lash_core::ProcessDefinitionDraft::new(
        "testing-fixture",
        serde_json::json!({ "program": "never published" }),
        [],
    )
    .expect("the fixture descriptor")
    .id();
    assert_eq!(
        artifacts
            .definition_graph(&unheld)
            .await
            .expect("the read answers"),
        WorkflowRead::Unavailable(WorkflowUnavailable::Definition {
            definition_id: unheld,
        })
    );
    core.shutdown().await.expect("the core shuts down");
}

/// FIG-5569: scan plus global changes converges through concurrent writes,
/// deletion and filter exit, and restarts when compaction prunes its fence.
#[tokio::test]
async fn a_roster_and_changes_converge_and_restart_after_compaction() {
    use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _, ProcessRetention as _};
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let registry = stores.process_registry();
    let core = standard_core_builder_over(lash_conformance::backend_over(stores))
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    let mut ids = Vec::new();
    for index in 0..3 {
        let mut registration = lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        );
        registration.identity.label = Some(format!("roster-{index}"));
        ids.push(
            registry
                .register_process(registration)
                .await
                .expect("register")
                .id,
        );
    }
    ids.sort();
    let processes = core.processes();
    let filter = lash_core::ProcessListFilter::default();
    let bound = std::num::NonZeroUsize::new(1).expect("bound");
    let first = processes
        .list(&filter, bound, None)
        .await
        .expect("first page");
    assert_eq!(first.processes.len(), 1);
    let fence = first.change_cursor;
    let stale_continuation = first.continuation.clone();
    let other_store = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("another store");
    assert!(matches!(
        lash_core::ProcessQuery::list_processes_page(
            other_store.process_registry().as_ref(),
            &filter,
            bound,
            stale_continuation.clone(),
        )
        .await,
        Err(lash_core::PluginError::ProcessRegistryCursorBackendMismatch { .. })
    ));

    let mut roster: std::collections::BTreeSet<_> = first
        .processes
        .into_iter()
        .map(|row| row.process_id)
        .collect();
    let second = processes
        .list(&filter, bound, first.continuation)
        .await
        .expect("second page");
    assert_eq!(second.change_cursor, fence);
    assert_eq!(second.processes.len(), 1);
    roster.extend(second.processes.into_iter().map(|row| row.process_id));
    // Both writes run while enumeration is incomplete. The first row is
    // deleted and the second exits the active filter before the changes drain.
    for id in &ids[..2] {
        registry
            .complete_process(
                id,
                lash_core::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::Value::Null),
                ),
                lash_core::ProcessCompletionAuthority::workflow_key(id),
            )
            .await
            .expect("complete");
    }
    // Prune only the first completed row; the other remains as an Upsert
    // outside the active filter, which the feed must still deliver.
    registry
        .prune_terminal_processes(
            u64::MAX,
            Some(lash_core::ProcessListFilter {
                identity_label: Some(
                    processes
                        .get(&ids[0])
                        .await
                        .expect("first row")
                        .expect("retained")
                        .identity
                        .label
                        .expect("label"),
                ),
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            }),
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune one terminal");
    let inserted = registry
        .register_process(lash_core::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("insert during scan")
        .id;
    let mut continuation = second.continuation;
    while let Some(cursor) = continuation {
        let page = processes
            .list(&filter, bound, Some(cursor))
            .await
            .expect("continue scan");
        assert_eq!(page.change_cursor, fence);
        assert!(page.processes.len() <= bound.get());
        roster.extend(page.processes.into_iter().map(|row| row.process_id));
        continuation = page.continuation;
    }
    let mut cursor = fence;
    let mut saw_deletion = false;
    let mut saw_filter_exit = false;
    loop {
        let page = processes
            .changed_since(cursor, bound)
            .await
            .expect("changes");
        cursor = page.next;
        if page.changes.is_empty() {
            break;
        }
        for change in page.changes {
            match change {
                lash_core::facade_support::ObservedProcessChange::Upsert { process } => {
                    if filter.status.matches(process.status()) {
                        roster.insert(process.process_id);
                    } else {
                        saw_filter_exit = true;
                        roster.remove(&process.process_id);
                    }
                }
                lash_core::facade_support::ObservedProcessChange::Deleted { tombstone } => {
                    saw_deletion = true;
                    roster.remove(&tombstone.process_id);
                }
            }
        }
    }
    assert!(saw_deletion);
    assert!(
        saw_filter_exit,
        "global changes include a row exiting the scan filter"
    );
    let expected = std::collections::BTreeSet::from([ids[2].clone(), inserted]);
    assert_eq!(roster, expected);
    registry
        .compact_process_tombstones(u64::MAX, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("compact");
    assert!(matches!(
        processes.changed_since(fence, bound).await,
        Err(EmbedError::Plugin(
            lash_core::PluginError::ProcessChangeCursorPruned { .. }
        ))
    ));
    assert!(matches!(
        processes.list(&filter, bound, stale_continuation).await,
        Err(EmbedError::Plugin(
            lash_core::PluginError::ProcessChangeCursorPruned { .. }
        ))
    ));
    let resumed = processes
        .changed_since(cursor, bound)
        .await
        .expect("horizon remains readable");
    assert!(resumed.retained_after.is_some());
    let mut restarted = std::collections::BTreeSet::new();
    let mut continuation = None;
    let restart_fence = loop {
        let page = processes
            .list(&filter, bound, continuation)
            .await
            .expect("restart scan");
        restarted.extend(page.processes.into_iter().map(|row| row.process_id));
        continuation = page.continuation;
        if continuation.is_none() {
            break page.change_cursor;
        }
    };
    assert_eq!(restarted, expected);
    let final_page = processes
        .changed_since(restart_fence, bound)
        .await
        .expect("scan handshake");
    assert!(final_page.changes.is_empty());
    assert_eq!(final_page.next, restart_fence);
    core.shutdown().await.expect("shutdown");
}
