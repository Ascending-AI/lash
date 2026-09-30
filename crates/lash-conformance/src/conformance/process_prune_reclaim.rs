//! Process retention leaves independent session history and checkpoint roots alive.
use super::DeploymentViewExt as _;
use super::session_delete_blob_reclaim::{
    SessionDeleteBlobProbe, commit_content_aliased_checkpoint_roots,
};
use lash_sansio::{ProcessId, SessionId};
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "conformance fixture establishes each result"
)]
pub async fn process_prune_preserves_independent_session_checkpoint_roots(
    backend: &str,
    factory: Arc<dyn crate::DeploymentStore>,
    registry: Arc<dyn crate::ProcessRegistry>,
    probe: Arc<dyn SessionDeleteBlobProbe>,
) {
    let process_id = register_process(registry.as_ref()).await;
    let dependent = SessionId::from("prune-independent-dependent");
    let aliased = SessionId::from("prune-independent-aliased");
    let roots = commit_content_aliased_checkpoint_roots(&factory, &dependent, &aliased).await;
    assert!(probe.blob_exists(&roots.aliased_root).await);
    assert!(probe.blob_exists(&roots.dependent_root).await);
    probe.fail_next_blob_delete().await;
    prune_completed_process(registry.as_ref(), &process_id).await;
    probe.clear_blob_delete_failure().await;
    for id in [&dependent, &aliased] {
        assert!(
            factory
                .live_view(id)
                .await
                .expect("read independent session after prune")
                .is_some(),
            "{backend}: process retention must preserve independently admitted sessions"
        );
        assert!(
            !factory
                .is_deleted(id)
                .await
                .expect("read independent deletion state")
        );
    }
    assert!(
        probe.blob_exists(&roots.aliased_root).await,
        "{backend}: content-aliased checkpoint root stays alive"
    );
    assert!(
        probe.blob_exists(&roots.dependent_root).await,
        "{backend}: dependent checkpoint root stays alive"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn register_process(registry: &dyn crate::ProcessRegistry) -> ProcessId {
    registry
        .register_process(crate::ProcessRegistration::new(
            crate::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register the pruned process")
        .id
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn prune_completed_process(registry: &dyn crate::ProcessRegistry, process_id: &ProcessId) {
    let terminal = registry
        .complete_process(
            process_id,
            crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete the process under prune");
    let report = registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            crate::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the terminal process");
    assert_eq!(
        report.pruned_processes, 1,
        "the completed process must be prunable"
    );
}
