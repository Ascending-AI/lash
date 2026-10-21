//! Host session-turn starts are root creations (FIG-4594): what a start
//! states is what lash registers, and the start-key fence compares only that.

use lash_vocabulary::schema_contract::*;

/// A host session-turn start under host key `key`, as the host states it
/// when it builds its create request from `spec`: its session's whole config.
pub(super) fn session_turn_start(
    key: &str,
    text: &str,
    spec: &lash::SessionSpec,
) -> lash_execution_contracts::process::ProcessStartRequest {
    session_turn_start_of(
        key,
        text,
        lash_execution_contracts::session_types::SessionCreateRequest::root(
            lash_core::PluginOptions::default(),
        )
        .with_spec(spec),
    )
}

/// A host session-turn start under host key `key` carrying `create_request`.
pub(super) fn session_turn_start_of(
    key: &str,
    text: &str,
    create_request: lash_execution_contracts::session_types::SessionCreateRequest,
) -> lash_execution_contracts::process::ProcessStartRequest {
    lash_execution_contracts::process::ProcessStartRequest::session_turn(
        "keys-session-turn-start",
        create_request.with_session_id(lash_vocabulary::identity::SessionId::fixture(format!(
            "{key}-child"
        ))),
        TurnInput::text(text),
        lash_execution_contracts::process::SessionTurnOutcome::Turn,
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::Lifetime::Detached,
    )
    .with_host_start_key(key)
}

/// Issue `request` on `core` through its own effect host. `operation`
/// names the host call in the law.
pub(super) async fn start_on(
    _double: &Double,
    core: &LashCore,
    _operation: &str,
    request: lash_execution_contracts::process::ProcessStartRequest,
) -> Result<lash_execution_contracts::process::ProcessStartReceipt, lash::EmbedError> {
    core.processes().start(request, core.effect_host()).await
}

