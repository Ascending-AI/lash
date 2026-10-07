//! A cell's oversized prints and final value are retained before they enter
//! history (FIG-1643): an output too long for history is put as a session
//! attachment, and a refusal to put it is the cell's typed, terminal failure.

/// An output larger than the attachment store will hold is a deterministic
/// refusal: typed, terminal, never a retry.
#[tokio::test]
async fn a_permanent_rlm_retention_refusal_is_typed_and_terminal() {
    let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let attachments = lash_core::facade_support::RuntimeAttachmentStore::ephemeral(
        host.backend().attachment_store(),
    )
    .with_max_attachment_bytes(Some(64));
    let value = serde_json::json!("x".repeat(900));
    let error = super::super::cell_outputs::retain_oversized_value(
        &attachments,
        lash_core::OutputRetentionPolicy {
            inline_limit_bytes: 128,
            witness_bytes: 64,
        },
        &value,
        "law",
    )
    .await
    .expect_err("the output exceeds the attachment limit");
    assert!(
        error.is_terminal(),
        "the deterministic refusal is terminal: {error:?}"
    );
    assert!(
        !error
            .journal_disposition(lash_core::RuntimeEffectKind::LanguageRuntimeValue)
            .is_retryable_derivation()
    );
    assert!(
        error.cause.is_some(),
        "retain the typed attachment-store cause"
    );
}
