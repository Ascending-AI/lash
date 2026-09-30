use super::*;
use pretty_assertions::assert_eq;

/// A reused start key mints a successor whose referrer is independent of the
/// permanently ended previous process, without admitting synthetic sessions.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes every result"
)]
pub async fn a_same_start_key_successor_after_prune_has_independent_attachment_referrers(
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    _effect_host: Arc<dyn crate::EffectHost>,
) {
    let key = crate::StartKey::for_host("successor-after-prune");
    let start = || {
        process_registry::registration("successor-after-prune").with_start_key(Some(key.clone()))
    };
    let first = registry
        .register_process(start())
        .await
        .expect("register first process");
    let digest = crate::AttachmentId::parse("successor-shared-digest").expect("digest");
    let previous = crate::ArtifactReferrer::ProcessRecord(first.id.clone());
    let write = crate::AttachmentWrite {
        attachment_id: digest.clone(),
        claim: crate::ReferrerClaim::unguarded(previous.clone()).expect("process claim"),
    };
    let crate::AttachmentWriteFence::Granted(permit) = factory
        .begin_attachment_write(&write)
        .await
        .expect("begin first write")
    else {
        panic!("free digest must grant")
    };
    factory
        .complete_attachment_write(&write, permit)
        .await
        .expect("complete first write");
    registry
        .complete_process(
            &first.id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"lifetime":"first"}),
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete first process");
    let report = registry
        .prune_terminal_processes(u64::MAX, None, crate::ProjectionWatermark::NoProjector)
        .await
        .expect("prune first process");
    assert_eq!(report.pruned_processes, 1);
    factory
        .end_attachment_referrer(&previous)
        .await
        .expect("apply attachment cleanup");
    let second = registry
        .register_process_reporting_outcome(start(), &[])
        .await
        .expect("register successor");
    assert_eq!(second.outcome, crate::ProcessRegistrationOutcome::Created);
    assert_ne!(second.record.id, first.id);
    assert!(matches!(
        registry.get_process(&first.id).await,
        Err(crate::PluginError::ProcessNoLongerRetained { .. })
    ));
    let successor = crate::ArtifactReferrer::ProcessRecord(second.record.id.clone());
    let claim = crate::ReferrerClaim::unguarded(successor.clone()).expect("successor claim");
    factory
        .acquire_attachment_refs(&claim, std::slice::from_ref(&digest))
        .await
        .expect("successor acquires independently");
    assert_eq!(
        factory
            .attachment_referrers(&digest)
            .await
            .expect("read successor edge"),
        vec![successor]
    );
    assert!(
        matches!(factory.begin_attachment_write(&write).await, Err(crate::StoreError::ArtifactReferrerEnded { referrer }) if referrer == previous)
    );
    let output = crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
        serde_json::json!({"lifetime":"successor"}),
    ));
    registry
        .complete_process(
            &second.record.id,
            output.clone(),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete successor");
    let record = registry
        .get_process(&second.record.id)
        .await
        .expect("read successor")
        .expect("successor retained");
    assert_eq!(record.status, crate::ProcessStatus::Completed);
    assert_eq!(record.outcome.as_ref(), Some(&output));
}
