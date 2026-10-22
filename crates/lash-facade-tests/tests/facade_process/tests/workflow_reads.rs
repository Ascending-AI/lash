//! FIG-5563: a host reads a definition's workflow through the facade alone,
//! or the typed reason it cannot. Reads that answer a document are the laws
//! of `tests/workflow_laws.rs`.

use super::*;

use lash::workflow::{WorkflowRead, WorkflowUnavailable};

/// Registers the fixture engine as an engine author would with no document
/// provider.
struct UndocumentedEngineFactory;

struct UndocumentedEnginePlugin;

impl lash_core::plugin::SessionPlugin for UndocumentedEnginePlugin {
    fn id(&self) -> &'static str {
        "undocumented-engine-plugin"
    }
}
impl UndocumentedEnginePlugin {
    fn declare(
        _reg: &mut lash::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        Ok(())
    }
}

impl lash_core::plugin::PluginFactory for UndocumentedEngineFactory {
    fn id(&self) -> &'static str {
        "undocumented-engine-factory"
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let recipe_factory = Arc::clone(&self);
        let recipe: std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> = {
            let mut reg = lash::plugins::PluginRegistrar::<UndocumentedEnginePlugin>::new(
                lash::plugins::PluginFactory::id(self.as_ref()),
            );
            UndocumentedEnginePlugin::declare(&mut reg)?;
            let id = self.id();
            Ok(lash::plugins::PluginRecipe::new(
                id,
                reg,
                move |_ctx, _state| Ok(Arc::new(UndocumentedEnginePlugin)),
            ))
        };
        let mut recipe = recipe?;
        let engine_factory = Arc::clone(&recipe_factory);
        recipe = recipe.with_process_engine("testing-fixture", move |context| {
            engine_factory.bind_process_engine(context)
        });
        Ok(recipe)
    }
}
impl UndocumentedEngineFactory {
    pub fn bind_process_engine(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<
        lash_core::ProcessEngineRegistration,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(lash_core::ProcessEngineRegistration::accepting(Arc::new(
            lash_core::testing::FixtureProcessEngine,
        )))
    }
}

impl UndocumentedEngineFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("undocumented-engine-factory")
    }
}
impl lash_core::plugin::PluginMetadata for UndocumentedEngineFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

/// An engine registered with no document provider has no workflow to read:
/// the answer names the engine, and is not an error or a missing artifact. A
/// definition nothing holds is the other typed absence.
#[tokio::test]
async fn an_engine_without_a_document_provider_reads_unsupported() {
    let backend = sqlite_memory_store_backend().await;
    let core = standard_core_builder_over(backend)
        .plugin(Arc::new(UndocumentedEngineFactory))
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("the core builds");
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let draft = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        "testing-fixture",
        serde_json::json!({ "program": "undocumented" }),
        [],
    )
    .expect("the fixture descriptor");
    let definition = artifacts
        .publish_definition(&pin, &draft)
        .await
        .expect("publish the fixture definition");
    assert!(matches!(
        artifacts.definition_graph(&definition.id).await.expect("the read answers"),
        WorkflowRead::Unsupported { engine_kind } if engine_kind == "testing-fixture"
    ));

    let unheld = lash_execution_contracts::process::ProcessDefinitionDraft::new(
        "testing-fixture",
        serde_json::json!({ "program": "never published" }),
        [],
    )
    .expect("the fixture descriptor")
    .id();
    assert!(matches!(
        artifacts.definition_graph(&unheld).await.expect("the read answers"),
        WorkflowRead::Unavailable(WorkflowUnavailable::Definition { definition_id }) if definition_id == unheld
    ));
    core.shutdown().await.expect("the core shuts down");
}

