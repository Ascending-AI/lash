use super::*;
use crate::rlm::RlmSendBuilderExt as _;

const SEED: u64 = 0x5c_f104;

// Facade-level tests for the durable RLM session facts: what a session records
// on the production path, what makes those facts durable, and what cannot
// change them once they are recorded. TypeScript is the sole RLM language
// (ADR 0096), so no fact here names one.

/// Create `session_id` stating its termination as creation config, through
/// the plugin-agnostic options seam (ADR 0066, FIG-4099, FIG-4112).
#[cfg(feature = "rlm")]
async fn create_stating_termination(
    core: &LashCore,
    session_id: &str,
    termination: crate::rlm::RlmTermination,
) -> crate::Result<crate::DurableSession> {
    core.session(session_id)
        .create(crate::SessionCreation {
            plugin_options: lash_core::PluginOptions::typed(
                crate::rlm::RLM_PROTOCOL_PLUGIN_ID,
                crate::rlm::RlmCreateExtras {
                    termination: Some(termination),
                    ..crate::rlm::RlmCreateExtras::default()
                },
            )
            .expect("the typed RLM session options must serialize"),
            ..Default::default()
        })
        .await
}

/// Set the session's RLM render through its one config command, written
/// against `revision`, and answer how it settled.
#[cfg(feature = "rlm")]
async fn set_render(
    session: &crate::LashSession,
    id: &str,
    revision: u64,
    print_chars: usize,
) -> crate::Result<crate::config::ConfigTransactionOutcome> {
    session
        .admin()
        .config()
        .apply(
            crate::config::ConfigWrite::new(id, revision),
            crate::config::ConfigTransaction::of(crate::rlm::SetRlmRender {
                print: crate::rlm::RenderParamsPatch {
                    max_chars: Some(print_chars),
                    ..Default::default()
                },
                preview: crate::rlm::RenderParamsPatch::default(),
            }),
        )
        .await
}

/// The session's recorded RLM namespace, as its owner records it.
#[cfg(feature = "rlm")]
fn recorded_rlm(session: &crate::LashSession) -> crate::rlm::RlmRecordedConfig {
    session
        .read_view()
        .protocol_turn_options()
        .decode()
        .expect("the recorded RLM namespace decodes")
}

#[cfg(feature = "rlm")]
struct RefreshableDialectTool {
    name: std::sync::Mutex<String>,
}

#[cfg(feature = "rlm")]
impl RefreshableDialectTool {
    fn new(name: &str) -> Self {
        Self {
            name: std::sync::Mutex::new(name.to_string()),
        }
    }

    fn replace(&self, name: &str) {
        *self.name.lock_recover() = name.to_string();
    }

