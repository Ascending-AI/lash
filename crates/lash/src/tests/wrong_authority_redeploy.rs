//! A deployment opened over existing stores under another Restate authority
//! answers its senders (FIG-4597).
//!
//! A session's first root records the cancellation binding of the authority
//! it ran under. A deployment that later runs over the same stores under
//! another authority presents another binding, and the store refuses it on
//! every attempt: `TurnCancelBindingMismatch`. The refusal is the root's
//! recorded answer, so the root ends with it and its sender reads the typed
//! cause. Retried as an attempt fault, the root paused after its attempt
//! budget and its sender waited on it forever.

use super::*;

const SEED: u64 = 0x4597_0001;
const OTHER_SEED: u64 = 0x4597_0002;

/// How long a root the deployment can never run may take to answer its
/// sender. The bound turns a sender left waiting on a paused root into a
/// failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

fn core_over(double: &lash_restate_test::RestateTestBackend) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

/// A session runs a root under one authority. Another deployment then opens
/// the same stores under another authority, and each send to the session is
/// answered, within a bound, with the typed binding mismatch naming both
/// authorities. No root is left paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_under_another_authority_is_answered_with_the_binding_mismatch() -> Result<()> {
    const ID: &str = "wrong-authority-redeploy";
    let first = restate_double(SEED).await;
    {
        let core = core_over(&first)?;
        core.session(ID).created().await.open().await?;
        core.session(ID)
            .durable()
            .await?
            .send(TurnInput::text("under the first authority"))
            .output()
            .await?;
        first
            .settle_session_drive(&lash_core::SessionId::from(ID))
            .await;
    }
    let stores = Arc::clone(first.engine_stores());
    drop(first);
    let second = lash_restate_test::backend_with(
        OTHER_SEED,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("another deployment over the same stores");
    let core = core_over(&second)?;
    // Twice: the refusal ends the root it met, so the next send is a new
    // root that is refused the same way rather than one queued behind it.
    for send in ["the first send", "the second send"] {
        let answer = tokio::time::timeout(ANSWERS_WITHIN, async {
            core.session(ID)
                .durable()
                .await?
                .send(TurnInput::text("under another authority"))
                .output()
                .await
        })
        .await;
        let Ok(answer) = answer else {
            panic!(
                "{send}: a root refused for its session's cancellation binding answers its \
                 sender: none in {ANSWERS_WITHIN:?}, invocations {:?}",
                invocations(&second)
            );
        };
        let Err(EmbedError::Runtime(error)) = answer else {
            panic!("{send}: the sender reads the runtime refusal: {answer:?}");
        };
        assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::TurnCancelBindingMismatch,
            "{send}: {error:?}"
        );
        let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
            panic!("{send}: the refusal carries its typed cause: {error:?}");
        };
        let lash_core::store::StoreRefusal::TurnCancelBindingMismatch {
            session_id,
            expected,
            presented,
        } = &**refusal
        else {
            panic!("{send}: the cause is the binding mismatch: {refusal:?}");
        };
        assert_eq!(session_id.as_str(), ID, "{send}");
        assert_ne!(
            expected, presented,
            "{send}: the cause names the admitted and the presented authority"
        );
        assert!(
            error.is_terminal() && !error.is_retryable(),
            "{send}: {error:?}"
        );
    }
    let paused = invocations(&second)
        .into_iter()
        .filter(|invocation| invocation.contains(" paused "))
        .collect::<Vec<_>>();
    assert!(
        paused.is_empty(),
        "no refused root is left paused: {paused:?}"
    );
    Ok(())
}
