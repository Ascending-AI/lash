//! Durable attachment liveness, write permits, and shared referrer fences.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish every result before inspecting it"
)]
use super::attachment_adoption::{
    AttachmentBytesFactory, create, image_meta, record_completed_write,
};
use lash_core::facade_support::{SessionAttachmentStore, SystemClock};
use lash_core::*;
use std::sync::Arc;

pub type InsertAttachmentEdge = Arc<
    dyn Fn(
            AttachmentId,
            String,
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StoreError>> + Send>>
        + Send
        + Sync,
>;

pub struct AttachmentReferrerHandles {
    pub factory: Arc<dyn DeploymentStore>,
    pub cleanup: Arc<dyn store::ArtifactCleanupLedger>,
    pub bytes: AttachmentBytesFactory,
    pub insert_edge: InsertAttachmentEdge,
}

pub(super) fn claim(referrer: ArtifactReferrer) -> ReferrerClaim {
    match &referrer {
        ArtifactReferrer::Upload(_) => ReferrerClaim::guarded(
            referrer,
            ArtifactCleanupPlan::AwaitUploadExpiry {
                expires_at_ms: 1000,
            },
        )
        .unwrap(),
        ArtifactReferrer::Execution(_) => {
            ReferrerClaim::guarded(referrer, ArtifactCleanupPlan::AwaitJournal).unwrap()
        }
        _ => ReferrerClaim::unguarded(referrer).unwrap(),
    }
}

pub(super) fn write(id: &AttachmentId, referrer: ArtifactReferrer) -> AttachmentWrite {
    AttachmentWrite {
        attachment_id: id.clone(),
        claim: claim(referrer),
    }
}

pub(super) fn execution(session: &SessionId, name: &str) -> ArtifactReferrer {
    ArtifactReferrer::Execution(
        ExecutionScope::turn(session.clone(), name)
            .journal_identity()
            .unwrap(),
    )
}

pub(super) async fn permit(
    store: &dyn AttachmentReferrers,
    write: &AttachmentWrite,
) -> AttachmentWritePermit {
    let AttachmentWriteFence::Granted(permit) = store.begin_attachment_write(write).await.unwrap()
    else {
        panic!("free digest refused")
    };
    permit
}

pub async fn ended_process_record_refuses_attachment_writes_and_acquisitions(
    h: AttachmentReferrerHandles,
) {
    let store = create(&h.factory, "end-laws").await;
    let pass = h.factory.begin_attachment_sweep().await.unwrap();
    let session = SessionId::from("end-laws");
    for (n, referrer) in [
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("ended-process")),
        ArtifactReferrer::Upload(UploadReferrerId::mint(session.clone())),
        ArtifactReferrer::Session(session.clone()),
        execution(&session, "ended-execution"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = AttachmentId::parse(format!("ended-{n}")).unwrap();
        assert_eq!(
            h.factory.condemn_attachment(&id, &pass).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
        let write = write(&id, referrer.clone());
        let pending = permit(store.as_ref(), &write).await;
        assert!(
            matches!(h.factory.list_condemnations().await.unwrap().iter().find(|row| row.digest == id).unwrap().provenance, AttachmentCondemnationProvenance::RestoringWrite { referrer: ref r } if r == &referrer)
        );
        store.end_attachment_referrer(&referrer).await.unwrap();
        store.end_attachment_referrer(&referrer).await.unwrap();
        assert!(store.attachment_referrers(&id).await.unwrap().is_empty());
        assert!(matches!(
            store.complete_attachment_write(&write, pending).await,
            Err(StoreError::StaleWritePermit { .. })
        ));
        assert!(
            matches!(store.begin_attachment_write(&write).await, Err(StoreError::ArtifactReferrerEnded { referrer: r }) if r == referrer)
        );
        assert!(
            matches!(store.acquire_attachment_refs(&write.claim, std::slice::from_ref(&id)).await, Err(StoreError::ArtifactReferrerEnded { referrer: r }) if r == referrer)
        );
        assert!(matches!(
            h.factory
                .list_condemnations()
                .await
                .unwrap()
                .iter()
                .find(|row| row.digest == id)
                .unwrap()
                .provenance,
            AttachmentCondemnationProvenance::SweepOwned
        ));
    }
    let refused = write(
        &AttachmentId::parse("invalid-kind").unwrap(),
        ArtifactReferrer::HostPin(HostArtifactPin::mint()),
    );
    assert!(matches!(
        store.begin_attachment_write(&refused).await,
        Err(StoreError::ReferrerKindRefused {
            store: "attachment",
            ..
        })
    ));
}

pub async fn upload_staging_identities_are_distinct_guarded_and_fenced_independently(
    h: AttachmentReferrerHandles,
) {
    let store = create(&h.factory, "uploads").await;
    let clock = Arc::new(lash_core::testing::TestClock::new(100));
    let bytes = (h.bytes)();
    let facade = SessionAttachmentStore::new_with_clock(
        bytes,
        store.clone(),
        RuntimeOwner::Session("uploads".into()),
        clock,
    )
    .with_upload_expiry_ms(1000);
    let first = facade.put(vec![1], image_meta()).await.unwrap();
    let second = facade.put(vec![2], image_meta()).await.unwrap();
    let a = store.attachment_referrers(&first.id).await.unwrap();
    let b = store.attachment_referrers(&second.id).await.unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_ne!(a, b);
    for referrer in [&a[0], &b[0]] {
        assert!(matches!(referrer, ArtifactReferrer::Upload(_)));
        let cleanup = ArtifactCleanup {
            referrer: referrer.clone(),
            plan: ArtifactCleanupPlan::AwaitUploadExpiry {
                expires_at_ms: 9999,
            },
            gate: None,
        };
        let id = h.cleanup.arm_cleanup(&cleanup, 100).await.unwrap();
        let recorded = h.cleanup.load_cleanup(&id).await.unwrap().unwrap();
        assert_eq!(
            recorded.plan,
            ArtifactCleanupPlan::AwaitUploadExpiry {
                expires_at_ms: 1100
            }
        );
    }
    store.end_attachment_referrer(&a[0]).await.unwrap();
    assert!(
        store
            .attachment_referrers(&first.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.attachment_referrers(&second.id).await.unwrap(), b);
    let third = facade.put(vec![3], image_meta()).await.unwrap();
    assert_ne!(store.attachment_referrers(&third.id).await.unwrap(), a);
    for (kind, key, corrupt) in [
        ("upload", "[\"uploads\",\"upload:v1:BAD\"]", true),
        ("synthetic_next", "future", false),
    ] {
        let id = AttachmentId::parse(format!("malformed-{kind}")).unwrap();
        (h.insert_edge)(id.clone(), kind.into(), key.into())
            .await
            .unwrap();
        let error = store.attachment_referrers(&id).await.unwrap_err();
        if corrupt {
            assert!(matches!(error, StoreError::StoredDataCorrupt { .. }));
        } else {
            assert!(matches!(
                error,
                StoreError::Incompatible {
                    refusal: compat::CompatRefusal::UnknownVocabulary { .. }
                }
            ));
        }
    }
    assert!(
        (h.insert_edge)(
            AttachmentId::parse("empty-id").unwrap(),
            "upload".into(),
            String::new()
        )
        .await
        .is_err()
    );
}

fn state(session: &str) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: session.into(),
        ..RuntimeSessionState::new(SessionPolicy::new(TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    state
}

pub async fn commit_and_enqueue_acquire_session_edges_all_or_nothing(h: AttachmentReferrerHandles) {
    let producer = create(&h.factory, "source").await;
    let id = AttachmentId::parse("atomic-evidenced").unwrap();
    let source = ArtifactReferrer::ProcessRecord(ProcessId::fixture("atomic-source"));
    record_completed_write(&producer, &write(&id, source)).await;
    let receiver = create(&h.factory, "receiver").await;
    let snapshot = |head: Option<store::SessionHeadMeta>| {
        head.map(|head| {
            (
                head.schema_version,
                head.session_id,
                head.head_revision,
                head.leaf_node_id,
                head.checkpoint_ref,
                head.current_frame_node_id,
                serde_json::to_value(head.config).unwrap(),
                serde_json::to_value(head.pending_follow_on).unwrap(),
            )
        })
    };
    let before = receiver
        .load_session_head_meta(&SessionId::from("receiver"))
        .await
        .unwrap();
    let absent = AttachmentId::parse("atomic-z-absent").unwrap();
    let current = state("receiver");
    let commit = RuntimeCommit::persisted_state_for_test(&current, &[])
        .with_committed_attachments([id.clone(), absent.clone()]);
    assert!(matches!(
        receiver.commit_runtime_state(commit).await,
        Err(StoreError::UnknownAttachment { .. })
    ));
    assert_eq!(
        snapshot(
            receiver
                .load_session_head_meta(&SessionId::from("receiver"))
                .await
                .unwrap()
        ),
        snapshot(before)
    );
    let session_referrer = ArtifactReferrer::Session("receiver".into());
    assert!(
        !receiver
            .attachment_referrers(&id)
            .await
            .unwrap()
            .contains(&session_referrer)
    );
    let commit = RuntimeCommit::persisted_state_for_test(&current, &[])
        .with_committed_attachments([id.clone()]);
    receiver.commit_runtime_state(commit).await.unwrap();
    assert!(
        receiver
            .attachment_referrers(&id)
            .await
            .unwrap()
            .contains(&session_referrer)
    );
    create(&h.factory, "enqueue-receiver").await;
    let reference = |id| AttachmentRef {
        id,
        byte_len: 1,
        media_type: MediaType::parse("image/png").unwrap(),
        type_metadata: None,
        label: None,
    };
    let draft = |key: &str, ids: Vec<AttachmentId>| {
        PendingTurnInputDraft::new(
            "enqueue-receiver",
            TurnInputIngress::NextTurn,
            TurnInput::items(
                ids.into_iter()
                    .map(|id| InputItem::attachment(AttachmentSource::stored(reference(id)))),
            ),
        )
        .with_source_key(key)
    };
    let bad = PendingTurnInputBatch::one(draft("bad", vec![id.clone(), absent]));
    assert!(matches!(
        receiver.enqueue_pending_turn_inputs(bad).await,
        Err(StoreError::UnknownAttachment { .. })
    ));
    let enqueue_referrer = ArtifactReferrer::Session("enqueue-receiver".into());
    assert!(
        !receiver
            .attachment_referrers(&id)
            .await
            .unwrap()
            .contains(&enqueue_referrer)
    );
    assert!(
        receiver
            .list_pending_turn_inputs(&SessionId::from("enqueue-receiver"))
            .await
            .unwrap()
            .is_empty()
    );
    receiver
        .enqueue_pending_turn_inputs(PendingTurnInputBatch::one(draft(
            "good",
            vec![id.clone(), id.clone()],
        )))
        .await
        .unwrap();
    assert!(
        receiver
            .attachment_referrers(&id)
            .await
            .unwrap()
            .contains(&enqueue_referrer)
    );
    assert_eq!(
        receiver
            .list_pending_turn_inputs(&SessionId::from("enqueue-receiver"))
            .await
            .unwrap()
            .len(),
        1
    );
}

pub async fn session_referrer_waits_for_graph_retirement(h: AttachmentReferrerHandles) {
    let session = SessionId::from("retained-parent");
    let store = create(&h.factory, session.as_str()).await;
    assert_eq!(
        store.session_referrer_state(&session).await.unwrap(),
        SessionReferrerState::Live
    );
    let bytes = (h.bytes)();
    let facade =
        SessionAttachmentStore::new(bytes, store.clone(), RuntimeOwner::Session(session.clone()));
    let reference = facade.put(vec![4], image_meta()).await.unwrap();
    let mut current = state(session.as_str());
    current.session_graph.append_message(Message {
        id: "retained-message".into(),
        role: MessageRole::User,
        origin: None,
        parts: Arc::new(vec![Part::attachment_part(
            "retained-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                source: AttachmentSource::stored(reference.clone()),
            }),
        )]),
    });
    let commit = RuntimeCommit::persisted_state_for_test(&current, &[])
        .with_committed_attachments([reference.id.clone()]);
    let receipt = store.commit_runtime_state(commit).await.unwrap();
    let node = receipt.committed_leaf_node_id.unwrap();
    h.factory
        .fork_session(&ForkSessionRequest {
            session_id: "retained-child".into(),
            node_id: node,
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: SessionPolicy::new(TurnBudget::Unbounded),
        })
        .await
        .unwrap();
    let referrer = ArtifactReferrer::Session(session.clone());
    store
        .forget_attachment_ref(&referrer, &reference.id)
        .await
        .unwrap();
    assert!(
        store
            .attachment_referrers(&reference.id)
            .await
            .unwrap()
            .contains(&referrer)
    );
    h.factory.delete_session(&session).await.unwrap();
    assert_eq!(
        store.session_referrer_state(&session).await.unwrap(),
        SessionReferrerState::DeletedRetained
    );
    let cleanup = ArtifactCleanup {
        referrer: referrer.clone(),
        plan: ArtifactCleanupPlan::AwaitSessionGraphRetired,
        gate: None,
    };
    let claims = h
        .cleanup
        .claim_due(
            SystemClock.timestamp_ms().saturating_add(60_000),
            1_000,
            std::num::NonZeroUsize::new(100).unwrap(),
        )
        .await
        .unwrap();
    let claimed = claims
        .into_iter()
        .find(|claim| {
            claim.key.as_ref().is_ok_and(|key| {
                key == &store::ObligationKey::ArtifactCleanup {
                    referrer: referrer.clone(),
                }
            })
        })
        .expect("session deletion arms its cleanup without another producer");
    assert_eq!(
        h.cleanup.load_cleanup(&claimed.id).await.unwrap().unwrap(),
        cleanup
    );
    store
        .forget_attachment_ref(&referrer, &reference.id)
        .await
        .unwrap();
    assert!(
        store
            .attachment_referrers(&reference.id)
            .await
            .unwrap()
            .contains(&referrer)
    );
    h.factory
        .delete_session(&SessionId::from("retained-child"))
        .await
        .unwrap();
    assert_eq!(
        store.session_referrer_state(&session).await.unwrap(),
        SessionReferrerState::DeletedRetired
    );
    store.end_attachment_referrer(&referrer).await.unwrap();
    assert!(
        !store
            .attachment_referrers(&reference.id)
            .await
            .unwrap()
            .contains(&referrer)
    );
    assert_eq!(
        store
            .session_referrer_state(&SessionId::from("absent"))
            .await
            .unwrap(),
        SessionReferrerState::Absent
    );
}

pub async fn condemnation_needs_no_edge_and_no_pending_write(h: AttachmentReferrerHandles) {
    let store = create(&h.factory, "condemn-laws").await;
    let id = AttachmentId::parse("pending-root").unwrap();
    let source = ArtifactReferrer::ProcessRecord(ProcessId::fixture("pending-source"));
    let attempt = write(&id, source.clone());
    let first = permit(store.as_ref(), &attempt).await;
    let second = permit(store.as_ref(), &attempt).await;
    let pass = h.factory.begin_attachment_sweep().await.unwrap();
    store.forget_attachment_ref(&source, &id).await.unwrap();
    assert!(store.attachment_referrers(&id).await.unwrap().is_empty());
    assert_eq!(
        h.factory.condemn_attachment(&id, &pass).await.unwrap(),
        AttachmentCondemnation::RootPresent
    );
    store
        .complete_attachment_write(&attempt, first)
        .await
        .unwrap();
    store
        .abort_attachment_write(&attempt, second)
        .await
        .unwrap();
    assert_eq!(
        h.factory.condemn_attachment(&id, &pass).await.unwrap(),
        AttachmentCondemnation::Condemned
    );
    let receiving = claim(ArtifactReferrer::Session("condemn-laws".into()));
    assert!(matches!(
        store
            .acquire_attachment_refs(&receiving, std::slice::from_ref(&id))
            .await,
        Err(StoreError::UnknownAttachment { .. })
    ));
    let restoring = write(
        &id,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("restoring")),
    );
    let restoration = permit(store.as_ref(), &restoring).await;
    let peer = write(
        &id,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("peer")),
    );
    assert!(matches!(
        store.begin_attachment_write(&peer).await.unwrap(),
        AttachmentWriteFence::ReclamationInFlight
    ));
    store
        .abort_attachment_write(&restoring, restoration)
        .await
        .unwrap();
    assert_eq!(
        h.factory.arm_attachment_delete(&id, &pass).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    h.factory
        .settle_attachment_condemnation(&id, &pass, AttachmentCondemnationSettlement::Deleted)
        .await
        .unwrap();
    assert!(matches!(
        store
            .acquire_attachment_refs(&receiving, std::slice::from_ref(&id))
            .await,
        Err(StoreError::UnknownAttachment { .. })
    ));
    record_completed_write(&store, &peer).await;
    store
        .acquire_attachment_refs(&receiving, std::slice::from_ref(&id))
        .await
        .unwrap();
    assert_eq!(
        h.factory.condemn_attachment(&id, &pass).await.unwrap(),
        AttachmentCondemnation::RootPresent
    );
    let superseded_id = AttachmentId::parse("superseded-condemnation").unwrap();
    assert_eq!(
        h.factory
            .condemn_attachment(&superseded_id, &pass)
            .await
            .unwrap(),
        AttachmentCondemnation::Condemned
    );
    let restoring = write(
        &superseded_id,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("superseded-writer")),
    );
    let permit = permit(store.as_ref(), &restoring).await;
    let peer = ArtifactReferrer::ProcessRecord(ProcessId::fixture("superseding-root"));
    (h.insert_edge)(
        superseded_id.clone(),
        peer.kind().as_str().into(),
        peer.canonical_id(),
    )
    .await
    .unwrap();
    store
        .abort_attachment_write(&restoring, permit)
        .await
        .unwrap();
    assert!(
        !h.factory
            .list_condemnations()
            .await
            .unwrap()
            .iter()
            .any(|row| row.digest == superseded_id)
    );
    assert_eq!(
        store.attachment_referrers(&superseded_id).await.unwrap(),
        vec![peer]
    );
    store
        .abort_attachment_write(&restoring, permit)
        .await
        .unwrap();
    assert!(matches!(
        store.complete_attachment_write(&restoring, permit).await,
        Err(StoreError::StaleWritePermit { .. })
    ));
}