    fn definition(&self) -> lash_core::ToolDefinition {
        compile_surface_tool_definition(&self.name.lock_recover())
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl lash_core::ToolProvider for RefreshableDialectTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        let definition = self.definition();
        (definition.manifest.name == name).then(|| Arc::new(definition.contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
    }
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn typescript_is_served_on_the_production_session_path_and_survives_resume() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-typescript-production-path")
        .complete({
            let seen = Arc::clone(&seen);
            let calls = Arc::clone(&calls);
            move |request| {
                let seen = Arc::clone(&seen);
                let calls = Arc::clone(&calls);
                async move {
                    seen.lock_recover().push(system_text(&request));
                    let value = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 42;
                    Ok(text_response(&format!(
                        "<typescript>\nfinish({value});\n</typescript>"
                    )))
                }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("rlm-typescript-production")
        .created()
        .await
        .open()
        .await?;
    let first = session
        .send(TurnInput::text("compute"))
        .require_finish()?
        .output()
        .await?;
    assert!(matches!(
        first.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::Value::Number(ref number),
            ..
        }) if number.as_u64() == Some(42)
    ));

    let execution_snapshot = session
        .admin()
        .state()
        .snapshot_execution()
        .await?
        .expect("the completed RLM turn records an execution snapshot");
    session
        .admin()
        .state()
        .restore_execution(&execution_snapshot)
        .await?;

    let parked = Box::pin(session.park()).await?;
    let resumed = Box::pin(core.resume(parked)).await?;
    let second = resumed
        .send(TurnInput::text("compute again"))
        .require_finish()?
        .output()
        .await?;
    assert!(matches!(
        second.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::FinalValue {
            value: serde_json::Value::Number(ref number),
            ..
        }) if number.as_u64() == Some(43)
    ));

    let prompts = seen.lock_recover();
    assert_eq!(prompts.len(), 2);
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains("## TypeScript execution"))
    );
    assert!(prompts.iter().all(|prompt| prompt.contains("<typescript>")));
    assert!(prompts.iter().all(|prompt| !prompt.contains("<lashlang>")));
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn queued_session_command_restores_the_recorded_typescript_session() -> Result<()> {
    let tools = Arc::new(RefreshableDialectTool::new("before_refresh"));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-typescript-queued-session-command")
        .complete(|_| async { Ok(text_response("<typescript>\nfinish(42);\n</typescript>")) })
        .build()
        .into_handle();
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let store_factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::clone(&tools) as Arc<dyn lash_core::ToolProvider>)
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("rlm-typescript-queued-session-command")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("create a typescript execution snapshot"))
        .require_finish()?
        .output()
        .await?;
    assert!(
        session
            .admin()
            .tools()
            .state()
            .await?
            .contains(&lash_core::ToolId::from("tool:before_refresh"))
    );
    tools.replace("after_refresh");

    let receipt = Box::pin(session.admin().commands().refresh_tool_catalog(
        "restore the recorded typescript session",
        "typescript-session-refresh",
    ))
    .await?;

    // Wait for evidence that the *queued command* was applied, read without a
    // runtime: the durable head's own tool-state snapshot, and the batch
    // settling out of the full queued-work listing. A reopen cannot stand in
    // for either — restoring a session reconciles the live tool surface in
    // memory, so an `open()` shows `after_refresh` whether or not the command
    // ever ran. Both halves have teeth: without the enqueue the head never
    // records the replacement manifest, and with nothing draining the batch
    // the row never settles.
    drop(session);
    let session_id = lash_core::SessionId::from("rlm-typescript-queued-session-command");
    let durable_store = lash_core::runtime::live_session_view(&store_factory, &session_id)
        .await
        .expect("resolve the queued session's store")
        .expect("the queued session exists");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let recorded = lash_core::store::load_session_window_state(
                &durable_store,
                lash_core::store::WindowSelector::Current,
            )
            .await
            .expect("load the durable head")
            .and_then(|loaded| {
                let state = loaded.state;
                state.tool_state_snapshot().map(|snapshot| {
                    snapshot.contains(&lash_core::ToolId::from("tool:after_refresh"))
                })
            })
            .unwrap_or(false);
            // `list_queued_work`, not the pending view: the pending view hides
            // a claimed row and does not show an `AfterCurrentTurnCommit` row
            // before its delivery condition, so its emptiness is not evidence
            // that anything ran. The full listing keeps the batch until the
            // drain settles it (SPEC-PRELUDE, FIG-2875).
            let drained = !durable_store
                .list_queued_work()
                .await
                .expect("read every queued-work row, including claimed ones")
                .iter()
                .any(|batch| batch.batch_id == receipt.batch_id);
            if recorded && drained {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the queued catalog refresh drains and commits its replacement manifest");

    let reopened = core
        .session("rlm-typescript-queued-session-command")
        .created()
        .await
        .open()
        .await?;
    assert!(
        reopened
            .admin()
            .tools()
            .state()
            .await?
            .contains(&lash_core::ToolId::from("tool:after_refresh")),
        "queued catalog refresh must apply the source's replacement manifest"
    );
    assert!(reopened.durable().queued_work().await?.is_empty());
    Ok(())
}

