// Facade-level tests for the RLM source-dialect layer: how a session's dialect
// is selected on the production path, what makes it durable, and what cannot
// change it once it is.

/// State a session's dialect through the plugin-agnostic options seam, which
/// applies it as a guarded set-if-unset write (ADR 0066).
#[cfg(feature = "rlm")]
fn stating_dialect(builder: crate::SessionBuilder, dialect: RlmDialect) -> crate::SessionBuilder {
    builder
        .plugin_option(
            crate::rlm::RLM_PROTOCOL_PLUGIN_ID,
            crate::rlm::RlmCreateExtras {
                dialect: Some(dialect),
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

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true }))
    }
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn typescript_dialect_is_selected_on_the_production_session_path_and_survives_resume()
-> Result<()> {
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
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())?;

    let session = stating_dialect(
        core.session("rlm-typescript-production"),
        RlmDialect::Typescript,
    )
    .open()
    .await?;
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

    let parked = session.park().await?;
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
    assert!(prompts.iter().all(|prompt| prompt.contains("## TypeScript execution")));
    assert!(prompts.iter().all(|prompt| prompt.contains("<typescript>")));
    assert!(prompts.iter().all(|prompt| !prompt.contains("<lashlang>")));
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn queued_session_command_restores_the_recorded_typescript_dialect() -> Result<()> {
    let tools = Arc::new(RefreshableDialectTool::new("before_refresh"));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("rlm-typescript-queued-session-command")
        .complete(|_| async {
            Ok(text_response(
                "<typescript>\nfinish(42);\n</typescript>",
            ))
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .tools(Arc::clone(&tools) as Arc<dyn lash_core::ToolProvider>)
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .build(crate::testing::runtime_lease_owner())?;

    let session = stating_dialect(
        core.session("rlm-typescript-queued-session-command"),
        RlmDialect::Typescript,
    )
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
        .commands()
        .refresh_tool_catalog(
            "restore the recorded typescript dialect",
            "typescript-dialect-refresh",
        )
        .await?;

    session.await_queued_work_batch(&receipt.batch_id).await?;
    drop(session);
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
    assert!(reopened.queued_work().await?.is_empty());
    Ok(())
}

/// A per-turn protocol override cannot rewrite the session's recorded dialect.
///
/// `TurnBuilder::protocol_turn_options` is public host surface and the merge
/// behind it is a shallow key merge, so a host-supplied `{"dialect":"..."}` is
/// a write that reaches the protocol without passing through the create-time
/// resolution that enforces the pin. It does not reach durable state — the
/// commit persists the session-level options, not the turn-scoped merge — but
/// nothing pinned that, and the difference between "cannot" and "happens not
/// to" is one refactor. A session that could be re-pointed mid-life would
/// produce a bundle whose prompt and recorded dialect disagree, which is the
/// mislabeled-evidence class this layer exists to close.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_per_turn_protocol_override_cannot_rewrite_the_recorded_dialect() -> Result<()> {
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
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(provider)
        .model(mock_model_spec())
        .store_factory(store_factory)
        .build(crate::testing::runtime_lease_owner())?;

    let session = stating_dialect(
        core.session("rlm-dialect-turn-override"),
        RlmDialect::Typescript,
    )
    .open()
    .await?;
    session
        .turn(TurnInput::text("pin the dialect"))
        .require_finish()?
        .run()
        .await?;

    // The attack: a host-supplied per-turn override naming the other dialect.
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
            "a per-turn override must not re-point the served dialect: {prompt}"
        );
    }

    // And the durable pin is unchanged, so the next open still serves
    // TypeScript.
    let reopened = core.session("rlm-dialect-turn-override").open().await?;
    let recorded = reopened.read_view().protocol_turn_options().payload["dialect"].clone();
    assert_eq!(
        recorded,
        serde_json::json!("typescript"),
        "the recorded dialect must survive a per-turn override"
    );
    Ok(())
}

