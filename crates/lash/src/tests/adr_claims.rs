//! Integration witnesses for the facade contracts from the second host API
//! pass, on the core's node over SQLite memory stores (FIG-5307).

use super::*;
use crate::support::TurnInput;
use lash_vocabulary::identity::SessionId;

/// The RLM factory's plugin-owned `lash_vm` engine is installed for a
/// root and for a child session alike, and again by a cold core; each
/// session's RLM options are exactly its create request's (FIG-5296: a
/// child inherits no default of its parent); a second engine of one
/// kind and unknown creation options are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_and_child_materialization_install_the_same_plugin_owned_engines() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let build = || {
        explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .build(crate::tests::host::runtime_lease_owner())
    };
    let core = build()?;
    core.session(
        lash_vocabulary::identity::SessionId::parse("materialize-run")
            .expect("nonblank host identity"),
    )
    .create(crate::SessionCreation::root(mock_session_spec()))
    .await?;
    core.session(
        lash_vocabulary::identity::SessionId::parse("materialize-child")
            .expect("nonblank host identity"),
    )
    .create(crate::SessionCreation::child_of(
        "materialize-run".into(),
        mock_session_spec(),
    ))
    .await?;
    core.session(
        lash_vocabulary::identity::SessionId::parse("materialize-stated")
            .expect("nonblank host identity"),
    )
    .create(crate::SessionCreation {
        parent: Some("materialize-run".into()),
        prompt_plan: None,
        spec: mock_session_spec().plugin_options(
            lash_core::PluginOptions::typed(
                lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash_rlm_types::RlmCreateExtras::default(),
            )
            .map_err(EmbedError::ProtocolTurnOptions)?,
        ),
    })
    .await?;
    for id in ["materialize-run", "materialize-child", "materialize-stated"] {
        let session = core
            .session(
                lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"),
            )
            .open()
            .await?;
        assert_eq!(
            session.runtime.process_engines.require("lashvm")?.kind(),
            "lashvm"
        );
    }
    core.shutdown().await?;
    let cold = build()?;
    assert_eq!(
        cold.host_process_engines.require("lashvm")?.kind(),
        "lashvm"
    );
    for id in ["materialize-run", "materialize-child"] {
        let session = cold
            .session(
                lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"),
            )
            .open()
            .await?;
        assert_eq!(
            session.runtime.process_engines.require("lashvm")?.kind(),
            "lashvm"
        );
    }
    // Distinct plugin ids reach engine installation rather than the
    // duplicate-plugin-id preflight. Both plugins contribute `lashvm`.
    let workers = untimed_fixture_workers();
    let bounds = workers.config().run_bounds;
    let duplicate = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(crate::vm::KernelProcessPluginFactory::new(
            workers,
            crate::vm::TYPESCRIPT_DIALECT,
            bounds,
        )))
        .build(crate::tests::host::runtime_lease_owner());
    assert!(
        matches!(duplicate, Err(EmbedError::Plugin(lash_execution_contracts::plugin_error::PluginError::Registration(message)))
            if message == "duplicate process engine kind `lashvm`; each engine kind may be registered once"),
        "two plugin-owned engines of one kind must be refused"
    );
    let bad = cold
        .session(
            lash_vocabulary::identity::SessionId::parse("materialize-refused")
                .expect("nonblank host identity"),
        )
        .create(crate::SessionCreation {
            parent: Some("materialize-run".into()),
            prompt_plan: None,
            spec: mock_session_spec().plugin_options(
                lash_core::PluginOptions::typed(
                    lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
                    serde_json::json!({"termination": {"kind": "unknown"}}),
                )
                .map_err(EmbedError::ProtocolTurnOptions)?,
            ),
        })
        .await;
    assert!(bad.is_err(), "unknown creation options must be refused");
    assert!(matches!(
        cold.session(
            lash_vocabulary::identity::SessionId::parse("materialize-refused")
                .expect("nonblank host identity")
        )
        .open()
        .await,
        Err(EmbedError::UnknownSession { .. })
    ));
    Ok(())
}