/// A per-turn protocol override naming another `dialect` cannot re-point the
/// language a turn is served in: the RLM owner refuses the run's shape before
/// any provider call, and the session's recorded dialect stands.
///
/// `SendBuilder::protocol_turn_options` is public host surface and the merge
/// behind it is a shallow key merge, so a host-supplied `{"dialect":"..."}` is
/// a write that reaches the protocol without passing through the host's
/// selection. The dialect is the host's choice where it constructs the
/// protocol (ADR 0096), so the field cannot select anything — but a turn that
/// silently accepted it would leave a bundle whose prompt and recorded options
/// disagree, which is the mislabeled-evidence class this layer exists to close.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_per_turn_protocol_override_cannot_re_point_the_dialect() -> Result<()> {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-dialect-turn-override")
        .complete({
            let seen = Arc::clone(&seen);
            move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock_recover().push(system_text(&request));
                    Ok(text_response("<typescript>\nfinish(7);\n</typescript>"))
                }
            }
        })
        .build()
        .into_handle();
    // One store factory across both opens: the reopen has to read what the
    // first session's commit actually wrote.
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("rlm-dialect-turn-override")
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("open the session"))
        .require_finish()?
        .output()
        .await?;

    // The attack: a host-supplied per-turn override naming the retired field.
    let attack = lash_core::ProtocolTurnOptions::from_payload(serde_json::json!({
        "dialect": "lashlang"
    }));
    let attacked = session
        .send(TurnInput::text("switch me"))
        .protocol_turn_options(attack)
        // `require_finish` writes through the same seam and merges shallowly,
        // so the attack has to survive it — otherwise the turn below would be
        // carrying no override at all and this test would measure nothing.
        .require_finish()?;
    assert_eq!(
        attacked
            .run_spec
            .overrides
            .protocol_turn_options
            .as_ref()
            .expect("the turn carries protocol options")
            .payload["dialect"],
        serde_json::json!("lashlang"),
        "the override must actually reach the turn for this to be an attack"
    );
    let refused = attacked
        .output()
        .await
        .expect_err("the RLM owner refuses a run that re-points its dialect");
    assert!(
        matches!(
            &refused,
            crate::EmbedError::Runtime(error)
                if error.code == lash_core::RuntimeErrorCode::RunShapeRefused
                    && error.message.contains(lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID)
        ),
        "the refusal is the run shape's, naming the RLM owner: {refused:?}"
    );
    drop(session);

    // The attacked turn reached no provider; the one prompt served the
    // recorded language.
    let prompts = seen.lock_recover().clone();
    assert_eq!(prompts.len(), 1);
    for prompt in &prompts {
        assert!(
            prompt.contains("## TypeScript execution") && !prompt.contains("<lashlang>"),
            "a per-turn override must not re-point the served language: {prompt}"
        );
    }

    // And the durable bag keeps the host's selection, so the next open under
    // the same host is not refused.
    let reopened = core
        .session("rlm-dialect-turn-override")
        .created()
        .await
        .open()
        .await?;
    assert_eq!(
        reopened
            .read_view()
            .protocol_turn_options()
            .payload
            .get("dialect"),
        Some(&serde_json::json!("typescript")),
        "a per-turn override must not replace the session's recorded dialect"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn create_options_naming_a_dialect_fail_during_session_creation() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let mut options = lash_core::PluginOptions::default();
    options.plugins.insert(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
        serde_json::json!({ "dialect": "python" }),
    );

    let error = match core
        .session("rlm-unknown-dialect")
        .create(crate::SessionCreation {
            plugin_options: options,
            ..Default::default()
        })
        .await
    {
        Ok(_) => panic!("the create contract carries no language choice"),
        Err(error) => error,
    };
    let refusal = rlm_creation_refusal(&error);
    assert_eq!(refusal.owner, lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID);
    assert!(
        refusal.message.contains("invalid creation config") && refusal.message.contains("dialect"),
        "the refusal names the field the host stated: {refusal:?}"
    );
    Ok(())
}

/// The read-only-variables block reaches a served prompt **once**, spelled in
/// the session's language.
///
/// Session-scoped projected bindings are rendered once in the vocabulary of
/// the session that owns them. The two-dialect loop this used to run went with
/// the second dialect (ADR 0096); the once-not-twice claim is what it measured.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn projected_bindings_reach_a_served_prompt_once() -> Result<()> {
    use lash_protocol_rlm::RlmProjectedBindings;

    let served: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = {
        let served = Arc::clone(&served);
        crate::testing::TestProvider::builder()
            .kind("projected-prompt")
            .complete(move |request: crate::provider::LlmRequest| {
                let served = Arc::clone(&served);
                async move {
                    served.lock_recover().push(format!("{request:?}"));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "done".to_string(),
                            response_meta: None,
                        }],
                        ..LlmResponse::default()
                    })
                }
            })
            .build()
            .into_handle()
    };
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("projected-typescript")
        .created()
        .await
        .open()
        .await?;

    session
        .admin()
        .protocol()
        .apply_session_extension(lash_protocol_rlm::rlm_session_projection_extension(
            RlmProjectedBindings::new()
                .bind_json("current_file", serde_json::json!("src/lib.rs"))
                .expect("bind"),
        ))
        .await?;
    session
        .send(TurnInput::text("read the projected binding"))
        .output()
        .await?;

    let prompts = served.lock_recover().clone();
    let prompt = prompts
        .first()
        .expect("the turn reached the provider")
        .clone();
    assert_eq!(
        prompt
            .matches("These read-only values are already in scope")
            .count(),
        1,
        "the read-only block must be assembled once, not once per storage route"
    );
    assert!(
        prompt.contains("Access them directly in `<typescript>`"),
        "the session must be pointed at its own cells"
    );
    assert!(
        !prompt.contains("Access them directly in `<lashlang>`"),
        "the retired dialect's cells must not be named"
    );
    Ok(())
}