#[cfg(feature = "rlm")]
#[tokio::test]
async fn unknown_rlm_dialect_fails_during_session_creation() -> Result<()> {
    let core = explicit_ephemeral_facets(rlm_core_builder())
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
        Ok(_) => panic!("an unregistered dialect must fail at session creation"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid RLM create options"));
    assert!(error.to_string().contains("python"));
    Ok(())
}

/// The read-only-variables block reaches a served prompt **once**, spelled in
/// the session's own dialect.
///
/// `TurnInput::rlm_project` attaches the same bindings on two seams: the
/// protocol's plugin input, whose prompt hook renders the block with the
/// session's vocabulary, and a `ProtocolTurnExtension` handle carried for
/// validation. `lash-core` used to render that handle's own
/// `prompt_contributions()` into the same prompt, and `PromptLayer` does not
/// dedup — so the block landed twice, and the second copy was always Lashlang,
/// because the handle is built by a host before any session has resolved a
/// dialect. A TypeScript session read "Access them directly in `<lashlang>`
/// blocks" underneath a correct copy of the same block.
///
/// Both halves are asserted because they fail independently: the count, and the
/// spelling, in both dialects.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn projected_bindings_reach_a_served_prompt_once_in_the_sessions_dialect() -> Result<()> {
    use lash_protocol_rlm::{RlmProjectedBindings, RlmTurnInputExt};

    for (dialect, own_tag, foreign_tag) in [
        (
            lash_rlm_types::RlmDialect::Lashlang,
            "<lashlang>",
            "<typescript>",
        ),
        (
            lash_rlm_types::RlmDialect::Typescript,
            "<typescript>",
            "<lashlang>",
        ),
    ] {
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
                            full_text: "done".to_string(),
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
        let core = explicit_ephemeral_facets(rlm_core_builder())
            .provider(provider)
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())?;
        let session = stating_dialect(
            core.session(format!("projected-{}", dialect.language_id())),
            dialect,
        )
        .open()
        .await?;

        let input = TurnInput::text("read the projected binding")
            .rlm_project(
                RlmProjectedBindings::new()
                    .bind_json("current_file", serde_json::json!("src/lib.rs"))
                    .expect("bind"),
            )
            .map_err(|err| EmbedError::Session(SessionError::Protocol(err.to_string())))?;
        session.turn(input).run().await?;

        let prompts = served.lock_recover().clone();
        let prompt = prompts.first().expect("the turn reached the provider").clone();
        assert_eq!(
            prompt
                .matches("These read-only values are already in scope")
                .count(),
            1,
            "the read-only block must be assembled once, not once per storage route ({})",
            dialect.language_id()
        );
        assert!(
            prompt.contains(&format!("Access them directly in `{own_tag}`")),
            "{} must be pointed at its own cells",
            dialect.language_id()
        );
        assert!(
            !prompt.contains(&format!("Access them directly in `{foreign_tag}`")),
            "{} must not be pointed at the other dialect's cells",
            dialect.language_id()
        );
    }
    Ok(())
}

/// The typed read reports the facts as recorded, and the guarded write is
/// idempotent on a fact the session already carries (ADR 0066).
#[cfg(feature = "rlm")]
#[tokio::test]
async fn the_typed_read_reports_what_the_session_recorded_and_restating_it_is_a_no_op()
-> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = stating_dialect(core.session("rlm-typed-read"), RlmDialect::Typescript)
        .open()
        .await?;

    let recorded = session.rlm_config();
    assert_eq!(recorded.dialect, Some(RlmDialect::Typescript));
    assert_eq!(
        recorded.final_answer_format,
        Some(crate::rlm::RlmFinalAnswerFormat::Markdown)
    );
    assert_eq!(
        recorded.termination, None,
        "a fact the session never stated reads as absent, not as its default"
    );

    let unchanged = session
        .set_rlm_config_if_unset(crate::rlm::RlmSessionConfig::new().dialect(RlmDialect::Typescript))
        .await
        .expect("restating the recorded dialect is a no-op");
    assert_eq!(unchanged, recorded);
    Ok(())
}

/// A guarded write lands on a fact the session has not recorded, and the fact
/// it lands on is the only one it touches.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_lands_on_an_unrecorded_fact_and_leaves_the_rest_alone() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = stating_dialect(core.session("rlm-guarded-write"), RlmDialect::Typescript)
        .open()
        .await?;

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
    assert_eq!(written.dialect, Some(RlmDialect::Typescript));
    assert_eq!(
        session.rlm_config(),
        written,
        "the write is visible through the read half of the pair"
    );
    Ok(())
}

/// A staged RLM fact changes resident authority without advancing the durable
/// revision, so it can never be published as `Committed`.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn staged_rlm_fact_set_emits_resident_changed_at_the_same_revision() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = stating_dialect(core.session("rlm-resident-publication"), RlmDialect::Typescript)
        .open()
        .await?;
    let before = session.observe().current_observation();

    session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("set staged RLM fact");

    let lash_core::facade_support::SessionResume::Replayed { events } =
        session.observe().resume_from_cursor(&before.cursor)?
    else {
        panic!("staged fact publication must remain replayable");
    };
    assert!(matches!(
        events.as_slice(),
        [event]
            if event.revision() == lash_core::SessionRevision::new(0)
                && matches!(
                    event.payload,
                    lash_core::SessionObservationEventPayload::ResidentChanged { .. }
                )
    ));
    assert!(events.iter().all(|event| !matches!(
        event.payload,
        lash_core::SessionObservationEventPayload::Committed { .. }
    )));
    Ok(())
}

