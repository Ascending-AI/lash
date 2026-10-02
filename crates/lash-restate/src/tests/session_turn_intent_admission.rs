//! An env-less session-turn declaration is refused at intent admission,
//! before the drain records any command or registers any process.

use super::*;
use lash_restate_test::{RestateTestBackend, protocol::MessageType};

fn missing_env_start(session: &SessionId) -> lash_core::StartProcessIntent {
    lash_core::StartProcessIntent {
        owner: lash_core::RuntimeOwner::Session(session.clone()),
        declaration: lash_core::ProcessStartDeclaration::new(
            ProcessInput::SessionTurn {
                definition_key: "session-turn-intent-law".to_owned(),
                create_request: Box::new(lash_core::SessionCreateRequest::child_session(
                    session,
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )),
                turn_input: Box::new(lash_core::TurnInput::text("run the child")),
                result: lash_core::SessionTurnOutcome::Turn,
            },
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session)),
            lash_core::Lifetime::Detached,
        ),
    }
}

pub(super) async fn law<S: lash_core::StoreSet + ?Sized>(double: &RestateTestBackend<S>) {
    let session = SessionId::from("session-turn-intent-parent");
    let owner = lash_core::RuntimeOwner::Session(session.clone());
    let originator = lash_core::ProcessOriginator::session(lash_core::SessionScope::new(&session));
    let missing_env = lash_core::ToolIntent::StartProcess(Box::new(missing_env_start(&session)));
    let valid = lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        owner,
        declaration: lash_core::ProcessStartDeclaration::external(
            originator,
            serde_json::Value::Null,
            lash_core::Lifetime::Detached,
        ),
    }));
    let registry = double.engine_stores().process_registry();
    let service = lash_core::testing::effect_backed_process_service(
        Arc::clone(&registry),
        double.engine_stores().process_env_store(),
    );
    // The second batch has an executable prefix. Admission must inspect the
    // entire batch before realizing even that valid prefix.
    for (index, intents) in [
        lash_core::ToolIntents::v3(vec![missing_env.clone()]),
        lash_core::ToolIntents::v3(vec![valid, missing_env]),
    ]
    .into_iter()
    .enumerate()
    {
        let handler = double
            .open_handler(lash_core::AdmittedScope::turn(
                &session,
                lash_core::TurnId::fixture(format!("turn-{index}")),
            ))
            .await
            .expect("open the admission handler");
        let outcomes = tokio::time::timeout(
            Duration::from_secs(30),
            lash_core::testing::execute_tool_intents_with_services(
                handler.scoped(),
                Arc::clone(&service),
                &session,
                &lash_core::ToolCallId::fixture(&format!("env-less-start-{index}")),
                &intents,
            ),
        )
        .await
        .expect("admission must not hang retrying an env-less start")
        .expect("admission answers a refusal rather than a retryable controller fault");
        handler
            .close()
            .await
            .expect("the refused handler completes");
        assert_eq!(outcomes.len(), intents.intents.len());
        for outcome in outcomes {
            let lash_core::ToolIntentExecutionOutcome::Refused {
                kind: lash_core::ToolIntentKind::StartProcess,
                refusal,
                ..
            } = outcome
            else {
                panic!("the whole batch must be refused at admission: {outcome:?}");
            };
            assert_eq!(
                refusal.code(),
                "execution_env_missing",
                "typed admission cause"
            );
            let wire = serde_json::to_value(&refusal).expect("encode refusal");
            assert_eq!(wire["reason"], "execution_env_missing");
            assert_eq!(
                serde_json::from_value::<lash_core::ToolIntentRefusalReason>(wire)
                    .expect("decode typed refusal"),
                refusal,
            );
        }
    }
    assert!(
        registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("read processes after refusal")
            .is_empty(),
        "admission registers nothing, including the valid prefix",
    );
    for handler in double.server().invocations() {
        assert_eq!(handler.status, "completed", "no child or retry remains");
        assert_eq!(handler.retry_count, 0, "the refusal never retries");
        let journal = double
            .server()
            .journal(&handler.id)
            .expect("the handler journal");
        assert!(
            journal.iter().all(|entry| !matches!(
                entry.ty,
                MessageType::RunCommand | MessageType::OneWayCallCommand
            )),
            "admission records no intent command or child send: {journal:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_intent_without_an_env_is_refused_before_recording_sqlite_memory() {
    let double = lash_restate_test::backend(0x4520_0001, Default::default())
        .await
        .expect("the double over SQLite memory");
    law(&double).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_turn_intent_without_an_env_is_refused_before_recording_sqlite_file() {
    let ((directory, double), _tier) =
        super::recorded_child_facts_store_axis::sqlite_file_tier().await;
    law(&double).await;
    drop(directory);
}

pub(super) async fn declared_law<S: lash_core::StoreSet + ?Sized>(double: &RestateTestBackend<S>) {
    use lash_core::tool_dispatch::{
        ParkSite, PendingToolDispatchOutcome, ResolverArming, arm_pending_resolver,
    };

    let session = SessionId::from("declared-session-turn-intent-parent");
    let admitted = lash_core::AdmittedScope::turn(&session, "declared-turn");
    let call_id = lash_core::ToolCallId::fixture("env-less-declared-start");
    let identity = lash_core::derive_tool_intent_identity_under(
        &lash_core::RuntimeOwner::Session(session.clone()),
        "declared-turn",
        &call_id,
        0,
        None,
    );
    let start: lash_core::DeclaredStart = serde_json::from_value(serde_json::json!({
        "start": missing_env_start(&session),
        "identity": identity,
    }))
    .expect("a stored declared start decodes without its constructor");
    let handler = double
        .open_handler(admitted)
        .await
        .expect("open the declared-start handler");
    let scoped = handler.scoped();
    let key = scoped
        .controller()
        .prepare_completion_key(
            scoped.execution_scope(),
            lash_core::AwaitEventWaitIdentity::tool_completion(call_id.clone()),
            true,
        )
        .await
        .expect("prepare the completion key");
    let lash_core::CompletionKeyPreparation::Issued(key) = key else {
        panic!("declared starts require a durable completion key");
    };
    let pending = PendingToolDispatchOutcome {
        call_id: call_id.clone(),
        provider_call_id: None,
        tool_name: "env_less_start".to_owned(),
        args: serde_json::Value::Null,
        key,
        pending: lash_core::PendingCompletion::new().resolved_by_declared_start(start),
        declaring_identity: identity,
        attempts: Vec::new(),
        captures: Vec::new(),
        triggers: Vec::new(),
    };
    let registry = double.engine_stores().process_registry();
    let service = lash_core::testing::effect_backed_process_service(
        Arc::clone(&registry),
        double.engine_stores().process_env_store(),
    );
    let site = ParkSite {
        processes: service.as_ref(),
        owner: lash_core::RuntimeOwner::Session(session),
        call_id: &call_id,
        scope: lash_core::ProcessOpScope::new(scoped),
        child_trace_hook: None,
    };
    let arming = tokio::time::timeout(
        Duration::from_secs(30),
        arm_pending_resolver(&site, &pending),
    )
    .await
    .expect("admission must not hang retrying an env-less declared start")
    .expect("admission refuses without a retryable controller fault");
    let ResolverArming::Settled { failure, armed } = arming else {
        panic!("the refusal settles the call without parking: {arming:?}");
    };
    assert_eq!(failure.code, "execution_env_missing");
    assert_eq!(failure.retry, lash_core::ToolRetryStatus::Never);
    let receipt = armed
        .launch
        .expect("the call reports its typed launch refusal");
    assert!(receipt.process_id.is_none());
    let lash_core::ToolIntentExecutionOutcome::Refused { refusal, .. } = receipt.outcome else {
        panic!("the launch receipt must be a refusal");
    };
    assert_eq!(refusal.code(), "execution_env_missing");
    drop(site);
    handler
        .close()
        .await
        .expect("the refused handler completes");
    assert!(
        registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("read processes")
            .is_empty()
    );
    for handler in double.server().invocations() {
        assert_eq!(handler.status, "completed");
        assert_eq!(handler.retry_count, 0, "the call never retries");
        let journal = double
            .server()
            .journal(&handler.id)
            .expect("the handler journal");
        assert!(
            journal.iter().all(|entry| !matches!(
                entry.ty,
                MessageType::RunCommand | MessageType::OneWayCallCommand
            )),
            "the refusal records no intent command or child send: {journal:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_session_turn_without_an_env_is_refused_before_recording_sqlite_memory() {
    let double = lash_restate_test::backend(0x4520_0002, Default::default())
        .await
        .expect("the double over SQLite memory");
    declared_law(&double).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_session_turn_without_an_env_is_refused_before_recording_sqlite_file() {
    let ((directory, double), _tier) =
        super::recorded_child_facts_store_axis::sqlite_file_tier().await;
    declared_law(&double).await;
    drop(directory);
}