/// FIG-5569: scan plus global changes converges through concurrent writes,
/// deletion and filter exit, and restarts when compaction prunes its fence.
#[tokio::test]
async fn a_roster_and_changes_converge_and_restart_after_compaction() {
    use lash_execution_contracts::process::ProcessLifecycle as _;
    use lash_execution_contracts::process::ProcessRegistrar as _;
    use lash_execution_contracts::process::ProcessRetention as _;
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let registry = stores.process_registry();
    let core = standard_core_builder_over(lash_conformance::backend_over(stores))
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("core");
    let mut ids = Vec::new();
    for index in 0..3 {
        let mut registration = lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
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
    let filter = lash_execution_contracts::process::ProcessListFilter {
        created_at_start_ms: Some(0),
        created_at_end_ms: Some(u64::MAX),
        retired_since_ms: Some(1),
        ..lash_execution_contracts::process::ProcessListFilter::default()
    };
    let transported = |cursor: lash_execution_contracts::process::ProcessRosterCursor| {
        let budget = lash::process::ProcessRosterTokenBudget::default();
        let token = cursor.to_token(budget).expect("token within budget");
        lash::process::ProcessRosterCursor::from_token(&token, budget)
            .expect("transported position")
    };
    let bound = std::num::NonZeroUsize::new(1).expect("bound");
    let first = processes
        .list(&filter, bound, None)
        .await
        .expect("first page");
    assert_eq!(first.processes.len(), 1);
    let fence = first.change_cursor;
    let stale_continuation = first.continuation.clone().map(transported);
    let changed_filter = lash_execution_contracts::process::ProcessListFilter {
        retired_since_ms: Some(2),
        ..filter.clone()
    };
    assert!(matches!(
        processes
            .list(&changed_filter, bound, stale_continuation.clone())
            .await,
        Err(EmbedError::Plugin(
            lash_execution_contracts::plugin_error::PluginError::ProcessRosterFilterMismatch { .. }
        ))
    ));
    let other_store = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("another store");
    assert!(matches!(
        lash_execution_contracts::process::ProcessQuery::list_processes_page(
            other_store.process_registry().as_ref(),
            &filter,
            bound,
            stale_continuation.clone(),
        )
        .await,
        Err(lash_execution_contracts::plugin_error::PluginError::ProcessRegistryCursorBackendMismatch { .. })
    ));

    let mut roster: std::collections::BTreeSet<_> = first
        .processes
        .into_iter()
        .map(|row| row.process_id)
        .collect();
    let second = processes
        .list(&filter, bound, first.continuation.map(transported))
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
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    lash_core::ToolCallOutput::success(serde_json::Value::Null),
                ),
                lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(id),
            )
            .await
            .expect("complete");
    }
    // Prune only the first completed row; the other remains as an Upsert
    // outside the active filter, which the feed must still deliver.
    registry
        .prune_terminal_processes(
            u64::MAX,
            Some(lash_execution_contracts::process::ProcessListFilter {
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
                status: lash_execution_contracts::process::ProcessStatusFilter::Any,
                ..lash_execution_contracts::process::ProcessListFilter::default()
            }),
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune one terminal");
    let inserted = registry
        .register_process(lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .expect("insert during scan")
        .id;
    let mut continuation = second.continuation;
    while let Some(cursor) = continuation {
        let page = processes
            .list(&filter, bound, Some(transported(cursor)))
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
            match change.kind {
                lash_execution_contracts::process::ObservedProcessChangeKind::Upsert {
                    process,
                } => {
                    if filter.status.matches(process.status()) {
                        roster.insert(process.process_id);
                    } else {
                        saw_filter_exit = true;
                        roster.remove(&process.process_id);
                    }
                }
                lash_execution_contracts::process::ObservedProcessChangeKind::Deleted {
                    tombstone,
                } => {
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
        .compact_process_tombstones(
            u64::MAX,
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("compact");
    assert!(matches!(
        processes.changed_since(fence, bound).await,
        Err(EmbedError::Plugin(
            lash_execution_contracts::plugin_error::PluginError::ProcessChangeCursorPruned { .. }
        ))
    ));
    assert!(matches!(
        processes
            .list(&filter, bound, stale_continuation.map(transported))
            .await,
        Err(EmbedError::Plugin(
            lash_execution_contracts::plugin_error::PluginError::ProcessChangeCursorPruned { .. }
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

/// FIG-6174: an interrupted projector resumes after its last atomically
/// checkpointed entry, without skipping the rest of that page.
#[tokio::test]
async fn a_process_projector_resumes_after_the_middle_entry_without_skipping_changes() {
    use lash_execution_contracts::process::ProcessLifecycle as _;
    use lash_execution_contracts::process::ProcessRegistrar as _;
    use lash_execution_contracts::process::ProcessRetention as _;
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let registry = stores.process_registry();
    let core = standard_core_builder_over(lash_conformance::backend_over(stores))
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("core");
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            registry
                .register_process(lash_execution_contracts::testing::held_engine_registration(
                    serde_json::Value::Null,
                    lash_execution_contracts::process::ProcessProvenance::host(),
                    lash_execution_contracts::process::Lifetime::Detached,
                ))
                .await
                .expect("register")
                .id,
        );
    }
    registry
        .complete_process(
            &ids[1],
            lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::Value::Null),
            ),
            lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(&ids[1]),
        )
        .await
        .expect("complete");
    registry
        .prune_terminal_processes(
            u64::MAX,
            None,
            lash_execution_contracts::process::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune into a deletion entry");
    let limit = std::num::NonZeroUsize::new(10).expect("page bound");
    let page = core
        .processes()
        .changed_since(
            lash_execution_contracts::process::ProcessChangeCursor::initial(),
            limit,
        )
        .await
        .expect("initial page");
    assert_eq!(page.changes.len(), 5);
    let (stored, _) = lash_execution_contracts::process::ProcessQuery::processes_changed_since(
        registry.as_ref(),
        lash_execution_contracts::process::ProcessChangeCursor::initial(),
        limit.get(),
    )
    .await
    .expect("store-issued entries");
    let mut previous = lash_execution_contracts::process::ProcessChangeCursor::initial();
    for (observed, stored) in page.changes.iter().zip(stored) {
        assert_eq!(
            observed.cursor, stored.cursor,
            "the observer preserves the issued position"
        );
        assert!(observed.cursor.store_sequence() > previous.store_sequence());
        previous = observed.cursor;
    }
    assert_eq!(page.next, previous);
    assert!(matches!(
        page.changes[4].kind,
        lash::process::ObservedProcessChangeKind::Deleted { .. }
    ));
    // The host durably commits each effect and its serialized cursor together.
    // A crash after entry three discards the remainder of this fetched page.
    let mut projected = Vec::new();
    let mut checkpoint = serde_json::Value::Null;
    for change in &page.changes[..3] {
        checkpoint = serde_json::to_value(change.cursor).expect("serialize cursor");
        let cursor: lash_execution_contracts::process::ProcessChangeCursor =
            serde_json::from_value(checkpoint.clone()).expect("durable opaque checkpoint");
        assert_eq!(cursor, change.cursor);
        projected.push(change.clone());
    }
    let expected = page.changes.clone();
    let final_cursor = page.next;
    let horizon = page.retained_after;
    drop(page);
    let checkpoint = serde_json::from_value(checkpoint).expect("recover the host checkpoint");
    let resumed = core
        .processes()
        .changed_since(checkpoint, limit)
        .await
        .expect("resume");
    assert_eq!(resumed.changes, expected[3..]);
    assert_eq!(resumed.next, final_cursor);
    assert_eq!(resumed.retained_after, horizon);
    projected.extend(resumed.changes);
    assert_eq!(
        projected, expected,
        "every returned entry is projected exactly once"
    );
    let empty = core
        .processes()
        .changed_since(final_cursor, limit)
        .await
        .expect("drained feed");
    assert!(empty.changes.is_empty());
    assert_eq!(empty.next, final_cursor);
    assert_eq!(empty.retained_after, horizon);
    core.shutdown().await.expect("shutdown");
}
