use super::*;

// Facade-level tests for the durable RLM session facts: what a session records
// on the production path, what makes those facts durable, and what cannot
// change them once they are recorded. TypeScript is the sole RLM language
// (ADR 0096), so no fact here names one.

/// State a session's termination through the plugin-agnostic options seam,
/// which applies it as a guarded set-if-unset write (ADR 0066).
#[cfg(feature = "rlm")]
fn stating_termination(
    builder: crate::SessionBuilder,
    termination: crate::rlm::RlmTermination,
) -> crate::SessionBuilder {
    builder
        .plugin_option(
            crate::rlm::RLM_PROTOCOL_PLUGIN_ID,
            crate::rlm::RlmCreateExtras {
                termination: Some(termination),
                ..crate::rlm::RlmCreateExtras::default()
            },
        )
        .expect("the typed RLM session options must serialize")
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
    let core = explicit_ephemeral_facets_with_backend_work(rlm_core_builder().await)
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("rlm-typescript-production").open().await?;
    let first = session
        .turn(TurnInput::text("compute"))
        .require_finish()?
        .run()
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
        .turn(TurnInput::text("compute again"))
        .require_finish()?
        .run()
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
    let backend = memory_backend().await;
    let store_factory = backend.session_store_factory();
    let core =
        explicit_ephemeral_facets_with_backend_work(rlm_core_builder_over(backend.clone().into()))
            .provider(provider)
            .model(mock_model_spec())
            .tools(Arc::clone(&tools) as Arc<dyn lash_core::ToolProvider>)
            .build(crate::testing::runtime_lease_owner())?;

    let session = core
        .session("rlm-typescript-queued-session-command")
        .open()
        .await?;
    session
        .turn(TurnInput::text("create a typescript execution snapshot"))
        .require_finish()?
        .run()
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

    let receipt = session
        .admin()
        .commands()
        .refresh_tool_catalog(
            "restore the recorded typescript session",
            "typescript-session-refresh",
        )
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
    let durable_store = lash_core::SessionStoreFactory::open_existing_store_by_id(
        store_factory.as_ref(),
        &session_id,
    )
    .await
    .expect("resolve the queued session's store")
    .expect("the queued session exists");
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let recorded = lash_core::store::load_persisted_session_state(durable_store.as_ref())
                .await
                .expect("load the durable head")
                .and_then(|state| {
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
            let drained = !lash_core::store::QueuedWorkStore::list_queued_work(
                durable_store.as_ref(),
                &session_id,
            )
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

/// A per-turn protocol override naming the retired `dialect` field cannot
/// re-point the language a turn is served in, and never reaches durable state.
///
/// `TurnBuilder::protocol_turn_options` is public host surface and the merge
/// behind it is a shallow key merge, so a host-supplied `{"dialect":"..."}` is
/// a write that reaches the protocol without passing through create-time
/// resolution. TypeScript is now the only language (ADR 0096), so the field
/// cannot select anything — but a turn that silently accepted it would leave a
/// bundle whose prompt and recorded options disagree, which is the
/// mislabeled-evidence class this layer exists to close.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_per_turn_protocol_override_cannot_name_a_retired_dialect() -> Result<()> {
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
    let backend = memory_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone().into()))
        .provider(provider)
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("rlm-dialect-turn-override").open().await?;
    session
        .turn(TurnInput::text("open the session"))
        .require_finish()?
        .run()
        .await?;

    // The attack: a host-supplied per-turn override naming the retired field.
    let attack = lash_core::ProtocolTurnOptions::from_payload(serde_json::json!({
        "dialect": "lashlang"
    }));
    let attacked = session
        .turn(TurnInput::text("switch me"))
        .protocol_turn_options(attack)
        // `require_finish` writes through the same seam and merges shallowly,
        // so the attack has to survive it — otherwise the turn below would be
        // carrying no override at all and this test would measure nothing.
        .require_finish()?;
    assert_eq!(
        attacked
            .protocol_turn_options
            .as_ref()
            .expect("the turn carries protocol options")
            .payload["dialect"],
        serde_json::json!("lashlang"),
        "the override must actually reach the turn for this to be an attack"
    );
    attacked.run().await?;
    drop(session);

    // Every prompt the provider was handed, including the attacked turn's.
    let prompts = seen.lock_recover().clone();
    assert_eq!(prompts.len(), 2);
    for prompt in &prompts {
        assert!(
            prompt.contains("## TypeScript execution") && !prompt.contains("<lashlang>"),
            "a per-turn override must not re-point the served language: {prompt}"
        );
    }

    // And the durable bag never took the field, so the next open is not
    // refused as a pre-cutover record.
    let reopened = core.session("rlm-dialect-turn-override").open().await?;
    assert!(
        reopened
            .read_view()
            .protocol_turn_options()
            .payload
            .get("dialect")
            .is_none(),
        "a per-turn override must not write the retired field into durable state"
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
        .plugin_options(options)
        .open()
        .await
    {
        Ok(_) => panic!("the create contract carries no language choice"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid RLM create options"));
    assert!(
        error.to_string().contains("dialect"),
        "the refusal names the field the host stated: {error}"
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
    let session = core.session("projected-typescript").open().await?;

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
        .turn(TurnInput::text("read the projected binding"))
        .run()
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

/// The typed read reports the facts as recorded, and the guarded write is
/// idempotent on a fact the session already carries (ADR 0066).
#[cfg(feature = "rlm")]
#[tokio::test]
async fn the_typed_read_reports_what_the_session_recorded_and_restating_it_is_a_no_op() -> Result<()>
{
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-typed-read").open().await?;

    let recorded = session.rlm_config().expect("recorded config decodes");
    assert_eq!(
        recorded.final_answer_format,
        Some(crate::rlm::RlmFinalAnswerFormat::Markdown)
    );
    assert_eq!(
        recorded.termination, None,
        "a fact the session never stated reads as absent, not as its default"
    );

    let unchanged = session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .final_answer_format(crate::rlm::RlmFinalAnswerFormat::Markdown),
        )
        .await
        .expect("restating the recorded final-answer format is a no-op");
    assert_eq!(unchanged, recorded);
    Ok(())
}

/// A guarded write lands on a fact the session has not recorded, and the fact
/// it lands on is the only one it touches.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_lands_on_an_unrecorded_fact_and_leaves_the_rest_alone() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-guarded-write").open().await?;

    let written = session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("an unrecorded termination accepts a write");
    assert_eq!(
        written.termination,
        Some(crate::rlm::RlmTermination::FinishRequired { schema: None })
    );
    assert_eq!(
        written.final_answer_format,
        Some(crate::rlm::RlmFinalAnswerFormat::Markdown),
        "the fact the session already recorded is untouched"
    );
    assert_eq!(
        session.rlm_config().expect("recorded config decodes"),
        written,
        "the write is visible through the read half of the pair"
    );
    Ok(())
}

/// A guarded RLM fact write publishes the durable revision it committed.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn guarded_rlm_fact_set_emits_its_committed_revision() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-resident-publication").open().await?;
    let before = session.observe().current_observation();

    session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("commit RLM fact");

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&before.cursor)?
    else {
        panic!("committed fact publication must remain replayable");
    };
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].payload,
        lash_core::SessionObservationEventPayload::Committed { .. }
    ));
    assert_eq!(events[0].revision(), lash_core::SessionRevision::new(2));
    Ok(())
}