/// A session parked on one core and resumed on a core over another
/// backend keeps its original backend's owners: its processes, its turn
/// inputs and the node that runs its turns are its own backend's, and the
/// receiving core's own session is untouched. (The effect host's
/// await-event keys and the cancel-request record this law also covered
/// were deleted with the engine double; waits are actor wait rows, ADR 0132
/// §6.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_session_observe_wait_cancel_shift_keep_original_owners() -> Result<()> {
    let source_backend = sqlite_memory_store_backend().await;
    let receiving_backend = sqlite_memory_store_backend().await;
    let build = |backend, answer: &'static str| {
        explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(
                text_provider("owner-matrix", answer),
                mock_llm_profile_spec(),
            )
            .build(crate::tests::host::runtime_lease_owner())
    };
    let source = build(source_backend.clone(), "source-answer")?;
    let receiving = build(receiving_backend.clone(), "receiving-answer")?;
    let id = "owner-operation-matrix";
    for core in [&source, &receiving] {
        core.session(
            lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"),
        )
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    }
    let session = source
        .session(lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await?;
    let receiving_session = receiving
        .session(lash_vocabulary::identity::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await?;
    let receiving_output = receiving_session
        .send(TurnInput::text("receiving-row"))
        .id(lash_vocabulary::identity::TurnId::parse("receiving-run")
            .expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(
        receiving_output.assistant_message(),
        Some("receiving-answer")
    );
    let receiving_before = receiving_session
        .durable()
        .turn_input_applications()
        .await?;

    let source_process = source_backend
        .process_registry()
        .register_process_with_observers(
            lash_execution_contracts::testing::held_engine_registration(
                serde_json::json!({"owner": "source"}),
                lash_execution_contracts::process::ProcessProvenance::session(
                    session.observe().process_scope(),
                ),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
            &[SessionId::from(id)],
        )
        .await?
        .id;
    let receiving_process = receiving_backend
        .process_registry()
        .register_process_with_observers(
            lash_execution_contracts::testing::held_engine_registration(
                serde_json::json!({"owner": "receiving"}),
                lash_execution_contracts::process::ProcessProvenance::session(
                    receiving_session.observe().process_scope(),
                ),
                lash_execution_contracts::process::Lifetime::Detached,
            ),
            &[SessionId::from(id)],
        )
        .await?
        .id;

    let parked = Box::pin(session.park()).await?;
    let resumed = receiving.resume(parked).await?;
    let observed = resumed.admin().processes().list_all().await?;
    assert_eq!(
        observed
            .iter()
            .map(|row| &row.process_id)
            .collect::<Vec<_>>(),
        vec![&source_process],
        "the resumed session lists its own backend's processes"
    );
    let original_row = resumed
        .admin()
        .processes()
        .get(&source_process)
        .await?
        .expect("the source process");
    assert!(matches!(
        original_row.input,
        lash_execution_contracts::process::ProcessInput::Engine { payload, .. } if payload == serde_json::json!({"owner": "source"})
    ));
    let receiving_row = receiving_session
        .admin()
        .processes()
        .get(&receiving_process)
        .await?
        .expect("the receiving process");
    assert!(matches!(
        receiving_row.input,
        lash_execution_contracts::process::ProcessInput::Engine { payload, .. } if payload == serde_json::json!({"owner": "receiving"})
    ));
    assert!(
        resumed
            .durable()
            .turn_input_applications()
            .await?
            .is_empty()
    );

    let output = resumed
        .durable()
        .send(TurnInput::text("run on the original backend"))
        .id(lash_vocabulary::identity::TurnId::parse("source-run").expect("nonblank host identity"))
        .output()
        .await?;
    assert_eq!(
        output.assistant_message(),
        Some("source-answer"),
        "the turn runs on the node that serves the session's own backend"
    );
    assert_eq!(resumed.durable().turn_input_applications().await?.len(), 1);
    assert_eq!(
        receiving_session
            .durable()
            .turn_input_applications()
            .await?,
        receiving_before,
        "the receiving core's own session is untouched"
    );
    source.shutdown().await?;
    receiving.shutdown().await?;
    Ok(())
}