/// The typed read reports the facts as recorded (ADR 0066), and the RLM
/// owner admits exactly one command: its render. No command changes a
/// recorded fact (FIG-4379).
#[cfg(feature = "rlm")]
#[tokio::test]
async fn the_typed_read_reports_what_the_session_recorded_and_only_the_render_changes() -> Result<()>
{
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-typed-read")
        .created()
        .await
        .open()
        .await?;

    let recorded = session.rlm_config().expect("recorded config decodes");
    assert_eq!(
        recorded.final_answer_format,
        Some(crate::rlm::RlmFinalAnswerFormat::Markdown)
    );
    assert_eq!(
        recorded.termination, None,
        "a fact the session never stated reads as absent, not as its default"
    );

    let catalog = session.admin().config().commands().await?;
    let rlm_commands = catalog
        .commands
        .iter()
        .filter(|command| command.owner == crate::rlm::RLM_PROTOCOL_PLUGIN_ID)
        .map(|command| command.command.as_str())
        .collect::<Vec<_>>();
    assert_eq!(rlm_commands, vec!["set_render"]);
    let revision = catalog.revision;
    let error = session
        .admin()
        .config()
        .apply(
            crate::config::ConfigWrite::new("rlm-termination", revision),
            crate::config::ConfigTransaction::new().then_entry(crate::config::ConfigCommandEntry {
                owner: crate::rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
                command: "set_termination".to_string(),
                args: serde_json::json!({ "termination": { "kind": "natural" } }),
            }),
        )
        .await
        .expect_err("no RLM command changes the termination");
    assert!(
        matches!(
            &error,
            crate::EmbedError::ConfigSubmit(
                crate::config::ConfigSubmitError::UnknownCommand { .. }
            )
        ),
        "{error:?}"
    );

    let outcome = set_render(&session, "rlm-render", revision, 900).await?;
    assert!(
        matches!(
            outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    let after = recorded_rlm(&session);
    assert_eq!(
        after
            .render
            .as_ref()
            .and_then(|render| render.print.max_chars),
        Some(900)
    );
    assert_eq!(
        session.rlm_config().expect("recorded config decodes"),
        recorded,
        "the render command leaves every recorded fact as it was"
    );
    Ok(())
}

/// A render change publishes its durable revision. The session's creation
/// head already carries its RLM config, so the open publishes nothing and the
/// command's commit is the only publication (FIG-4099).
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_render_change_emits_its_committed_revision() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session("rlm-resident-publication")
        .created()
        .await
        .open()
        .await?;
    let before = session.observe().current_observation();
    let revision = session.admin().config().revision().await?;

    let outcome = set_render(&session, "rlm-render-publication", revision, 700).await?;
    assert!(
        matches!(
            outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&before.cursor)?
    else {
        panic!("committed render publication must remain replayable");
    };
    assert_eq!(
        events
            .iter()
            .map(|event| event.revision())
            .collect::<Vec<_>>(),
        vec![lash_core::SessionRevision::new(1)],
        "the command's commit publishes once and the open publishes nothing"
    );
    assert!(events.iter().all(|event| matches!(
        event.payload,
        lash_core::SessionObservationEventPayload::Committed { .. }
    )));
    Ok(())
}

/// A writer whose resident state was invalidated after another writer
/// changed the render writes against the revision it read: its transaction
/// settles stale and publishes nothing, and its view then reads the other
/// writer's durable render.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_render_change_written_against_a_stale_revision_settles_stale() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();

    let build_core = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .provider(mock_provider())
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())
    };
    let stale_core = build_core()?;
    let concurrent_core = build_core()?;
    let stale = stale_core
        .session("rlm-stale-render")
        .created()
        .await
        .open()
        .await?;
    let concurrent = concurrent_core
        .session("rlm-stale-render")
        .created()
        .await
        .open()
        .await?;
    let read = stale.admin().config().revision().await?;
    let concurrent_outcome = set_render(&concurrent, "rlm-concurrent-render", read, 500).await?;
    assert!(
        matches!(
            concurrent_outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{concurrent_outcome:?}"
    );

    {
        let writer = stale.runtime.writer();
        let mut runtime = writer.lock().await;
        lash_core::testing::invalidate_resident_session_state_for_testing(&mut runtime);
    }

    let outcome = set_render(&stale, "rlm-stale-render", read, 300).await?;
    assert_eq!(
        outcome,
        crate::config::ConfigTransactionOutcome::Stale {
            expected: read,
            actual: read + 1,
        }
    );
    assert_eq!(
        recorded_rlm(&stale)
            .render
            .as_ref()
            .and_then(|render| render.print.max_chars),
        Some(500),
        "the stale writer reads the concurrently recorded render"
    );
    Ok(())
}