/// A guarded write that disagrees with a recorded fact is refused as a typed
/// value carrying both sides — no host ever has to read the prose to tell a pin
/// conflict from an unrelated failure.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_that_disagrees_is_refused_with_a_typed_conflict() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder().await)
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = stating_termination(
        core.session("rlm-refused-write"),
        crate::rlm::RlmTermination::FinishRequired { schema: None },
    )
    .open()
    .await?;

    let error = session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new().termination(crate::rlm::RlmTermination::Natural),
        )
        .await
        .expect_err("a recorded termination cannot be set to another one");
    let crate::rlm::RlmSessionConfigError::Conflict(
        crate::rlm::RlmSessionConfigConflict::Termination {
            recorded,
            requested,
        },
    ) = error
    else {
        panic!("a termination disagreement must refuse as the typed termination conflict");
    };
    assert_eq!(
        recorded,
        Box::new(crate::rlm::RlmTermination::FinishRequired { schema: None })
    );
    assert_eq!(requested, Box::new(crate::rlm::RlmTermination::Natural));
    assert_eq!(
        session
            .rlm_config()
            .expect("recorded config decodes")
            .termination,
        Some(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        "a refused write leaves the recorded fact exactly as it was"
    );
    Ok(())
}

/// A writer whose resident bag was invalidated must decide set-if-unset from
/// the reloaded durable head. Otherwise a stale `None` smooths over the value
/// another writer recorded between resident reads.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn an_invalidated_guarded_write_refuses_a_concurrently_recorded_termination() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let backend = memory_backend().await;

    let build_core = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone().into()))
            .provider(mock_provider())
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())
    };
    let stale_core = build_core()?;
    let concurrent_core = build_core()?;
    let stale = stale_core.session("rlm-stale-guarded-write").open().await?;
    let concurrent = concurrent_core
        .session("rlm-stale-guarded-write")
        .open()
        .await?;
    concurrent
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("the concurrent writer records the previously unset termination");

    {
        let writer = stale.runtime.writer();
        let mut runtime = writer.lock().await;
        lash_core::testing::invalidate_resident_session_state_for_testing(&mut runtime);
    }

    let error = stale
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new().termination(crate::rlm::RlmTermination::Natural),
        )
        .await
        .expect_err("the stale writer must reload and refuse the recorded termination");
    let crate::rlm::RlmSessionConfigError::Conflict(
        crate::rlm::RlmSessionConfigConflict::Termination {
            recorded,
            requested,
        },
    ) = error
    else {
        panic!("the disagreement must remain a typed termination conflict");
    };
    assert_eq!(
        recorded,
        Box::new(crate::rlm::RlmTermination::FinishRequired { schema: None })
    );
    assert_eq!(requested, Box::new(crate::rlm::RlmTermination::Natural));

    let verifier_core = build_core()?;
    let verifier = verifier_core
        .session("rlm-stale-guarded-write")
        .open()
        .await?;
    assert_eq!(
        verifier
            .rlm_config()
            .expect("the durable verifier config decodes")
            .termination,
        Some(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        "the refused stale write must leave the concurrently recorded head intact"
    );
    Ok(())
}