/// A guarded write that disagrees with a recorded fact is refused as a typed
/// value carrying both sides — no host ever has to read the prose to tell a pin
/// conflict from an unrelated failure.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_that_disagrees_is_refused_with_a_typed_conflict() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = stating_dialect(core.session("rlm-refused-write"), RlmDialect::Typescript)
        .open()
        .await?;

    let error = session
        .set_rlm_config_if_unset(crate::rlm::RlmSessionConfig::new().dialect(RlmDialect::Lashlang))
        .await
        .expect_err("a recorded dialect cannot be set to another one");
    let crate::rlm::RlmSessionConfigError::Conflict(
        crate::rlm::RlmSessionConfigConflict::Dialect {
            recorded,
            requested,
        },
    ) = error
    else {
        panic!("a dialect disagreement must refuse as the typed dialect conflict");
    };
    assert_eq!(recorded, RlmDialect::Typescript);
    assert_eq!(requested, RlmDialect::Lashlang);
    assert_eq!(
        session.rlm_config().dialect,
        Some(RlmDialect::Typescript),
        "a refused write leaves the recorded fact exactly as it was"
    );
    Ok(())
}

/// A host that states no dialect still gets one: the first open records the
/// default, and that default is a pin like any other. A post-open statement is
/// compared against the dialect the session is running and never written, so
/// the recorded fact cannot drift away from the plugin that is executing.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_post_open_dialect_is_compared_against_the_running_default_never_written() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("rlm-unrecorded-dialect").open().await?;
    assert_eq!(
        session.rlm_config().dialect,
        Some(RlmDialect::Lashlang),
        "a host that states no dialect gets the default recorded at its first open"
    );

    let agreed = session
        .set_rlm_config_if_unset(crate::rlm::RlmSessionConfig::new().dialect(RlmDialect::Lashlang))
        .await
        .expect("stating the dialect the session is running is a no-op");
    assert_eq!(agreed, session.rlm_config());

    let error = session
        .set_rlm_config_if_unset(crate::rlm::RlmSessionConfig::new().dialect(RlmDialect::Typescript))
        .await
        .expect_err("an open session cannot be moved onto another dialect");
    let crate::rlm::RlmSessionConfigError::Conflict(
        crate::rlm::RlmSessionConfigConflict::Dialect {
            recorded,
            requested,
        },
    ) = error
    else {
        panic!("a dialect disagreement must refuse as the typed dialect conflict");
    };
    assert_eq!(
        recorded,
        RlmDialect::Lashlang,
        "the conflict names the dialect the session is running, not an absent one"
    );
    assert_eq!(requested, RlmDialect::Typescript);
    Ok(())
}

/// A guarded write is durable: the fact it lands on is still recorded when the
/// session is closed and reopened cold.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn a_guarded_write_survives_a_cold_reopen() -> Result<()> {
    use crate::rlm::RlmSessionExt as _;

    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory)
        .build(crate::testing::runtime_lease_owner())?;

    let session = stating_dialect(core.session("rlm-write-roundtrip"), RlmDialect::Typescript)
        .open()
        .await?;
    session
        .set_rlm_config_if_unset(
            crate::rlm::RlmSessionConfig::new()
                .termination(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .expect("an unrecorded termination accepts a write");
    session.close().await?;

    let reopened = core.session("rlm-write-roundtrip").open().await?;
    let recorded = reopened.rlm_config();
    assert_eq!(
        recorded.termination,
        Some(crate::rlm::RlmTermination::FinishRequired { schema: None }),
        "the written termination is still recorded after a cold reopen"
    );
    assert_eq!(recorded.dialect, Some(RlmDialect::Typescript));
    Ok(())
}

/// The same refusal reaches a host that states a disagreeing dialect at open:
/// the open fails rather than quietly running the session in its old dialect.
#[cfg(feature = "rlm")]
#[tokio::test]
async fn stating_a_disagreeing_dialect_at_open_refuses_instead_of_falling_back() -> Result<()> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(rlm_core_builder())
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(store_factory)
        .build(crate::testing::runtime_lease_owner())?;

    let session = stating_dialect(core.session("rlm-open-refusal"), RlmDialect::Typescript)
        .open()
        .await?;
    session.close().await?;

    let Err(error) = stating_dialect(core.session("rlm-open-refusal"), RlmDialect::Lashlang)
        .open()
        .await
    else {
        panic!("a recorded dialect cannot be reopened as another one");
    };
    assert!(
        error.to_string().contains(
            "RLM session dialect is durably pinned to `typescript` and cannot be set to `lashlang`"
        ),
        "the refusal must render the one typed message: {error}"
    );
    Ok(())
}