/// FIG-4099, FIG-4112: RLM facts are creation config. A session created
/// stating a termination records it with its catalog row; a reopen, which
/// cannot state one, opens with the recorded fact, unchanged, and writes
/// nothing.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_reopen_keeps_the_recorded_rlm_facts_and_writes_nothing() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let mut ledger = None;
    let backend: lash_core::Backend = backend_with_catalog(|inner| {
        let (layer, writes) = CountingWrites::over(inner);
        ledger = Some(writes);
        layer
    })
    .await
    .into();
    let writes = ledger.expect("the catalog is decorated");
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let finish_required = crate::rlm::RlmTermination::FinishRequired { schema: None };
    create_stating_termination(&core, "rlm-reopen-ignores", finish_required.clone()).await?;
    let session = core.session("rlm-reopen-ignores").open().await?;
    assert_eq!(
        session
            .rlm_config()
            .expect("recorded config decodes")
            .termination,
        Some(finish_required.clone()),
        "creation records the stated fact"
    );
    Box::pin(session.close()).await?;
    let runtime_store: Arc<dyn lash_core::RuntimeStore> = backend.session_store_factory();
    let view = lash_core::store::SessionStore::new(
        runtime_store,
        lash_core::SessionId::from("rlm-reopen-ignores"),
    )?;
    let before = view.load_session_head_meta().await?.expect("head");

    writes.lock_recover().clear();
    let reopened = core.session("rlm-reopen-ignores").open().await?;
    assert_eq!(
        *writes.lock_recover(),
        Vec::<&str>::new(),
        "a reopen writes nothing"
    );
    let after = view.load_session_head_meta().await?.expect("head");
    assert_eq!(after.head_revision, before.head_revision);
    assert_eq!(after.config, before.config);
    assert_eq!(
        reopened
            .rlm_config()
            .expect("recorded config decodes")
            .termination,
        Some(finish_required),
        "the reopen runs the recorded fact"
    );
    Ok(())
}

// `a_post_open_dialect_is_compared_against_the_running_default_never_written`
// was deleted with the session language pin (ADR 0096): there is no dialect to
// default, compare, or drift away from the running plugin.

/// A render change is durable: it is still recorded when the session is
/// closed and reopened cold.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_render_change_survives_a_cold_reopen() -> Result<()> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();

    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("rlm-render-roundtrip")
        .created()
        .await
        .open()
        .await?;
    let revision = session.admin().config().revision().await?;
    let outcome = set_render(&session, "rlm-render-roundtrip", revision, 640).await?;
    assert!(
        matches!(
            outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    Box::pin(session.close()).await?;

    let reopened = core
        .session("rlm-render-roundtrip")
        .created()
        .await
        .open()
        .await?;
    assert_eq!(
        recorded_rlm(&reopened)
            .render
            .as_ref()
            .and_then(|render| render.print.max_chars),
        Some(640),
        "the render is still recorded after a cold reopen"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
fn rlm_creation_refusal(error: &crate::EmbedError) -> &lash_core::ConfigRefusal {
    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = error
    else {
        panic!("expected a typed session config refusal, got: {error:?}");
    };
    refusal
        .downcast_ref::<lash_core::ConfigRefusal>()
        .expect("an owner's creation refusal")
}
