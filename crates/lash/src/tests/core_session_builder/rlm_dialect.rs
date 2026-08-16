// Facade-level tests for the RLM source-dialect layer: how a session's dialect
// is selected on the production path, what makes it durable, and what cannot
// change it once it is.

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

    let session = core
        .session("rlm-typescript-production")
        .rlm_dialect(RlmDialect::Typescript)?
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

    let session = core
        .session("rlm-dialect-turn-override")
        .rlm_dialect(RlmDialect::Typescript)?
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
        let session = {
            use crate::rlm::RlmSessionBuilderExt as _;
            core.session(format!("projected-{}", dialect.language_id()))
                .rlm_dialect(dialect)?
                .open()
                .await?
        };

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
