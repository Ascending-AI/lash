use super::*;

/// Commit `count` more messages over `state`, the session's head.
async fn advance(backend: &Backend, state: &mut RuntimeSessionState, count: usize) {
    state.ensure_agent_frame_initialized().unwrap();
    for ordinal in 0..count {
        state
            .session_graph
            .append_message(crate::Message {
                id: format!("held-base-{}-{ordinal}", state.head_revision),
                role: crate::MessageRole::User,
                parts: crate::shared_parts(vec![crate::Part::text(
                    format!("held-base-{}-{ordinal}.p0", state.head_revision),
                    format!("message {ordinal} over revision {}", state.head_revision),
                    None,
                )]),
                origin: None,
                reply_marker: None,
            })
            .unwrap();
    }
    let receipt = backend
        .session_store_factory()
        .commit_runtime_state(crate::store::RuntimeCommit::persisted_state_for_test(state))
        .await
        .expect("commit the messages");
    state.apply_persisted_commit_result(receipt).unwrap();
}

fn encoded(messages: &crate::MessageSequence) -> serde_json::Value {
    serde_json::to_value(messages).expect("messages encode")
}

/// FIG-6634: a prelude restores over the base its caller holds only when
/// that base is the window the prelude names. A base held at a head the
/// session has since advanced past is never served for the later head's
/// prelude: that prelude's base is read again from the store.
#[tokio::test]
async fn a_held_base_serves_only_the_window_it_is() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let session = SessionId::fixture("held-prelude-base");
    crate::testing::runtime_helpers::recording_session_store(&backend, session.clone()).await;
    let mut state = SessionHead::load(&backend, &session)
        .await
        .expect("the created head")
        .state()
        .clone();
    advance(&backend, &mut state, 3).await;
    let earlier = SessionHead::load(&backend, &session)
        .await
        .expect("the earlier head");
    let (earlier_head, earlier_base) = earlier.prelude_base();
    let mut state = earlier.state().clone();
    advance(&backend, &mut state, 2).await;
    let (later_head, later_base) = SessionHead::load(&backend, &session)
        .await
        .expect("the later head")
        .prelude_base();
    assert_ne!(earlier_head, later_head, "the head advanced");
    assert_ne!(encoded(&earlier_base), encoded(&later_base));

    let mut prelude = crate::runtime::effect::TurnPrelude::new(
        crate::EffectAddress::new(
            crate::ExecutionScope::turn(session.clone(), crate::TurnId::from("held-base")),
            "turn-config:held-base",
        )
        .expect("an effect address"),
        Vec::new(),
        later_head.clone(),
        None,
    );
    prelude.capture_messages(&later_base, &later_base, &later_base);
    let store = session_store(&backend, &session)
        .await
        .expect("the session's store");

    // The base held at the earlier head is not the later head's window.
    let (base, history, _) =
        restore_prelude(&prelude, Some((&earlier_head, &earlier_base)), &store)
            .await
            .expect("restore over the store's window");
    assert_eq!(
        encoded(&base),
        encoded(&later_base),
        "the later head's window"
    );
    assert_eq!(encoded(&history), encoded(&later_base));

    // A base held at the prelude's own head is the base, whatever checkpoint
    // its head carries: the store is not read for it.
    let held: crate::MessageSequence = later_base
        .iter()
        .take(1)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    let mut pinned = later_head.clone();
    pinned.checkpoint = Some(crate::BlobRef::for_content(b"held-base-checkpoint"));
    let (base, _, _) = restore_prelude(&prelude, Some((&pinned, &held)), &store)
        .await
        .expect("restore over the held base");
    assert_eq!(
        encoded(&base),
        encoded(&held),
        "the held base serves its own window"
    );
}

