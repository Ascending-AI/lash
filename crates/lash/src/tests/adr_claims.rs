//! Integration witnesses for the facade contracts from the second host API
//! pass, on the core's node over SQLite memory stores (FIG-5307).

use super::*;
use crate::support::TurnInput;
use lash_kernel_value::identity::SessionId;

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
            lash_kernel_value::identity::SessionId::parse(id).expect("nonblank host identity"),
        )
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    }
    let session = source
        .session(lash_kernel_value::identity::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await?;
    let receiving_session = receiving
        .session(lash_kernel_value::identity::SessionId::parse(id).expect("nonblank host identity"))
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
