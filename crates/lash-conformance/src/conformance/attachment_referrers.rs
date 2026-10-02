//! Durable attachment liveness, write permits, and shared referrer fences.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance fixtures establish every result before inspecting it"
)]
use super::attachment_adoption::{
    AttachmentBytesFactory, create, image_meta, record_completed_write,
};
use lash_core::attachments::reclaim_unreferenced_attachments;
use lash_core::facade_support::{
    ModelToolReturn, ModelToolReturnPart, RuntimeAttachmentStore, SystemClock,
};
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
    match referrer {
        ArtifactReferrer::Upload(upload) => ReferrerClaim::guarded(ReferrerGuard::Upload {
            upload,
            expires_at_ms: 1000,
        }),
        ArtifactReferrer::Execution(journal) => {
            ReferrerClaim::guarded(ReferrerGuard::Journal(journal))
        }
        ArtifactReferrer::StartInput { start_key, starter } => {
            ReferrerClaim::guarded(ReferrerGuard::StartInput { start_key, starter })
        }
        referrer => ReferrerClaim::unguarded(referrer).unwrap(),
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
            store: lash_core::ReferrerStore::Attachment,
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
    let facade = RuntimeAttachmentStore::new_with_clock(
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
        let cleanup = ArtifactCleanup::Await(ReferrerGuard::Upload {
            upload: {
                let ArtifactReferrer::Upload(upload) = referrer.clone() else {
                    panic!("fixture referrer kind")
                };
                upload
            },
            expires_at_ms: 9999,
        });
        let id = h.cleanup.arm_cleanup(&cleanup, 100).await.unwrap();
        let recorded = h.cleanup.load_cleanup(&id).await.unwrap().unwrap();
        assert!(matches!(
            recorded,
            ArtifactCleanup::Await(ReferrerGuard::Upload {
                expires_at_ms: 1100,
                ..
            })
        ));
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
        ..RuntimeSessionState::new(SessionPolicy::new(
            TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
        ))
    };
    state.ensure_agent_frame_initialized();
    state
}