fn retention(total: u64, dirty: u64) -> lash_durable::OwnerRetentionSettings {
    lash_durable::OwnerRetentionSettings {
        idle_evict: std::time::Duration::from_secs(60),
        max_owner_bytes: std::num::NonZeroU64::new(total).unwrap(),
        max_node_bytes: std::num::NonZeroU64::new(total).unwrap(),
        max_owner_dirty_bytes: std::num::NonZeroU64::new(dirty).unwrap(),
        max_node_dirty_bytes: std::num::NonZeroU64::new(dirty).unwrap(),
    }
}

async fn claim(backend: &Backend, session: &SessionId) -> ActorContext {
    let actor = ActorKey::session(session.as_str()).unwrap();
    let formats = backend
        .durable()
        .actor(&actor)
        .await
        .unwrap()
        .unwrap()
        .formats;
    let mut wake = lash_durable::MailTx::new();
    wake.wake(actor.clone());
    backend
        .durable()
        .commit_mail(wake, lash_durable::CommitLabel::new("law.wake"))
        .await
        .unwrap();
    let lease = backend
        .durable()
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("head-hydration-law"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    let owned = backend
        .durable()
        .claim(&lease, 16)
        .await
        .unwrap()
        .into_iter()
        .find(|owned| owned.actor == actor)
        .unwrap();
    ActorContext::new(
        backend.clone(),
        actor,
        owned.epoch,
        crate::AdmittedScope::runtime_operation("head-hydration-law"),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    )
}

/// COLD-HEAD-CLASS: committed source bytes fit the retained cap even when
/// the host permits almost no new uncommitted work. Future edits still use
/// the dirty cap after the coherent hydration is installed.
#[tokio::test]
async fn cold_head_hydrates_against_retained_capacity() {
    let backend = Backend::for_testing(
        crate::testing::sqlite_memory_store_set().await,
        retention(64 << 20, 1),
    );
    let session = SessionId::fixture("retained-hydration");
    crate::testing::runtime_helpers::recording_session_store(&backend, session.clone()).await;
    let mut state = SessionHead::load(&backend, &session)
        .await
        .unwrap()
        .state()
        .clone();
    advance(&backend, &mut state, 1).await;
    let cx = claim(&backend, &session).await;
    let mut owner = SessionOwner::default();
    let head = owner
        .head(&cx, &session)
        .await
        .expect("committed head fits retained capacity");
    assert_eq!(head.revision(), state.head_revision);
    let usage = cx.owner_memory().unwrap().usage();
    assert!(usage.retained > 0);
    assert_eq!(usage.dirty, 0);
    let projection = cx.session_projection().await.unwrap().unwrap();
    assert!(
        projection.allocation_budget.reserve(2).is_err(),
        "new work still obeys dirty cap"
    );
}

/// REFUSED-HYDRATION-RELEASE: when a later source cannot fit, the earlier
/// sources and their pending acknowledgement entries release together.
#[tokio::test]
async fn refused_head_hydration_releases_pending_sources() {
    let backend = crate::testing::sqlite_recording_backend().await;
    let session = SessionId::fixture("refused-hydration");
    crate::testing::runtime_helpers::recording_session_store(&backend, session.clone()).await;
    let mut state = SessionHead::load(&backend, &session)
        .await
        .unwrap()
        .state()
        .clone();
    advance(&backend, &mut state, 1).await;
    // Admit a root-sized reservation, then refuse a later source. The
    // ledger remains live to detect metadata left behind by failed loads.
    let memory = lash_durable::owner_memory::NodeMemory::new(retention(32768, 32768))
        .owner(&ActorKey::session(session.as_str()).unwrap(), Epoch(1));
    let (budget, _allocations) =
        lash_core_execution::runtime::actor::owner_allocation::OwnerAllocation::budget(
            memory.clone(),
            session.to_string(),
        );
    assert!(
        SessionHead::load_in(&backend, &session, &budget)
            .await
            .is_err()
    );
    assert_eq!(
        memory.usage().total(),
        0,
        "a refused load retains no source or pending metadata"
    );
}
