use super::*;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

/// The attachment GC fence is a durable, clockless CAS state machine over one
/// digest: `Free -> Condemned -> Deleting`, with `Condemned -> Free` whenever a
/// writer takes the digest back, `Deleting -> Free` when a host abandons or
/// recovers a failed delete, and `Deleting -> (no row)` when the physical
/// delete completes. There is no terminal byte-absence phase: a completed
/// delete retires the row and clears the digest's manifest evidence, and
/// adoption is gated on that evidence rather than on a tombstone.
///
/// Every transition is exercised here rather than in per-backend tests, so a
/// divergence between the in-memory, SQLite, and PostgreSQL implementations of
/// the same protocol is a conformance failure. Authorities that report
/// [`AttachmentGcFence::BestEffort`](crate::AttachmentGcFence::BestEffort)
/// implement no fence and are skipped.
pub(super) async fn session_store_factory_attachment_gc_fence_state_machine(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    if crate::AttachmentRootSet::fence(&*factory) == crate::AttachmentGcFence::BestEffort {
        return;
    }
    let request = session_store_request(
        &SessionId::from("attachment-gc-fence-state-machine"),
        "attachment-gc-fence-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .create_store(&request)
        .await
        .expect("create session store");
    let attachment_id = crate::AttachmentId::parse("c".repeat(64)).expect("valid attachment id");
    let intent = || crate::AttachmentIntent {
        attachment_id: attachment_id.clone(),
        session_id: request.session_id.clone(),
        canonical_uri: format!("lash-attachment://blake3/{attachment_id}"),
        intent_at_epoch_ms: 1,
        owner: None,
    };
    // Nothing is aged out at cutoff 0, so a recorded intent is unambiguously a
    // root and a forgotten one leaves no row at all.
    const ROOT_CUTOFF: u64 = 0;
    let condemn =
        || crate::AttachmentRootSet::condemn_attachment(&*factory, &attachment_id, ROOT_CUTOFF);
    let arm = || crate::AttachmentRootSet::arm_attachment_delete(&*factory, &attachment_id);

    // A recorded intent is a root: `Free -> Condemned` refuses.
    assert!(
        matches!(
            crate::AttachmentManifest::begin_attachment_write(&*store, intent())
                .await
                .expect("first fenced write"),
            crate::AttachmentWriteFence::Granted(_)
        ),
        "a write against a free digest must be granted"
    );
    assert_eq!(
        condemn().await.expect("condemn a rooted digest"),
        crate::AttachmentCondemnation::RootPresent,
        "an uncommitted intent is a root: the condemn CAS must refuse"
    );

    // Drop the root and the digest becomes condemnable — once.
    crate::AttachmentManifest::forget(&*store, &request.session_id, &attachment_id)
        .await
        .expect("forget the ref");
    assert_eq!(
        condemn().await.expect("condemn"),
        crate::AttachmentCondemnation::Condemned,
        "a rootless digest must be condemnable"
    );
    assert_eq!(
        condemn().await.expect("second condemn"),
        crate::AttachmentCondemnation::AlreadyCondemned,
        "a peer sweeper's condemnation is skipped, never waited on"
    );
    crate::AttachmentRootSet::release_attachment_condemnation(&*factory, &attachment_id)
        .await
        .expect("release condemned digest");
    assert_eq!(
        condemn().await.expect("condemn after release"),
        crate::AttachmentCondemnation::Condemned,
        "release must recover a digest abandoned before physical delete"
    );

    // `Condemned -> Free` by writer revoke: the delete can no longer be armed.
    let restoring_intent = intent();
    let restoring_permit =
        match crate::AttachmentManifest::begin_attachment_write(&*store, restoring_intent.clone())
            .await
            .expect("write against a condemned digest")
        {
            crate::AttachmentWriteFence::Granted(permit) => permit,
            crate::AttachmentWriteFence::ReclamationInFlight => {
                panic!("a writer must be able to take a condemned digest back")
            }
        };
    crate::AttachmentManifest::complete_attachment_write(
        &*store,
        &restoring_intent,
        restoring_permit,
    )
    .await
    .expect("settle the successful restoring write");
    assert_eq!(
        arm().await.expect("arm after revocation"),
        crate::AttachmentDeleteArming::Revoked,
        "a revoked condemnation must not arm a delete"
    );

    // `Condemned -> Deleting`: a writer now parks instead of putting bytes into
    // an in-flight delete, and only the release lets it through.
    crate::AttachmentManifest::forget(&*store, &request.session_id, &attachment_id)
        .await
        .expect("forget the ref again");
    assert_eq!(
        condemn().await.expect("re-condemn"),
        crate::AttachmentCondemnation::Condemned
    );
    assert_eq!(
        arm().await.expect("arm"),
        crate::AttachmentDeleteArming::Armed
    );
    assert_eq!(
        arm().await.expect("re-arm an already-deleting digest"),
        crate::AttachmentDeleteArming::Revoked,
        "arming is `Condemned -> Deleting` only; a digest already `Deleting` is not re-armed"
    );
    assert!(
        matches!(
            crate::AttachmentManifest::begin_attachment_write(&*store, intent())
                .await
                .expect("write against an armed digest"),
            crate::AttachmentWriteFence::ReclamationInFlight
        ),
        "a writer must park while the physical delete is in flight"
    );
    assert!(
        !crate::AttachmentManifest::list_all_refs(&*store)
            .await
            .map(|refs| refs.contains(&attachment_id))
            .expect("contains_ref"),
        "a parked writer must record no intent"
    );

    // `Deleting -> Free` is the explicit abandon/recovery path.
    crate::AttachmentRootSet::release_attachment_condemnation(&*factory, &attachment_id)
        .await
        .expect("release");
    assert!(
        matches!(
            crate::AttachmentManifest::begin_attachment_write(&*store, intent())
                .await
                .expect("write after the release"),
            crate::AttachmentWriteFence::Granted(_)
        ),
        "a released digest must grant the next writer immediately"
    );

    // A completed delete retires the condemnation row outright. The fence is
    // back at `Free`, but the digest is no longer adoptable: condemnation
    // cleared its manifest evidence under the same fence, so only a fresh
    // completed write can make it adoptable again.
    crate::AttachmentManifest::forget(&*store, &request.session_id, &attachment_id)
        .await
        .expect("forget the ref before the successful-delete path");
    assert_eq!(
        condemn().await.expect("condemn before successful delete"),
        crate::AttachmentCondemnation::Condemned
    );
    assert_eq!(
        arm().await.expect("arm before successful delete"),
        crate::AttachmentDeleteArming::Armed
    );
    crate::AttachmentRootSet::retire_attachment_condemnation(&*factory, &attachment_id)
        .await
        .expect("record successful delete");
    assert!(
        crate::AttachmentRootSet::list_condemnations(&*factory)
            .await
            .expect("list condemnations after a completed delete")
            .iter()
            .all(|record| record.digest != attachment_id),
        "a completed delete retires the condemnation row"
    );
    crate::AttachmentRootSet::release_attachment_condemnation(&*factory, &attachment_id)
        .await
        .expect("release a retired digest is idempotent");
    let adoption_error = crate::AttachmentManifest::commit_refs(
        &*store,
        &request.session_id,
        std::slice::from_ref(&attachment_id),
    )
    .await
    .expect_err("adoption must refuse a digest with no upload evidence");
    assert!(matches!(
        adoption_error,
        crate::StoreError::UnknownAttachment { ref digest }
            if digest == &attachment_id
    ));
    let restored_intent = intent();
    let crate::AttachmentWriteFence::Granted(restored_permit) =
        crate::AttachmentManifest::begin_attachment_write(&*store, restored_intent.clone())
            .await
            .expect("a retired digest grants the next writer")
    else {
        panic!("a retired digest must not park a writer");
    };
    crate::AttachmentManifest::complete_attachment_write(
        &*store,
        &restored_intent,
        restored_permit,
    )
    .await
    .expect("stamp the restoring upload");
    crate::AttachmentManifest::commit_refs(
        &*store,
        &request.session_id,
        std::slice::from_ref(&attachment_id),
    )
    .await
    .expect("a fresh completed write restores adoptability");
}
