//! FIG-4390: a run's response phase plan is recorded with its paid
//! completion, so adding or removing an assistant-response hook between a
//! run's first attempt and its replay never changes the response the run
//! serves (ADR 0105 §1).
//!
//! Each law executes one run on the Restate server double. Its first attempt
//! journals the LLM call's phases and dies before the step after them; the
//! core that ran it is dropped as it dies, and a second core over the same
//! stores, with the other hook set, installs its `SessionShifts` and replays
//! the run:
//!
//! - **removed**: the first core had a response hook, so the first attempt
//!   journaled phase 2's derived response. The replay, with no hook
//!   installed, serves that derived response from the journal.
//! - **added**: the first core had none, so the first attempt journaled no
//!   phase 2. The replay, with a hook installed, serves the raw completion
//!   and never runs the hook.
//!
//! Each runs with and without forced replay (every await suspends and
//! replays the journal from the start), over SQLite memory, SQLite file and
//! PostgreSQL. The PostgreSQL legs are ignored in ordinary runs and require
//! `LASH_POSTGRES_DATABASE_URL` when selected with `--include-ignored`
//! inside a PostgreSQL gate.
//!
//! The `live_*` laws run the same run on a live `restate-server`, over the
//! live backend's SQLite memory store set: the first attempt's death is the
//! deployment dying at the checkpoint's journal frame, and the deployment
//! that comes back serves the second core. The `recorded-runs` suite
//! of `scripts/restate-suites.toml` runs them on its live and replay legs.

use super::*;

/// What the provider answers every call with.
const RAW: &str = "the raw completion";

fn callback_session(
    mut plugins: Vec<Arc<dyn lash_core::plugin::PluginFactory>>,
) -> Arc<lash_core::facade_support::PluginSession> {
    plugins.push(Arc::new(
        lash_protocol_standard::StandardProtocolPluginFactory::new(),
    ));
    lash_core::facade_support::PluginHost::new(plugins)
        .build_session(lash_core::plugin::PluginSessionRequest::creation(
            "recorded-response-plan",
            Default::default(),
        ))
        .expect("materialize the callback registry")
}

fn appending_callback(
    plugin: &'static str,
    revision: u32,
    suffix: &'static str,
    calls: &Arc<AtomicUsize>,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    let calls = Arc::clone(calls);
    let mut declaration = lash_core::plugin::PluginDeclaration::initial(plugin);
    declaration.behavior_revision = lash_core::plugin::BehaviorRevision::new(revision).unwrap();
    Arc::new(StaticPluginFactory::new(
        declaration,
        lash_core::facade_support::PluginSpec::new().with_assistant_response(
            crate::hook_key!("append"),
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
) -> std::result::Result<String, lash_core::PluginError> {
    let plan: lash_core::AssistantResponsePlan =
        serde_json::from_value(serde_json::to_value(recorded.assistant_response_plan()).unwrap())
            .unwrap();
    let transforms = live
        .transform_assistant_response(
            &lash_core::SessionId::fixture("recorded-response-plan"),
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
    let a = appending_callback("response-a", 1, ":a", &calls);
    let b = appending_callback("response-b", 1, ":b", &calls);
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
    let a = appending_callback("response-a", 1, ":a", &calls);
    let b = appending_callback("response-b", 1, ":b", &calls);
    let recorded = callback_session(vec![b.clone(), a]);
    let live = callback_session(vec![b]);
    let error = replay_callbacks(&recorded, &live, &[])
        .await
        .expect_err("the recorded callback is owed");
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginRevisionUnavailable
    );
    let Some(lash_core::RuntimeErrorCause::PluginExecution { refusal }) = error.cause else {
        panic!("the missing callback remains typed");
    };
    assert_eq!(
        refusal.callback.as_ref().unwrap().owner.plugin,
        "response-a"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_recorded_callback_revision_never_runs_a_substitute() {
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded = callback_session(vec![appending_callback("response-a", 1, ":old", &calls)]);
    let live = callback_session(vec![appending_callback("response-a", 2, ":new", &calls)]);
    let error = replay_callbacks(&recorded, &live, &[])
        .await
        .expect_err("revision one is owed");
    let error = lash_core::RuntimeEffectControllerError::from(error).into_runtime_error();
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginRevisionUnavailable
    );
    let Some(lash_core::RuntimeErrorCause::PluginExecution { refusal }) = error.cause else {
        panic!("the revision refusal remains typed");
    };
    let callback = refusal.callback.as_ref().unwrap();
    assert_eq!(callback.key, "assistant_response:append");
    assert_eq!(callback.owner.behavior_revision.get(), 1);
    assert_eq!(
        refusal
            .available
            .iter()
            .find(|revision| revision.plugin == "response-a")
            .unwrap()
            .behavior_revision
            .get(),
        2
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
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
            &lash_core::SessionId::fixture("recorded-response-plan"),
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
