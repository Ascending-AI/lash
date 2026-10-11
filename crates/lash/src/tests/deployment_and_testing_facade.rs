use super::*;

/// A provider whose `execute` forwards through its granted branch only when
/// the attempt context carries the grant's execution binding — the same seam
/// a deferred-resolution host branches on (FIG-3436). Its tool is a grant
/// target, not a catalog member, so `tool_manifests` is empty.
struct GrantBoundTools;

#[async_trait]
impl ToolProvider for GrantBoundTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!({
                "binding": call.context.tool_execution_binding().clone(),
            }))
        })
        .await
        .into()
    }
}

fn grant_bound_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:grant_bound",
        "grant_bound",
        "Executes only under a granted route.",
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[tokio::test]
async fn testing_facade_run_tool_granted_honors_the_granted_source_binding() {
    let grant = lash_core::ToolExecutionGrant::from_definition(
        lash_core::plugin::PluginRevision::new("mock", lash_core::plugin::BehaviorRevision::ONE),
        grant_bound_tool_definition(),
    )
    .with_source_id("grant-source")
    .with_execution_binding(serde_json::json!({ "route": "deferred" }));
    let args = serde_json::json!({});

    let outcome = crate::testing::run_tool_granted(&GrantBoundTools, &grant, &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("granted call must complete inline");
    };
    assert!(intents.is_empty());
    let output = result.into_output();
    assert!(output.is_success());
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "binding": { "route": "deferred" } })
    );

    // The ungranted route cannot admit the tool: its manifest lives on the
    // grant, outside the provider's catalog membership.
    let outcome = crate::testing::run_tool(&GrantBoundTools, "grant_bound", &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = outcome else {
        panic!("the catalog-route probe resolves to a completed failure");
    };
    assert!(!result.into_output().is_success());
}

/// FIG-5411: hosts check partial mappings and complete inputs against the
/// retained signature, without admitting a process or accepting a forged claim.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn definition_args_checks_partial_and_complete_inputs_without_starting() {
    use crate::process::{ArgsMismatch, ArgsMode};
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("the core builds");
    let document = crate::workflow::document::parse_document(
        "kernel 1\nnumbers float\nentry handler(event: Text, count: Int) -> Text\n\n\
         fn handler(event, count) {\n  return event\n}\n\nmain {\n  finish null\n}\n",
    )
    .unwrap();
    let pin = crate::process::HostArtifactPin::mint();
    let environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            crate::TurnBudget::bounded(32),
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    );
    let crate::workflow::WorkflowPublish::Published(publication) = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &crate::workflow::edit::Draft::open(document, None).unwrap(),
            &crate::workflow::document::Name::new("handler"),
            &environment,
        )
        .await
        .unwrap()
    else {
        panic!("the document publishes");
    };
    let definition = publication.definition;
    let checker = core.process_definitions();
    let partial = serde_json::json!({"event": "ready"})
        .as_object()
        .unwrap()
        .clone();
    checker
        .check_args(&definition, &partial, ArgsMode::Partial)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(checker.check_args(&definition, &partial, ArgsMode::Complete).await.unwrap(), Err(ArgsMismatch::ArgumentMissing { path, .. }) if path == "count")
    );
    assert_eq!(
        checker
            .check_args(&definition, &partial, ArgsMode::Complete)
            .await
            .unwrap()
            .unwrap_err()
            .code(),
        "argument_missing"
    );
    let complete = serde_json::json!({"event": "ready", "count": 2})
        .as_object()
        .unwrap()
        .clone();
    checker
        .check_args(&definition, &complete, ArgsMode::Complete)
        .await
        .unwrap()
        .unwrap();
    let bad = serde_json::json!({"count": "two"})
        .as_object()
        .unwrap()
        .clone();
    assert!(
        matches!(checker.check_args(&definition, &bad, ArgsMode::Partial).await.unwrap(), Err(ArgsMismatch::Argument { path, .. }) if path == "count")
    );
    assert_eq!(
        checker
            .check_args(&definition, &bad, ArgsMode::Partial)
            .await
            .unwrap()
            .unwrap_err()
            .code(),
        "argument_invalid"
    );
    let extra = serde_json::json!({"other": 1}).as_object().unwrap().clone();
    assert!(
        matches!(checker.check_args(&definition, &extra, ArgsMode::Partial).await.unwrap(), Err(ArgsMismatch::Argument { path, .. }) if path == "other")
    );
    let forged = lash_core::ProcessDefinition::new(
        definition.id,
        lash_core::ProcessSignature::known(serde_json::json!({"forged": true})),
    );
    assert!(matches!(
        checker
            .check_args(&forged, &complete, ArgsMode::Complete)
            .await,
        Ok(Err(ArgsMismatch::DefinitionRefused { .. }))
    ));
    assert!(
        lash_core::testing::process_roster_records_for_fixture(
            core.backend().process_registry().as_ref(),
            &Default::default()
        )
        .await
        .unwrap()
        .is_empty(),
        "argument checking admits no process"
    );
    core.shutdown().await.unwrap();
}

