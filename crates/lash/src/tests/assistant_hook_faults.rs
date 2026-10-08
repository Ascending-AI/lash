//! An assistant-response hook's fault on the durable turn path (FIG-3726):
//! a live fault is its attempt's, so the phase runs again and nothing of
//! the faulted attempt is recorded; a
//! deterministic failure is the turn's own outcome, met once.

use super::*;
use crate::TurnInput;

/// How long a turn whose hook fails may take to answer.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// A core whose assistant-response hook answers its `n`th call with
/// `answer(n)`, and the model's call count.
fn hooked_core(
    answer: impl Fn(usize) -> std::result::Result<(), lash_core::PluginError> + Send + Sync + 'static,
    hook_calls: &Arc<AtomicUsize>,
    model_calls: &Arc<AtomicUsize>,
    backend: lash_core::Backend,
) -> Result<LashCore> {
    let answer = Arc::new(answer);
    let hook_calls = Arc::clone(hook_calls);
    let hook = StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("hook-faults"),
        lash_core::facade_support::PluginSpec::new().with_assistant_response(
            crate::hook_key!("suffix"),
            None,
            Arc::new(move |ctx| {
                let call = hook_calls.fetch_add(1, Ordering::SeqCst) + 1;
                let answered = answer(call);
                let response = text_response(&format!("{} [hooked]", ctx.response.full_text()));
                Box::pin(async move {
                    answered?;
                    Ok(lash_core::facade_support::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        ),
    );
    let model_calls = Arc::clone(model_calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("hook-faults")
        .complete(move |_| {
            model_calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(text_response("the paid completion")) }
        })
        .build()
        .into_handle();
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .plugin(Arc::new(hook))
        .build(crate::testing::runtime_lease_owner())
}

/// The committed transcript of `id`'s session: role and text per message.
async fn transcript(core: &LashCore, id: &str) -> Result<Vec<(String, String)>> {
    let store = lash_core::runtime::live_session_view(
        &core.store_factory,
        &crate::SessionId::fixture(id.to_string()),
    )
    .await?
    .expect("the session's store");
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the committed head")
    .state;
    Ok(state
        .read_model()
        .messages
        .iter()
        .map(|message| {
            (
                crate::turn::message_role(message).to_owned(),
                crate::turn::message_text(message),
            )
        })
        .collect())
}

/// A hook whose first run meets a live fault (an invocation failure, which
/// the runtime marks a retryable response derivation) and whose next run
/// answers: the turn answers the hooked response once, the hook ran again,
/// and the committed transcript holds the hooked answer alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assistant_hook_session_fault_retries_the_step_without_recording_it() -> Result<()> {
    const ID: &str = "hook-live-fault";
    let (hook_calls, model_calls) = (Arc::default(), Arc::default());
    let core = hooked_core(
        |call| {
            if call == 1 {
                Err(lash_core::PluginError::Invoke(
                    "fig3726: the session lease was lost mid-derivation".into(),
                ))
            } else {
                Ok(())
            }
        },
        &hook_calls,
        &model_calls,
        sqlite_memory_store_backend().await,
    )?;
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let output = tokio::time::timeout(
        ANSWERS_WITHIN,
        session.send(TurnInput::text("hello")).output(),
    )
    .await
    .expect("the turn answers")?;
    assert_eq!(
        output.assistant_message(),
        Some("the paid completion [hooked]"),
        "{:?}",
        output.result.outcome
    );
    assert_eq!(
        hook_calls.load(Ordering::SeqCst),
        2,
        "the faulted step ran again"
    );
    let transcript = transcript(&core, ID).await?;
    assert_eq!(
        transcript,
        vec![
            ("user".to_owned(), "hello".to_owned()),
            (
                "assistant".to_owned(),
                "the paid completion [hooked]".to_owned()
            ),
        ],
        "the faulted attempt recorded nothing"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// A hook that fails the same way on every run: its failure is the turn's
/// outcome. The turn answers within the bound, not answered, and the hook
/// is not run again for a failure no retry changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deterministic_assistant_hook_failure_is_the_steps_recorded_outcome() -> Result<()> {
    const ID: &str = "hook-deterministic-failure";
    let (hook_calls, model_calls) = (Arc::default(), Arc::default());
    let core = hooked_core(
        |_| {
            Err(lash_core::PluginError::Session(
                "fig3726: deterministic failure".into(),
            ))
        },
        &hook_calls,
        &model_calls,
        sqlite_memory_store_backend().await,
    )?;
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let answered = tokio::time::timeout(
        ANSWERS_WITHIN,
        session.send(TurnInput::text("hello")).output(),
    )
    .await;
    let hook_runs = hook_calls.load(Ordering::SeqCst);
    let answered = answered.unwrap_or_else(|_| {
        panic!("the turn answers its hook's failure: the hook ran {hook_runs} times")
    });
    match &answered {
        Ok(output) => assert!(
            !output.is_success(),
            "a failed hook does not answer: {:?}",
            output.result.outcome
        ),
        Err(error) => assert!(
            error.to_string().contains("deterministic failure"),
            "the turn's error is the hook's: {error:?}"
        ),
    }
    assert_eq!(hook_runs, 1, "a deterministic failure is not retried");
    assert_eq!(model_calls.load(Ordering::SeqCst), 1);
    drop(session);
    core.shutdown().await?;
    Ok(())
}