/// The create request the start retained under `process_id` recorded.
pub(super) async fn recorded_create_request(
    double: &Double,
    process_id: &lash_vocabulary::identity::ProcessId,
) -> lash_execution_contracts::session_types::SessionCreateRequest {
    let record = double
        .backend()
        .process_registry()
        .get_process(process_id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    let lash_execution_contracts::process::ProcessInput::SessionTurn { create_request, .. } =
        record.input.as_ref()
    else {
        panic!("the retained start is a session turn: {record:?}");
    };
    create_request.as_ref().clone()
}

/// A host session-turn start is a root creation: it states its session's
/// whole spec, and lash registers exactly what it stated (FIG-4594). Nothing
/// a catalog mints or a deployment defaults is laid under it, in the request
/// or in the environment the start captures, so the start-key fence compares
/// only what the host stated. The same start retried under its host key
/// after the catalog entry was edited, and again after the host changed what
/// it passes to its other creations and starts, is returned the retained
/// process; neither retry is a `StartKeyConflict`. A start that states
/// another spec, or another input, under the key still is. A start that
/// states no spec is refused typed and registers nothing.
pub(super) async fn a_session_turn_start_retried_after_the_host_changed_what_it_passes_keeps_its_retained_start(
    tier: Tier,
) {
    let Some(double) = double(tier).await else {
        return;
    };
    let key = "keys-start-retry";
    let glm = Route::new("glm answers");
    let kimi = Route::new("kimi answers");
    let stated = lash::SessionSpec::new(
        GLM,
        lash::TurnBudget::bounded(6),
        lash::MaxToolCalls::new(1024),
        lash::NoProgressBudget::bounded(12),
    )
    .generation(lash::direct::GenerationOptions {
        seed: Some(11),
        ..Default::default()
    });
    let first = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r1",
            route: &glm,
        }],
        "keys-boot-1",
    );

    // A start that states no spec is refused typed, and so is one that
    // states a policy and names no model; neither registers anything.
    let bare = lash_execution_contracts::session_types::SessionCreateRequest::root(
        lash_core::PluginOptions::default(),
    );
    let unstated = start_on(
        &double,
        &first,
        "keys-start-unstated",
        session_turn_start_of(key, "run the child", bare.clone()),
    )
    .await;
    assert!(
        matches!(
            &unstated,
            Err(lash::EmbedError::SessionTurnStartUnspecified {
                unstated: lash_execution_contracts::session_types::UnstatedSessionConfig::Policy
            })
        ),
        "a start with no spec is refused typed: {unstated:?}"
    );
    let mut modelless = bare;
    modelless.policy = Some(lash::runtime::SessionPolicy::new(
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
        lash::NoProgressBudget::bounded(12),
    ));
    let unstated = start_on(
        &double,
        &first,
        "keys-start-modelless",
        session_turn_start_of(key, "run the child", modelless),
    )
    .await;
    assert!(
        matches!(
            &unstated,
            Err(lash::EmbedError::SessionTurnStartUnspecified {
                unstated: lash_execution_contracts::session_types::UnstatedSessionConfig::Model
            })
        ),
        "a start that names no model is refused typed: {unstated:?}"
    );

    let started = start_on(
        &double,
        &first,
        "keys-start-first",
        session_turn_start(key, "run the child", &stated),
    )
    .await
    .expect("the first start registers");
    assert_eq!(
        started.disposition,
        lash_execution_contracts::process::ProcessRegistrationOutcome::Created,
        "the refused starts registered nothing under the key"
    );
    let recorded = recorded_create_request(&double, &started.process_id).await;
    assert_eq!(
        recorded.model.as_ref().map(LlmProfileKey::as_str),
        Some(GLM),
        "the start records the key the host stated"
    );
    let recorded_policy = recorded
        .policy
        .clone()
        .expect("the start records the policy the host stated");
    assert_eq!(recorded_policy.model, None, "registration mints nothing");
    assert_eq!(recorded_policy.turn_budget, lash::TurnBudget::bounded(6));
    assert_eq!(recorded_policy.generation.seed, Some(11));
    drop(first);

    let edited = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r2",
            route: &glm,
        }],
        "keys-boot-2",
    );
    let after_edit = start_on(
        &double,
        &edited,
        "keys-start-after-edit",
        session_turn_start(key, "run the child", &stated),
    )
    .await
    .expect("the retry after a catalog edit presents the same start");
    assert_eq!(after_edit.process_id, started.process_id);
    assert_eq!(
        after_edit.disposition,
        lash_execution_contracts::process::ProcessRegistrationOutcome::Existing
    );
    drop(edited);

    // The host changes what it passes: its catalog serves another key too,
    // and its other creations and starts state another spec.
    let moved = core(
        &double,
        &[
            Entry {
                key: KIMI,
                wire_model: "kimi-k3",
                revision: "r1",
                route: &kimi,
            },
            Entry {
                key: GLM,
                wire_model: "glm-5.3-flash",
                revision: "r2",
                route: &glm,
            },
        ],
        "keys-boot-3",
    );
    let changed = lash::SessionSpec::new(
        KIMI,
        lash::TurnBudget::bounded(2),
        lash::MaxToolCalls::new(1024),
        lash::NoProgressBudget::bounded(12),
    )
    .generation(lash::direct::GenerationOptions {
        seed: Some(99),
        ..Default::default()
    });
    moved
        .session(
            lash_vocabulary::identity::SessionId::parse("keys-start-other-session")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(changed.clone()))
        .await
        .expect("the host creates a session from its changed spec");
    let elsewhere = start_on(
        &double,
        &moved,
        "keys-start-elsewhere",
        session_turn_start("keys-start-elsewhere", "run another child", &changed),
    )
    .await
    .expect("the host's other start registers");
    assert_ne!(elsewhere.process_id, started.process_id);
    let after_change = start_on(
        &double,
        &moved,
        "keys-start-after-change",
        session_turn_start(key, "run the child", &stated),
    )
    .await
    .expect("the retry after the host changed what it passes presents the same start");
    assert_eq!(after_change.process_id, started.process_id);
    assert_eq!(
        after_change.disposition,
        lash_execution_contracts::process::ProcessRegistrationOutcome::Existing
    );
    let after_retries = recorded_create_request(&double, &started.process_id).await;
    assert_eq!(
        serde_json::to_value(&after_retries).expect("encode the retained request"),
        serde_json::to_value(&recorded).expect("encode the recorded request"),
        "no retry rewrote what the start recorded"
    );

    for (operation, request) in [
        (
            "keys-start-other-spec",
            session_turn_start(key, "run the child", &changed),
        ),
        (
            "keys-start-other-input",
            session_turn_start(key, "run another child", &stated),
        ),
    ] {
        let other = start_on(&double, &moved, operation, request).await;
        match &other {
            Err(lash::EmbedError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::StartKeyConflict { .. },
            )) => {}
            other => panic!("{operation}: another start under the key conflicts, got: {other:?}"),
        }
    }
}