/// The existing scripted-store seam faults the document read a retained
/// definition resolves through, rather than judging the host's arguments.
#[cfg(feature = "rlm")]
struct ScriptedArtifactReads {
    store: Arc<lash_core::testing::Scripted<dyn lash_core::ModuleArtifactStore>>,
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::ModuleArtifactStore for ScriptedArtifactReads {
    async fn publish_module_artifact(
        &self,
        claim: &lash_core::ReferrerClaim,
        reference: &str,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        self.store
            .inner()
            .publish_module_artifact(claim, reference, bytes)
            .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &lash_core::ReferrerClaim,
        reference: &str,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        self.store
            .inner()
            .acquire_module_artifact(claim, reference)
            .await
    }

    async fn end_module_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        self.store.inner().end_module_referrer(cleanup).await
    }

    async fn get_module_artifact(
        &self,
        reference: &str,
    ) -> std::result::Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        self.store
            .call(
                lash_core::testing::Op::listed("get_module_artifact"),
                async { Ok::<(), lash_core::StoreError>(()) },
            )
            .await?;
        self.store.inner().get_module_artifact(reference).await
    }
}

/// FIG-6014: a live read fault while checking a saved-definition start is
/// operational, never an argument refusal.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_saved_definition_start_read_fault_is_an_outer_store_error() {
    let script = lash_core::testing::Script::new();
    let backend = crate::testing::LayeredBackend::over(sqlite_memory_store_backend().await)
        .map_module_artifacts(|store| {
            Arc::new(ScriptedArtifactReads {
                store: script.wrap("definition", store),
            })
        })
        .into_backend();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .build(crate::testing::runtime_lease_owner())
        .unwrap();
    let document = crate::workflow::document::parse_document(
        "kernel 1\nnumbers float\nentry handler(event: Text) -> Text\n\n\
         fn handler(event) {\n  return event\n}\n\nmain {\n  finish null\n}\n",
    )
    .unwrap();
    let pin = crate::process::HostArtifactPin::mint();
    let environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            crate::TurnBudget::bounded(32),
            crate::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    );
    let crate::workflow::WorkflowPublish::Published(publication) = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &crate::workflow::edit::Draft::open(document, None).unwrap(),
            &crate::workflow::document::Name::new("handler"),
            &environment,
        )
        .await
        .unwrap()
    else {
        panic!("the document publishes");
    };
    let read = lash_core::testing::Op::listed("get_module_artifact");
    script
        .on(read)
        .nth(script.calls(read) + 1)
        .before()
        .fail(|| lash_core::StoreError::Contended);
    let args = serde_json::json!({"event": "ready"})
        .as_object()
        .unwrap()
        .clone();
    let error = core
        .process_definitions()
        .check_args(
            &publication.definition,
            &args,
            crate::process::ArgsMode::Complete,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::EmbedError::Plugin(lash_core::PluginError::StoreUnavailable {
                fault: lash_core::store::StoreFault::Contended,
            })
        ),
        "the outer error preserves the actual store fault: {error:?}"
    );
    assert!(error.is_retryable());
    assert!(error.artifact_refusal().is_none());
    let draft = lash_core::ProcessDefinitionDraft::from_store_bytes(
        &publication.definition.id,
        &core
            .backend()
            .definition_store()
            .get_process_definition(&publication.definition.id)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    for operation in ["read", "pin", "publish", "graph", "execution_document"] {
        script
            .on(read)
            .nth(script.calls(read) + 1)
            .before()
            .fail(|| lash_core::StoreError::Contended);
        let artifacts = core.host_artifacts();
        let error = match operation {
            "read" => artifacts
                .get_definition(&publication.definition.id)
                .await
                .unwrap_err(),
            "pin" => artifacts
                .pin_definition(&pin, &publication.definition.id)
                .await
                .unwrap_err(),
            "publish" => artifacts
                .publish_definition(&pin, &draft)
                .await
                .unwrap_err(),
            "graph" => artifacts
                .definition_graph(&publication.definition.id)
                .await
                .unwrap_err(),
            "execution_document" => artifacts
                .execution_document(publication.document.reference())
                .await
                .unwrap_err(),
            _ => unreachable!(),
        };
        assert!(
            matches!(
                error,
                crate::EmbedError::Plugin(lash_core::PluginError::StoreUnavailable {
                    fault: lash_core::store::StoreFault::Contended,
                })
            ),
            "{operation} must preserve the store fault: {error:?}"
        );
        assert!(error.is_retryable());
        assert!(error.artifact_refusal().is_none());
    }
    core.shutdown().await.unwrap();
}