/// A presentation's retained output (FIG-1643) across a crash before its
/// commit, through the runtime's own retention boundary.
///
/// The boundary retains an oversized return under its turn's execution, as a
/// presentation does, and that turn dies before its commit. A sweep with no
/// grace keeps the retained attachment while its execution can still commit,
/// since a redrive's commit names it. A later turn retains its own output and
/// its commit names it, which holds it for the session. Once both executions
/// end, as the cleanup executor ends a retired journal's referrer, the sweep
/// reclaims the crashed output and the committed one resolves to its exact
/// bytes.
pub async fn retained_output_is_held_by_its_execution_until_a_commit_names_it(
    h: AttachmentReferrerHandles,
) {
    const SESSION: &str = "retained-output-crash";
    const POLICY: OutputRetentionPolicy = OutputRetentionPolicy {
        inline_limit_bytes: 1024,
        witness_bytes: 256,
    };
    let store = create(&h.factory, SESSION).await;
    let backend = (h.bytes)();
    let facade = Arc::new(
        RuntimeAttachmentStore::new(
            Arc::clone(&backend),
            Arc::clone(&store) as Arc<dyn AttachmentReferrers>,
            RuntimeOwner::Session(SessionId::from(SESSION)),
        )
        .with_output_retention(POLICY),
    );
    let sweep = || async {
        reclaim_unreferenced_attachments(
            h.factory.as_ref(),
            backend.as_ref(),
            AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
            },
        )
        .await
        .unwrap()
    };
    let journal = |turn: &str| {
        ExecutionScope::turn(SessionId::from(SESSION), turn)
            .journal_identity()
            .unwrap()
    };
    let retain = |text: String, turn: &'static str| {
        let facade = Arc::clone(&facade);
        let journal = journal(turn);
        async move {
            let _execution = facade.bind_execution_scoped(journal).unwrap();
            let artifacts =
                lash_core::runtime::effect::SessionPresentationArtifacts::new(Arc::clone(&facade));
            let mut model_return = ModelToolReturn {
                tool_name: "oversized".to_string(),
                parts: vec![ModelToolReturnPart::text(text)],
                attachment_notices: Vec::new(),
            };
            lash_core::runtime::effect::retain_oversized_return(
                &mut model_return,
                &ToolCallId::fixture(turn),
                &artifacts,
                POLICY,
            )
            .await
            .unwrap();
            let [ModelToolReturnPart::Retained(retained)] = model_return.parts.as_slice() else {
                panic!("an oversized return is one retained block: {model_return:?}");
            };
            assert!(retained.witness.len() <= 256);
            retained.clone()
        }
    };

    // The crashed turn retained its output and never committed.
    let crashed = retain("crashed turn output\n".repeat(500), "crashed-turn").await;
    assert_eq!(
        store
            .attachment_referrers(&crashed.reference.id)
            .await
            .unwrap(),
        vec![ArtifactReferrer::Execution(journal("crashed-turn"))]
    );
    sweep().await;
    assert!(
        backend
            .get(&crashed.reference.id, 32 * 1024 * 1024)
            .await
            .is_ok(),
        "a still-committable turn's retained output survives the sweep"
    );

    // A later turn retains its own output, and its commit names it.
    let committed_text = "committed turn output\n".repeat(500);
    let committed = retain(committed_text.clone(), "committed-turn").await;
    let commit = RuntimeCommit::persisted_state_for_test(&state(SESSION))
        .with_committed_attachments([committed.reference.id.clone()]);
    store.commit_runtime_state(commit).await.unwrap();

    for turn in ["crashed-turn", "committed-turn"] {
        store
            .end_attachment_referrer(&ArtifactReferrer::Execution(journal(turn)))
            .await
            .unwrap();
    }
    let report = sweep().await;
    assert!(report.reclaimed_count >= 1, "{report:?}");
    assert!(
        matches!(
            backend.get(&crashed.reference.id, 32 * 1024 * 1024).await,
            Err(AttachmentStoreError::NotFound(_))
        ),
        "the crashed turn's retained output is reclaimed once its execution ends unnamed"
    );
    assert_eq!(
        backend
            .get(&committed.reference.id, 32 * 1024 * 1024)
            .await
            .unwrap()
            .bytes,
        committed_text.into_bytes(),
        "the committed reference resolves to the exact retained bytes"
    );
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
    let commit = RuntimeCommit::persisted_state_for_test(&current)
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
    let commit =
        RuntimeCommit::persisted_state_for_test(&current).with_committed_attachments([id.clone()]);
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

