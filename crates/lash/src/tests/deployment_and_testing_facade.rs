use super::*;

/// A provider whose `execute` forwards through its granted branch only when
/// the attempt context carries the grant's execution binding — the same seam
/// a deferred-resolution host branches on (FIG-3436). Its tool is a grant
/// target, not a catalog member, so `tool_manifests` is empty.
struct GrantBoundTools;

#[async_trait]
impl ToolProvider for GrantBoundTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, crate::plugins::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!({
                "binding": call
                    .context
                    .tool_execution_binding()
                    .map(|binding| binding.payload.clone()),
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
    let provider = lash_core::plugin::ToolProviderHandle::new(
        lash_core::plugin::RegistryKind::Session,
        "grant-source",
        lash_core::tool_provider_key!("tools"),
    );
    let grant = lash_core::ToolExecutionGrant::from_definition(
        provider.clone(),
        grant_bound_tool_definition(),
    )
    .with_execution_binding(Some(lash_core::ExecutionBinding {
        codec: provider,
        format: lash_core::FormatVersion::ONE,
        payload: serde_json::json!({ "route": "deferred" }),
    }));
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
    let environment = lash_core::testing::process_env_fixture();
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
    let environment = lash_core::testing::process_env_fixture();
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

/// TEST-PARK-USES-FENCED-AUTHORITY (FIG-6175): quiescent session and process
/// actors retain typed parks and resume on redrive; no test writer steals a
/// live claim, changes an unrelated actor, or revives a terminal.
async fn test_parks_are_typed_fenced_and_redrivable(stores: Arc<dyn lash_core::StoreSet>) {
    use crate::durable::{
        ActorKey, ActorState, CommitLabel, DurableError, MailRefusal, NodeId, NodeSpec, Release,
    };
    let backend = crate::durable::DurableBackendBuilder::new(stores)
        .build()
        .unwrap();
    lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref()).await;
    let client = standard_core_builder_over(backend.clone())
        .serve_sessions(false)
        .plugin(crate::testing::process_engine_plugin_fixture())
        .build(crate::testing::runtime_lease_owner())
        .unwrap();
    let session_id = SessionId::fixture("test-park-session");
    let session = client
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .unwrap();
    let handle = session
        .send(TurnInput::text("resume the parked input"))
        .id(crate::TurnId::parse("test-park-input").unwrap())
        .await
        .unwrap();
    let session_actor = ActorKey::session(session_id.as_str()).unwrap();
    let session_reason = crate::SessionParkReason::ActivationLoop {
        failed_activations: 3,
    };
    let store = backend.durable();
    let row = store.actor(&session_actor).await.unwrap().unwrap();
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new("test-park-live-owner"),
            decodes: vec![row.formats],
            serves: crate::testing::node_serves(&client),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let claim = store.claim(&writer, 1).await.unwrap().pop().unwrap();
    assert_eq!(claim.actor, session_actor);
    let before = store.actor(&session_actor).await.unwrap().unwrap();
    assert!(matches!(
        store
            .acquire_idle_for_testing(&writer, &session_actor, before.epoch)
            .await,
        Err(DurableError::OwnershipLost(_))
    ));
    assert!(matches!(
        crate::testing::park_session(&backend, &session_id, session_reason.clone()).await,
        Err(DurableError::OwnershipLost(_))
    ));
    assert_eq!(store.actor(&session_actor).await.unwrap().unwrap(), before);
    let mut tx = store.begin(&session_actor, claim.epoch).await.unwrap();
    tx.ack_seen().give_up(Release::Idle);
    store
        .commit(tx, CommitLabel::SESSION_RELEASE)
        .await
        .unwrap();
    assert_eq!(
        store.actor(&session_actor).await.unwrap().unwrap().state,
        ActorState::Idle
    );

    let process_id = backend
        .process_registry()
        .register_process(lash_core::ProcessRegistration::new(
            lash_core::ProcessInput::Engine {
                kind: "testing-fixture".into(),
                payload: serde_json::Value::Null,
                launch: lash_core::testing::fixture_engine_launch(),
            },
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .unwrap()
        .id;
    store.release_node(&writer).await.unwrap();
    let process_actor = ActorKey::process(process_id.as_str()).unwrap();
    let row = store.actor(&process_actor).await.unwrap().unwrap();
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new("test-park-live-process-owner"),
            decodes: vec![row.formats],
            serves: crate::testing::node_serves(&client),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let process_reason = crate::process::ProcessParkReason::ActivationLoop {
        failed_activations: 4,
    };
    let claim = store.claim(&writer, 1).await.unwrap().pop().unwrap();
    assert_eq!(claim.actor, process_actor);
    let before = store.actor(&process_actor).await.unwrap().unwrap();
    assert!(matches!(
        crate::testing::park_process(&backend, &process_id, process_reason.clone()).await,
        Err(DurableError::OwnershipLost(_))
    ));
    assert_eq!(store.actor(&process_actor).await.unwrap().unwrap(), before);
    let mut tx = store.begin(&process_actor, claim.epoch).await.unwrap();
    tx.ack_seen().give_up(Release::Waiting { next_due: None });
    store
        .commit(tx, CommitLabel::PROCESS_ADVANCE)
        .await
        .unwrap();
    store.release_node(&writer).await.unwrap();

    // A different ready actor must not be taken while acquiring either writer.
    let unrelated = ActorKey::session("test-park-unrelated").unwrap();
    let mut mail = crate::durable::MailTx::new();
    mail.create_actor(
        unrelated.clone(),
        crate::durable::FormatSet::new("test-park-unrelated"),
    );
    store
        .commit_mail(mail, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let unrelated_before = store.actor(&unrelated).await.unwrap();
    crate::testing::park_session(&backend, &session_id, session_reason.clone())
        .await
        .unwrap();
    crate::testing::park_process(&backend, &process_id, process_reason.clone())
        .await
        .unwrap();
    assert_eq!(
        session.park_reason().await.unwrap(),
        Some(session_reason.clone())
    );
    assert_eq!(
        client
            .processes()
            .get(&process_id)
            .await
            .unwrap()
            .unwrap()
            .park,
        crate::process::ProcessParkState::Parked(process_reason.clone())
    );
    assert_eq!(
        store.actor(&session_actor).await.unwrap().unwrap().state,
        ActorState::Parked
    );
    assert_eq!(
        store.actor(&process_actor).await.unwrap().unwrap().state,
        ActorState::Parked
    );
    assert_eq!(store.actor(&unrelated).await.unwrap(), unrelated_before);
    assert!(matches!(
        crate::testing::park_session(
            &backend,
            &SessionId::fixture("test-park-missing"),
            session_reason.clone()
        )
        .await,
        Err(DurableError::MailRefused(MailRefusal::UnknownActor(_)))
    ));
    assert!(matches!(
        crate::testing::park_process(
            &backend,
            &crate::ProcessId::fixture("test-park-missing"),
            process_reason.clone()
        )
        .await,
        Err(DurableError::MailRefused(MailRefusal::UnknownActor(_)))
    ));

    let serving = standard_core_builder_over(backend.clone())
        .plugin(crate::testing::process_engine_plugin_fixture())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("test-park-serving"),
            lash_core::LeaseIncarnationId::new("test-park-serving-boot"),
        ))
        .unwrap();
    assert!(session.redrive("test-park-law").await.unwrap());
    assert!(
        serving
            .processes()
            .redrive(&process_id, "test-park-law")
            .await
            .unwrap()
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), handle.outcome())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status(), crate::TurnStatus::Answered);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        serving.processes().await_output(&process_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        output.into_tool_output().value_for_projection(),
        serde_json::json!({"fixture": "complete"})
    );
    assert!(session.park_reason().await.unwrap().is_none());
    assert_eq!(
        serving
            .processes()
            .get(&process_id)
            .await
            .unwrap()
            .unwrap()
            .park,
        crate::process::ProcessParkState::NotParked
    );
    // The outcome commits before the actor finishes its terminal cascade
    // (ADR 0132 §11). Wait for that release before testing a terminal actor.
    lash_core::testing::poll_until(
        std::time::Duration::from_secs(30),
        "the process actor to finish its terminal cascade",
        || async {
            let row = store.actor(&process_actor).await.unwrap().unwrap();
            (row.state == ActorState::Terminal).then_some(())
        },
    )
    .await;
    assert!(matches!(
        crate::testing::park_process(&backend, &process_id, process_reason).await,
        Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_)))
    ));
    // The session's terminal actor also refuses a park, regardless of its reason.
    serving.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    let row = store.actor(&session_actor).await.unwrap().unwrap();
    let writer = store
        .register_node(&NodeSpec {
            node: NodeId::new("test-park-terminal"),
            decodes: Vec::new(),
            serves: Vec::new(),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let epoch = store
        .acquire_idle_for_testing(&writer, &session_actor, row.epoch)
        .await
        .unwrap();
    let mut tx = store.begin(&session_actor, epoch).await.unwrap();
    tx.give_up(Release::Terminal);
    store
        .commit(tx, CommitLabel::SESSION_RELEASE)
        .await
        .unwrap();
    store.release_node(&writer).await.unwrap();
    assert!(matches!(
        crate::testing::park_session(&backend, &session_id, session_reason).await,
        Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_)))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_test_parks_are_typed_fenced_and_redrivable() {
    test_parks_are_typed_fenced_and_redrivable(sqlite_memory_store_set().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run inside a with-service.sh pg gate"]
async fn postgres_test_parks_are_typed_fenced_and_redrivable_when_configured() {
    let (stores, _database, _attachments) = postgres_store_parts().await;
    test_parks_are_typed_fenced_and_redrivable(stores).await;
}
