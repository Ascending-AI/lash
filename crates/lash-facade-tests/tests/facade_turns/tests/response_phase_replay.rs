//! FIG-4390: a run's response phase plan is recorded with its paid
//! completion, so the assistant-response callbacks a resumed run applies are
//! the recorded plan's, in recorded order, whatever the resuming core has
//! installed (ADR 0105 §1). A recorded callback the resuming core lacks
//! refuses before any callback runs; the running implementation of a recorded
//! key finishes an uncommitted derivation.

use super::*;

/// What the provider answers every call with.
const RAW: &str = "the raw completion";

fn callback_session(
    mut plugins: Vec<Arc<dyn lash_core::plugin::PluginFactory>>,
) -> Arc<lash_core::facade_support::PluginSession> {
    plugins.push(Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    ));
    lash_core::facade_support::PluginHost::new(
        plugins,
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
            lash_core::facade_support::SystemClock,
        )),
    )
    .build_session(lash_core::plugin::PluginSessionRequest::creation(
        "recorded-response-plan",
        lash_core::plugin::SessionAuthorityContext::fixture(),
    ))
    .expect("materialize the callback registry")
}

fn appending_callback(
    plugin: &'static str,
    key: lash_core::plugin::HookKey,
    suffix: &'static str,
    calls: &Arc<AtomicUsize>,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    let calls = Arc::clone(calls);
    let declaration = lash_core::plugin::PluginDeclaration::initial(plugin);
    Arc::new(StaticPluginFactory::new(
        declaration,
        lash_core::facade_support::PluginSpec::new().with_assistant_response(
            key,
            None,
            Arc::new(move |ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                let response = text_response(&format!("{}{suffix}", ctx.response.full_text()));
                Box::pin(async move {
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        ),
    ))
}

async fn replay_callbacks(
    recorded: &lash_core::facade_support::PluginSession,
    live: &lash_core::facade_support::PluginSession,
    states: &[lash_core::AssistantStreamHookState],
) -> std::result::Result<String, lash_execution_contracts::plugin_error::PluginError> {
    let plan: lash_core::AssistantResponsePlan =
        serde_json::from_value(serde_json::to_value(recorded.assistant_response_plan()).unwrap())
            .unwrap();
    let transforms = live
        .transform_assistant_response(
            &lash_core::plugin::ModelCallAddress {
                session_id: lash_core::SessionId::from("recorded-response-plan"),
                turn_id: lash_core::TurnId::from("hook-turn"),
                call_id: lash_core::LlmCallId("hook-call".into()),
                attempt: Some(1),
            },
            text_response(RAW),
            &plan,
            states,
        )
        .await?;
    Ok(transforms
        .last()
        .map_or_else(|| RAW.into(), |t| t.value.response.full_text()))
}

#[tokio::test]
async fn recorded_callback_order_survives_reordered_installation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let a = appending_callback("response-a", lash::hook_key!("append"), ":a", &calls);
    let b = appending_callback("response-b", lash::hook_key!("append"), ":b", &calls);
    let recorded = callback_session(vec![a.clone(), b.clone()]);
    let live = callback_session(vec![b, a]);
    assert_eq!(
        replay_callbacks(&recorded, &live, &[]).await.unwrap(),
        format!("{RAW}:a:b")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_unavailable_recorded_callback_refuses_before_any_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let a = appending_callback("response-a", lash::hook_key!("append"), ":a", &calls);
    let b = appending_callback("response-b", lash::hook_key!("append"), ":b", &calls);
    let recorded = callback_session(vec![b.clone(), a]);
    let live = callback_session(vec![
        b,
        appending_callback("response-a", lash::hook_key!("renamed"), ":a", &calls),
    ]);
    let error = replay_callbacks(&recorded, &live, &[])
        .await
        .expect_err("the recorded callback is owed");
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::RecordedCallbackUnavailable
    );
    let Some(lash_core::RuntimeErrorCause::RecordedCallbackUnavailable { refusal }) = error.cause
    else {
        panic!("the missing callback remains typed");
    };
    assert_eq!(refusal.recorded.owner, "response-a");
    assert_eq!(refusal.recorded.key, "assistant_response:append");
    assert!(
        refusal
            .available
            .iter()
            .any(|key| key.owner == "response-a" && key.key == "assistant_response:renamed")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// SAME-KEY-REDEPLOY-FINISHES-DERIVATION: an uncommitted response
/// derivation finishes using the running implementation of its recorded key.
#[tokio::test]
async fn same_key_redeploy_finishes_derivation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = callback_session(vec![appending_callback(
        "response-a",
        lash::hook_key!("append"),
        ":old",
        &calls,
    )]);
    let live = callback_session(vec![appending_callback(
        "response-a",
        lash::hook_key!("append"),
        ":new",
        &calls,
    )]);
    assert_eq!(
        replay_callbacks(&recorded, &live, &[]).await.unwrap(),
        format!("{RAW}:new")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_state_pairs_multiple_callbacks_of_one_plugin_on_cold_replay() {
    let mut spec = lash_core::facade_support::PluginSpec::new();
    for state in ["first", "second"] {
        let key = lash_core::plugin::HookKey::new(state).unwrap();
        spec = spec.with_assistant_stream_finished(
            key,
            Arc::new(move |_| Box::pin(async move { Ok(Some(serde_json::json!(state))) })),
        );
        spec = spec.with_assistant_response(
            key,
            Some(key),
            Arc::new(move |ctx| {
                let state = ctx
                    .stream_state
                    .expect("this callback's recorded stream state");
                let response = text_response(&format!(
                    "{}:{}",
                    ctx.response.full_text(),
                    state.as_str().unwrap()
                ));
                Box::pin(async move {
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        );
    }
    let plugin: Arc<dyn lash_core::plugin::PluginFactory> = Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("paired-responses"),
        spec,
    ));
    let recorded = callback_session(vec![plugin.clone()]);
    let states = recorded
        .finish_assistant_stream(
            &lash_core::plugin::ModelCallAddress {
                session_id: lash_core::SessionId::from("recorded-response-plan"),
                turn_id: lash_core::TurnId::from("hook-turn"),
                call_id: lash_core::LlmCallId("hook-call".into()),
                attempt: Some(1),
            },
            lash_core::plugin::AssistantStreamFinishReason::Complete,
        )
        .await
        .unwrap();
    let states: Vec<lash_core::AssistantStreamHookState> =
        serde_json::from_value(serde_json::to_value(states).unwrap()).unwrap();
    let live = callback_session(vec![plugin]);
    assert_eq!(
        replay_callbacks(&recorded, &live, &states).await.unwrap(),
        format!("{RAW}:first:second")
    );
}

/// STREAM-STATE-FORMAT-REFUSES: all state formats are checked before
/// callbacks; a plugin that reads an older format can finish that derivation.
#[tokio::test]
async fn stream_state_format_refuses_and_readable_older_state_runs() {
    fn paired(
        native: u32,
        readable: &[u32],
        calls: &Arc<AtomicUsize>,
    ) -> Arc<dyn lash_core::plugin::PluginFactory> {
        let mut declaration = lash_core::plugin::PluginDeclaration::initial("stream-format");
        declaration.stream_state_format = lash_core::plugin::FormatVersion::new(native).unwrap();
        declaration.readable_stream_state_formats = readable
            .iter()
            .map(|version| lash_core::plugin::FormatVersion::new(*version).unwrap())
            .collect();
        let calls = Arc::clone(calls);
        let spec = lash_core::facade_support::PluginSpec::new()
            .with_assistant_stream_finished(
                lash::hook_key!("state"),
                Arc::new(|_| Box::pin(async { Ok(Some(serde_json::json!("saved"))) })),
            )
            .with_assistant_response(
                lash::hook_key!("consume"),
                Some(lash::hook_key!("state")),
                Arc::new(move |ctx| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        ctx.stream_state_format.unwrap().get(),
                        1,
                        "the reader sees the stored format, even on a native-format-2 build"
                    );
                    let response = text_response(&format!(
                        "{}:{}",
                        ctx.response.full_text(),
                        ctx.stream_state.unwrap().as_str().unwrap()
                    ));
                    Box::pin(async move {
                        Ok(lash_core::facade_support::AssistantResponseTransform {
                            response,
                            events: Vec::new(),
                        })
                    })
                }),
            );
        Arc::new(StaticPluginFactory::new(declaration, spec))
    }
    let calls = Arc::new(AtomicUsize::new(0));
    // A preceding callback must not run before a later callback's format refusal.
    let first = appending_callback("first", lash::hook_key!("append"), ":first", &calls);
    let recorded = callback_session(vec![first.clone(), paired(1, &[1], &calls)]);
    let states = recorded
        .finish_assistant_stream(
            &lash_core::plugin::ModelCallAddress {
                session_id: lash_core::SessionId::from("recorded-response-plan"),
                turn_id: lash_core::TurnId::from("hook-turn"),
                call_id: lash_core::LlmCallId("hook-call".into()),
                attempt: Some(1),
            },
            lash_core::plugin::AssistantStreamFinishReason::Complete,
        )
        .await
        .unwrap();
    let states: Vec<lash_core::AssistantStreamHookState> =
        serde_json::from_value(serde_json::to_value(states).unwrap()).unwrap();
    assert_eq!(states[0].format.get(), 1);
    let unreadable = callback_session(vec![first.clone(), paired(2, &[2], &calls)]);
    let error = replay_callbacks(&recorded, &unreadable, &states)
        .await
        .unwrap_err();
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::StreamStateFormatUnreadable
    );
    let Some(lash_core::RuntimeErrorCause::StreamStateFormatUnreadable { refusal }) = &error.cause
    else {
        panic!("typed stream-state format refusal");
    };
    assert_eq!(refusal.callback.owner, "stream-format");
    assert_eq!(refusal.callback.key, "assistant_response:consume");
    assert_eq!(refusal.stored.get(), 1);
    assert_eq!(
        refusal.readable.iter().map(|v| v.get()).collect::<Vec<_>>(),
        vec![2]
    );
    let park =
        lash_core::store::ParkReason::of_error(&error).expect("unreadable stream state parks");
    let decoded: lash_core::store::ParkReason =
        serde_json::from_value(serde_json::to_value(&park).unwrap()).unwrap();
    assert_eq!(park, decoded);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let readable = callback_session(vec![first, paired(2, &[1, 2], &calls)]);
    assert_eq!(
        replay_callbacks(&recorded, &readable, &states)
            .await
            .unwrap(),
        format!("{RAW}:first:saved")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
