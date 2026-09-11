use super::*;

/// The deployment-level administrative read exposes only registered,
/// unresolved waits for exactly one session, and every returned key remains a
/// valid input to the existing resolution path.
pub(crate) async fn effect_host_lists_registered_unresolved_waits(
    owner: Arc<dyn EffectHost>,
    observer: Arc<dyn EffectHost>,
    resolver: Arc<dyn EffectHost>,
) {
    let suffix = uuid::Uuid::new_v4();
    let session_a = SessionId::from(format!("await-event-list-a-{suffix}"));
    let session_b = SessionId::from(format!("await-event-list-b-{suffix}"));
    let scope_a = durable_turn_scope(&session_a, format!("turn-a-{suffix}"));
    let scope_b = durable_turn_scope(&session_b, format!("turn-b-{suffix}"));

    assert!(
        observer
            .list_outstanding_await_event_keys(&SessionId::from(format!(
                "await-event-list-unknown-{suffix}"
            )))
            .await
            .expect("unknown session is an empty supported read")
            .is_empty()
    );

    let unregistered = owner
        .await_event_key(
            &scope_a,
            AwaitEventWaitIdentity::tool_completion(format!("unregistered-{suffix}")),
        )
        .await
        .expect("derive a key without registering it");
    assert!(
        observer
            .list_outstanding_await_event_keys(&session_a)
            .await
            .expect("derived key does not register a wait")
            .is_empty()
    );

    let key_a = owner
        .await_event_key(
            &scope_a,
            AwaitEventWaitIdentity::tool_completion(format!("registered-a-{suffix}")),
        )
        .await
        .expect("derive session A key");
    let key_b = owner
        .await_event_key(
            &scope_b,
            AwaitEventWaitIdentity::tool_completion(format!("registered-b-{suffix}")),
        )
        .await
        .expect("derive session B key");
    let owner_a = Arc::clone(&owner);
    let waiter_key_a = key_a.clone();
    let waiter_a = crate::task::spawn(async move {
        owner_a
            .await_await_event(
                &waiter_key_a,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });
    let owner_b = Arc::clone(&owner);
    let waiter_key_b = key_b.clone();
    let waiter_b = crate::task::spawn(async move {
        owner_b
            .await_await_event(
                &waiter_key_b,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });

    let listed_a = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let listed = observer
                .list_outstanding_await_event_keys(&session_a)
                .await
                .expect("list session A waits");
            if listed == vec![key_a.clone()] {
                break listed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session A wait becomes registered");
    let listed_b = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let listed = observer
                .list_outstanding_await_event_keys(&session_b)
                .await
                .expect("list session B waits");
            if listed == vec![key_b.clone()] {
                break listed;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session B wait becomes registered");
    pretty_assertions::assert_eq!(listed_a, vec![key_a.clone()]);
    pretty_assertions::assert_eq!(listed_b, vec![key_b.clone()]);
    assert!(
        !listed_a.contains(&key_b),
        "session B leaked into session A"
    );

    let terminal_a = Resolution::Ok(serde_json::json!({ "session": "a" }));
    pretty_assertions::assert_eq!(
        resolver
            .resolve_await_event(&listed_a[0], terminal_a.clone())
            .await
            .expect("resolve discovered session A key"),
        ResolveOutcome::Accepted
    );
    pretty_assertions::assert_eq!(
        waiter_a
            .await
            .expect("session A waiter joins")
            .expect("session A waiter resolves"),
        terminal_a
    );
    assert!(
        observer
            .list_outstanding_await_event_keys(&session_a)
            .await
            .expect("settled wait is absent")
            .is_empty()
    );

    resolver
        .cancel_await_events_for_session(&session_b)
        .await
        .expect("cancel session B waits");
    pretty_assertions::assert_eq!(
        waiter_b
            .await
            .expect("session B waiter joins")
            .expect("session B waiter resolves"),
        Resolution::Cancelled
    );
    assert!(
        observer
            .list_outstanding_await_event_keys(&session_b)
            .await
            .expect("cancelled terminal is absent")
            .is_empty()
    );

    pretty_assertions::assert_eq!(
        resolver
            .resolve_await_event(&unregistered, Resolution::Cancelled)
            .await
            .expect("resolve never-registered key"),
        ResolveOutcome::Accepted
    );
    assert!(
        observer
            .list_outstanding_await_event_keys(&session_a)
            .await
            .expect("early terminal is absent")
            .is_empty()
    );

    let revoked_key = owner
        .await_event_key(
            &scope_a,
            AwaitEventWaitIdentity::tool_completion(format!("revoked-{suffix}")),
        )
        .await
        .expect("derive revoked-session witness key");
    let revoked_owner = Arc::clone(&owner);
    let waiter_revoked_key = revoked_key.clone();
    let revoked_waiter = crate::task::spawn(async move {
        revoked_owner
            .await_await_event(
                &waiter_revoked_key,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if observer
                .list_outstanding_await_event_keys(&session_a)
                .await
                .expect("list revocation witness")
                == vec![revoked_key.clone()]
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("revocation witness becomes registered");
    resolver
        .revoke_await_events_for_session(&session_a)
        .await
        .expect("revoke session A");
    assert!(revoked_waiter.await.expect("revoked waiter joins").is_err());
    assert!(
        observer
            .list_outstanding_await_event_keys(&session_a)
            .await
            .expect("revoked waits are absent")
            .is_empty()
    );
}