/// A guarded write that agrees with a concurrently recorded fact performs no
/// durable change, but reloading that fact must still refresh the facade view.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn an_invalidated_same_value_guarded_write_publishes_the_reloaded_config() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let backend = memory_backend().await;

    let build_core = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone().into()))
            .provider(mock_provider())
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())
    };
    let stale_core = build_core()?;
    let concurrent_core = build_core()?;
    let stale = stale_core
        .session("rlm-stale-same-value-guarded-write")
        .open()
        .await?;
    let concurrent = concurrent_core
        .session("rlm-stale-same-value-guarded-write")
        .open()
        .await?;
    let termination = crate::rlm::RlmTermination::FinishRequired { schema: None };
    concurrent
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new().termination(termination.clone()),
        )
        .await
        .expect("the concurrent writer records the previously unset termination");

    {
        let writer = stale.runtime.writer();
        let mut runtime = writer.lock().await;
        lash_core::testing::invalidate_resident_session_state_for_testing(&mut runtime);
    }

    let resolved = stale
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new().termination(termination.clone()),
        )
        .await
        .expect("the stale writer agrees with the concurrently recorded termination");
    assert_eq!(resolved.termination, Some(termination.clone()));
    assert_eq!(
        stale
            .rlm_config()
            .expect("the refreshed facade config decodes")
            .termination,
        Some(termination),
        "a successful no-op guarded write must publish the reloaded config"
    );
    Ok(())
}

// `a_post_open_dialect_is_compared_against_the_running_default_never_written`
// was deleted with the session language pin (ADR 0096): there is no dialect to
// default, compare, or drift away from the running plugin.

/// A guarded write is durable: the fact it lands on is still recorded when the
/// session is closed and reopened cold.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_survives_a_cold_reopen() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let backend = memory_backend().await;

    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone().into()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("rlm-write-roundtrip").open().await?;
    session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("an unrecorded termination accepts a write");
    Box::pin(session.close()).await?;

    let reopened = core.session("rlm-write-roundtrip").open().await?;
    let recorded = reopened.rlm_config().expect("recorded config decodes");
    assert_eq!(
        recorded.termination,
        Some(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        "the written termination is still recorded after a cold reopen"
    );
    Ok(())
}

/// A host that still states a language at open is refused rather than having
/// the statement silently dropped.
///
/// The create contract has no such field since ADR 0096, and `RlmCreateExtras`
/// denies unknown keys, so a pre-cutover host learns at its next open instead
/// of running a session it believes is pinned to something.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn stating_a_dialect_at_open_refuses_instead_of_being_dropped() -> Result<()> {
    let backend = memory_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone().into()))
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;

    let session = core.session("rlm-open-refusal").open().await?;
    Box::pin(session.close()).await?;

    let mut options = lash_core::PluginOptions::default();
    options.plugins.insert(
        lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID.to_string(),
        serde_json::json!({ "dialect": "lashlang" }),
    );
    let Err(error) = core
        .session("rlm-open-refusal")
        .plugin_options(options)
        .open()
        .await
    else {
        panic!("a session cannot be opened with a stated language");
    };
    assert!(
        error.to_string().contains("invalid RLM create options")
            && error.to_string().contains("dialect"),
        "the refusal must name the retired field: {error}"
    );
    Ok(())
}
