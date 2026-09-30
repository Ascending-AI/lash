#![expect(
    clippy::expect_used,
    reason = "fixture claims use known supported referrer kinds"
)]
//! Evidence-backed session edge and independent process pending write.
use super::RuntimeStore;
use lash_core::store::StoreError;
use lash_core::{
    ArtifactReferrer, AttachmentWrite, AttachmentWriteFence, ProcessId, ReferrerClaim, SessionId,
};
pub(crate) async fn seed_differential_attachment_rows(
    store: &dyn RuntimeStore,
    session_id: &SessionId,
) -> Result<(), StoreError> {
    let completed = AttachmentWrite {
        attachment_id: super::differential_attachment_id(),
        claim: ReferrerClaim::unguarded(ArtifactReferrer::Session(session_id.clone()))
            .expect("session claim"),
    };
    let AttachmentWriteFence::Granted(permit) = store.begin_attachment_write(&completed).await?
    else {
        panic!("free digest must grant")
    };
    store.complete_attachment_write(&completed, permit).await?;
    let pending = AttachmentWrite {
        attachment_id: super::differential_process_attachment_id(),
        claim: ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(ProcessId::fixture(
            session_id.as_str(),
        )))
        .expect("process claim"),
    };
    let AttachmentWriteFence::Granted(_) = store.begin_attachment_write(&pending).await? else {
        panic!("free digest must grant")
    };
    Ok(())
}
