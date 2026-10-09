//! FIG-5563: a host reads a process's or a definition's workflow through the
//! facade alone, or the typed reason it cannot.

use super::*;
use crate::workflow::{WorkflowRead, WorkflowUnavailable};

const GREETER: &str = "const greet = async (name: string) => {\n  return name;\n};\n";

/// The host authors a module, publishes it and starts it; from then on the
/// process id is all it needs. The read answers what the host's own module
/// copy would have, under the identity the running process records.
#[tokio::test]
async fn a_started_process_reads_as_its_graph_and_canonical_source() {
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    let environment = lash_vm_runtime::LashVmSurface::default()
        .host_environment(&lash_core::ToolCatalog::default())
        .expect("the default surface has an environment");
    let artifact = lash_typescript::link(GREETER, &environment)
        .expect("the greeter links")
        .artifact;
    let artifacts = core.host_artifacts();
    let pin = crate::process::HostArtifactPin::mint();
    artifacts
        .publish_module(&pin, &artifact)
        .await
        .expect("publish the module");
    let entry = artifact
        .exports()
        .processes
        .keys()
        .next()
        .expect("the greeter exports its process")
        .clone();
    let draft = lash_vm::ProcessDefinitionIdentity::from_artifact_export(&artifact, &entry)
        .expect("the export names a process")
        .draft()
        .expect("the definition descriptor");
    let definition = artifacts
        .publish_definition(&pin, &draft)
        .await
        .expect("publish the definition");
    let env_ref = artifacts
        .publish_process_env(
            &pin,
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy::new(
                    crate::TurnBudget::bounded(32),
                    crate::MaxToolCalls::new(1024),
                    crate::NoProgressBudget::bounded(12),
                ),
                lash_core::SessionToolAccess::ambient(),
            ),
        )
        .await
        .expect("publish the process environment");
    let mut args = serde_json::Map::new();
    args.insert("name".to_owned(), serde_json::json!("operator"));
    let process_id = core
        .processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessStartTarget::Definition {
                    definition_id: definition.id.clone(),
                    signature_claim: Some(definition.signature.clone()),
                    args,
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key("workflow-read")
            .with_env_ref(env_ref),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id;
    // The start holds the closure from here: the host's pin and module copy
    // are not what the read answers from.
    artifacts.release(pin).await.expect("release the pin");

    let WorkflowRead::Inspected(of_process) = core
        .processes()
        .graph(&process_id)
        .await
        .expect("the process's workflow reads")
    else {
        panic!("a retained lash_vm process has a workflow");
    };
    let process = core
        .processes()
        .get(&process_id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    assert_eq!(
        Some(&of_process.definition.id),
        process.identity.definition_id.as_ref(),
        "the read names the definition the process runs"
    );
    assert_eq!(of_process.definition, definition);
    assert_eq!(of_process.engine_kind, process.identity.kind);
    assert_eq!(of_process.document.entry, entry);
    assert_eq!(
        of_process.document.graph.source_identity,
        Some(artifact.source_identity())
    );
    assert_eq!(
        of_process.document.graph,
        lash_typescript::workflow_graph::workflow_graph_from_artifact(&artifact)
    );
    assert_eq!(
        of_process.document.source,
        lash_typescript::workflow_graph::typescript_program_source(artifact.ir())
            .expect("the greeter prints")
    );

    let of_definition = artifacts
        .definition_graph(&definition.id)
        .await
        .expect("the definition's workflow reads");
    assert_eq!(of_definition, WorkflowRead::Inspected(of_process));

    let absent = lash_core::ProcessId::fixture("workflow-read-absent");
    assert_eq!(
        core.processes()
            .graph(&absent)
            .await
            .expect("an absent read"),
        WorkflowRead::Unavailable(WorkflowUnavailable::Process { process_id: absent })
    );
    core.shutdown().await.expect("the core shuts down");
}

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