/// A pin holds its session's attachments only while the session lives
/// (FIG-4156, FIG-4731): a sweep keeps the bytes a pinned revision names, and
/// the deletion takes the pin with the session, so the graph retires at once,
/// the cleanup executor may end the session's edge, and the sweep reclaims
/// the bytes.
pub async fn attachment_prefix_pin_keeps_the_session_edge_until_unpin(
    h: AttachmentReferrerHandles,
) {
    let session = SessionId::from("pinned-attachment-session");
    let store = create(&h.factory, session.as_str()).await;
    let bytes = (h.bytes)();
    let facade = RuntimeAttachmentStore::new(
        bytes.clone(),
        store.clone(),
        RuntimeOwner::Session(session.clone()),
    );
    let reference = facade.put(vec![42], image_meta()).await.unwrap();
    let mut current = state(session.as_str());
    current.session_graph.append_message(Message {
        id: "pinned-message".into(),
        role: MessageRole::User,
        origin: None,
        parts: Arc::new(vec![Part::attachment_part(
            "pinned-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                source: AttachmentSource::stored(reference.clone()),
            }),
        )]),
    });
    let commit = RuntimeCommit::persisted_state_for_test(&current)
        .with_committed_attachments([reference.id.clone()]);
    let receipt = store.commit_runtime_state(commit).await.unwrap();
    let pinned = lash_core::Target::Revision(receipt.head_revision);
    h.factory.pin(&session, &pinned).await.unwrap();
    // The unbound put's upload edge is not what this law is about.
    for referrer in store.attachment_referrers(&reference.id).await.unwrap() {
        if matches!(referrer, ArtifactReferrer::Upload(_)) {
            store.end_attachment_referrer(&referrer).await.unwrap();
        }
    }
    let referrer = ArtifactReferrer::Session(session.clone());
    let sweep = || async {
        reclaim_unreferenced_attachments(
            h.factory.as_ref(),
            bytes.as_ref(),
            AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
            },
        )
        .await
        .unwrap()
    };

    assert_eq!(
        store.attachment_referrers(&reference.id).await.unwrap(),
        vec![referrer.clone()]
    );
    assert_eq!(sweep().await.reclaimed_count, 0);
    assert_eq!(
        bytes
            .get(&reference.id, 32 * 1024 * 1024)
            .await
            .unwrap()
            .bytes,
        vec![42]
    );

    h.factory.delete_session(&session).await.unwrap();
    assert_eq!(
        store.session_referrer_state(&session).await.unwrap(),
        SessionReferrerState::DeletedRetired,
        "the pin is deleted with its session, so nothing retains the graph"
    );
    // What the cleanup executor does for a retired session's guard.
    store.end_attachment_referrer(&referrer).await.unwrap();
    assert!(
        store
            .attachment_referrers(&reference.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(sweep().await.reclaimed_count, 1);
    assert!(bytes.head(&reference.id).await.unwrap().is_none());
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
        RuntimeAttachmentStore::new(bytes, store.clone(), RuntimeOwner::Session(session.clone()));
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
    let commit = RuntimeCommit::persisted_state_for_test(&current)
        .with_committed_attachments([reference.id.clone()]);
    let receipt = store.commit_runtime_state(commit).await.unwrap();
    h.factory
        .fork_session(&ForkSessionRequest {
            session_id: "retained-child".into(),
            source_session_id: session.clone(),
            head_revision: receipt.head_revision,
            relation: SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            config: SessionPolicy::new(TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
                .into(),
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
    let cleanup = ArtifactCleanup::Await(ReferrerGuard::SessionGraphRetired(session.clone()));
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

/// Deliberately stop a root enumeration before one kind or its next page.
struct PartialAttachmentRoots {
    factory: Arc<dyn DeploymentStore>,
    omitted: AttachmentId,
    truncate_page: bool,
}

#[async_trait::async_trait]
impl AttachmentRootSet for PartialAttachmentRoots {
    async fn attachment_root_page(
        &self,
        source: lash_core::attachments::AttachmentRootSource,
        after: Option<&AttachmentId>,
    ) -> Result<lash_core::attachments::AttachmentRootPage, StoreError> {
        use lash_core::attachments::{AttachmentRootPage, AttachmentRootSource};
        if source == AttachmentRootSource::Referrer(ArtifactReferrerKind::ProcessRecord) {
            if self.truncate_page && after.is_none() {
                // Plant a nonterminal page. Stopping at this page must never
                // authorize a deletion, even if another source was complete.
                return AttachmentRootPage::from_rows(
                    (0..AttachmentRootPage::QUERY_LIMIT)
                        .map(|index| {
                            AttachmentId::parse(format!("partial-page-{index:04}")).unwrap()
                        })
                        .collect(),
                );
            }
            return Err(StoreError::IncompleteEnumeration {
                scope: "live attachment roots",
                unfinished: if self.truncate_page {
                    "truncated process_record continuation"
                } else {
                    "skipped process_record source"
                }
                .into(),
            });
        }
        self.factory.attachment_root_page(source, after).await
    }

    async fn has_live_attachment_ref(&self, id: &AttachmentId) -> Result<bool, StoreError> {
        if id == &self.omitted {
            return Ok(false);
        }
        self.factory.has_live_attachment_ref(id).await
    }
}

async fn partial_attachment_enumeration(h: AttachmentReferrerHandles, truncate_page: bool) {
    let store = create(&h.factory, "partial-roots").await;
    let bytes = (h.bytes)();
    let protected = bytes
        .put(
            b"protected by omitted process record".to_vec(),
            image_meta(),
        )
        .await
        .unwrap();
    let visible = bytes
        .put(b"visible session root".to_vec(), image_meta())
        .await
        .unwrap();
    record_completed_write(
        &store,
        &write(
            &protected.id,
            ArtifactReferrer::ProcessRecord(ProcessId::fixture("partial-process")),
        ),
    )
    .await;
    record_completed_write(
        &store,
        &write(
            &visible.id,
            ArtifactReferrer::Session("partial-roots".into()),
        ),
    )
    .await;
    let partial = PartialAttachmentRoots {
        factory: h.factory.clone(),
        omitted: protected.id.clone(),
        truncate_page,
    };
    let result = reclaim_unreferenced_attachments(
        &partial,
        bytes.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await;
    assert!(
        matches!(result.unwrap_err().stop,
        store::MaintenanceStop::Failed(AttachmentStoreError::RootSetEnumerationFailed { source })
        if matches!(*source, StoreError::IncompleteEnumeration { .. })),
        "partial enumeration must stop typed before destruction"
    );
    bytes
        .get(&protected.id, 32 * 1024 * 1024)
        .await
        .expect("omitted referrer's live bytes survive");
    bytes
        .get(&visible.id, 32 * 1024 * 1024)
        .await
        .expect("visible live bytes survive");
}

pub async fn skipped_attachment_referrer_kind_cannot_authorize_delete(
    h: AttachmentReferrerHandles,
) {
    partial_attachment_enumeration(h, false).await;
}

pub async fn truncated_attachment_root_page_cannot_authorize_delete(h: AttachmentReferrerHandles) {
    partial_attachment_enumeration(h, true).await;
}

pub async fn complete_attachment_roots_cover_every_kind_and_exhaust_pages(
    h: AttachmentReferrerHandles,
) {
    let store = create(&h.factory, "complete-roots").await;
    let session = SessionId::from("complete-roots");
    let mut expected = std::collections::BTreeSet::new();
    for index in 0..258 {
        let id = AttachmentId::parse(format!("paged-root-{index:04}")).unwrap();
        record_completed_write(
            &store,
            &write(&id, ArtifactReferrer::Session(session.clone())),
        )
        .await;
        expected.insert(id);
    }
    for (index, referrer) in [
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("complete-process")),
        ArtifactReferrer::Upload(UploadReferrerId::mint(session.clone())),
        execution(&session, "complete-execution"),
    ]
    .into_iter()
    .enumerate()
    {
        let id = AttachmentId::parse(format!("kind-root-{index}")).unwrap();
        record_completed_write(&store, &write(&id, referrer)).await;
        expected.insert(id);
    }
    let pending = AttachmentId::parse("pending-without-edge").unwrap();
    let attempt = write(
        &pending,
        ArtifactReferrer::ProcessRecord(ProcessId::fixture("pending-process")),
    );
    let permit = permit(store.as_ref(), &attempt).await;
    store
        .forget_attachment_ref(&attempt.claim.referrer(), &pending)
        .await
        .unwrap();
    expected.insert(pending);
    let witnessed = h.factory.live_attachment_refs().await.unwrap();
    assert_eq!(
        witnessed.values(),
        &expected,
        "the witness includes every source, the second page, and an edgeless pending write"
    );
    store
        .abort_attachment_write(&attempt, permit)
        .await
        .unwrap();
}

pub async fn attachment_root_sources_partition_start_inputs(h: AttachmentReferrerHandles) {
    use lash_core::attachments::AttachmentRootSource;
    let id = AttachmentId::parse("start-input-root-partition").unwrap();
    let starter = ExecutionScope::runtime_operation("start-input-root-partition")
        .journal_identity()
        .unwrap();
    let referrer = ArtifactReferrer::StartInput {
        start_key: lash_core::StartKey::for_host("root-partition"),
        starter,
    };
    (h.insert_edge)(
        id.clone(),
        referrer.kind().as_str().into(),
        referrer.canonical_id(),
    )
    .await
    .unwrap();
    let page = h
        .factory
        .attachment_root_page(
            AttachmentRootSource::Referrer(ArtifactReferrerKind::StartInput),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        format!("{page:?}"),
        format!(
            "{:?}",
            lash_core::attachments::AttachmentRootPage::from_rows(vec![id]).unwrap()
        )
    );
    let other = h
        .factory
        .attachment_root_page(AttachmentRootSource::OtherReferrers, None)
        .await
        .unwrap();
    assert_eq!(
        format!("{other:?}"),
        format!(
            "{:?}",
            lash_core::attachments::AttachmentRootPage::from_rows(vec![]).unwrap()
        ),
        "a known attachment kind must not be enumerated as an unknown kind"
    );
}
