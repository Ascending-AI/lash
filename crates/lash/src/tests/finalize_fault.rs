//! A recorded after-turn refusal ends the Run with its typed failure.
//!
//! After-turn decisions and failures are recorded once under K10/L19
//! (ADR 0078). Replay invokes no completed callback. These laws retain the
//! terminal refusal and equal-id redrive contract on the Restate double.

use super::*;

const SEED: u64 = 0x3897_f1a1;

/// A core over the double's engine, and the double, which must outlive it:
/// a core built over `double.lash_backend()` does not hold it (FIG-3723).
struct Fixture {
    core: LashCore,
    double: lash_restate_test::RestateTestBackend,
    provider_calls: Arc<AtomicUsize>,
}

impl Fixture {
    async fn with_plugins(plugins: Vec<Arc<dyn PluginFactory>>) -> Result<Self> {
        let double = restate_double(SEED).await;
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let builder = LashCore::standard_builder(double.lash_backend());
        let builder = plugins
            .into_iter()
            .fold(builder, |builder, plugin| builder.plugin(plugin));
        let core = explicit_ephemeral_facets(builder)
            .serve_test_llm_profile(
                counting_provider(Arc::clone(&provider_calls)),
                mock_llm_profile_spec(),
            )
            .build(crate::testing::runtime_lease_owner())?;
        Ok(Self {
            core,
            double,
            provider_calls,
        })
    }

    /// The attempts the engine made of `turn`'s `LashTurn` run.
    fn turn_attempts(&self, session: &str, turn: &str) -> u32 {
        let key = lash_restate::turn_workflow_key(
            &lash_core::SessionId::fixture(session),
            &lash_core::TurnId::fixture(turn),
        );
        self.double
            .server()
            .invocations()
            .into_iter()
            .filter(|view| {
                view.target.starts_with("LashTurn") && view.target.ends_with(&format!("/{key}/run"))
            })
            .map(|view| view.attempts)
            .sum()
    }
}

fn counting_provider(calls: Arc<AtomicUsize>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("finalize-fault")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

fn plugin(id: &'static str, spec: lash_core::facade_support::PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(id),
        spec,
    ))
}

/// An `after_turn` refusal that counts every invocation.
fn failing_after_turn(
    calls: Arc<AtomicUsize>,
    fault: fn() -> lash_core::PluginError,
) -> Arc<dyn PluginFactory> {
    plugin(
        "finalize-fault-after-turn",
        lash_core::facade_support::PluginSpec::new().with_after_turn(
            crate::hook_key!("after-turn-1"),
            Arc::new(move |_| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(fault())
                })
            }),
        ),
    )
}

/// A plugin's refusal over the turn it finalizes.
fn finalize_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Invoke("the finalize hook refuses this turn".to_string())
}

/// A refusal under a plugin-minted code: the plugin classes it
/// [`TurnFailureCause::Outcome`](lash_core::TurnFailureCause::Outcome).
fn minted_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Runtime(lash_core::RuntimeError::foreign(
        "finalize-plugin.refused",
        lash_core::TurnFailureCause::Outcome,
        "the finalize hook refuses this turn",
    ))
}

/// A refusal in the finalize hook is an outcome: the run ends terminal
/// after one attempt with the typed finalize failure, and a redrive of the
/// same turn id answers that failure without running the turn again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finalize_refusal_ends_the_run_terminal_with_its_typed_failure() -> Result<()> {
    refused_terminal("finalize-refusal", finalize_refusal).await
}

/// The same for a refusal the plugin classes itself: a plugin-minted code it
/// marks an outcome ends the run terminal, never retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_minted_finalize_refusal_ends_the_run_terminal_with_its_typed_failure() -> Result<()> {
    refused_terminal("finalize-minted-refusal", minted_refusal).await
}

async fn refused_terminal(
    session_id: &'static str,
    fault: fn() -> lash_core::PluginError,
) -> Result<()> {
    const TURN: &str = "finalize-refused";
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let fixture =
        Fixture::with_plugins(vec![failing_after_turn(Arc::clone(&finalize_calls), fault)]).await?;
    let session = fixture
        .core
        .session(session_id)
        .created()
        .await
        .open()
        .await?;

    let refused = session
        .send(TurnInput::text("finalize refuses"))
        .id(TURN)
        .output()
        .await
        .expect_err("a finalize refusal ends the run terminal");
    assert_finalize_refusal(&refused);
    assert_eq!(finalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.turn_attempts(session_id, TURN),
        1,
        "a refusal is never retried"
    );

    let redriven = session
        .send(TurnInput::text("finalize refuses"))
        .id(TURN)
        .output()
        .await
        .expect_err("the redrive answers the recorded refusal");
    assert_finalize_refusal(&redriven);
    assert_eq!(
        finalize_calls.load(Ordering::SeqCst),
        1,
        "the redrive never runs the turn again"
    );
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.turn_attempts(session_id, TURN), 1);
    Ok(())
}

fn assert_finalize_refusal(error: &EmbedError) {
    let EmbedError::Runtime(runtime) = error else {
        panic!("the refusal is the typed runtime error: {error:?}");
    };
    assert_eq!(
        runtime.code,
        lash_core::RuntimeErrorCode::PluginFinalizeTurn,
        "the refusal carries the finalize failure: {runtime:?}"
    );
    assert!(
        runtime
            .message
            .contains("the finalize hook refuses this turn"),
        "{runtime:?}"
    );
}
