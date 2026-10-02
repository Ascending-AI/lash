use super::*;
use pretty_assertions::assert_eq;

/// Subscription projectors retain the latest source state and durable deletion
/// evidence, including through physical retention and reopening the store.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture outcomes are asserted at each step"
)]
pub async fn trigger_subscription_change_cursor_law<F>(make: F)
where
    F: Fn() -> ReopenableTriggerStore,
{
    let stores = make();
    let store = &stores.open;
    let start = crate::TriggerSubscriptionChangeCursor::initial();
    let (empty, initial) = store
        .list_subscriptions_with_cursor()
        .await
        .expect("empty snapshot");
    assert!(empty.is_empty());
    assert_eq!(initial, start);
    let session = SessionId::from("subscription-change-owner");
    let mut draft = sample_draft(&session, "first", "source", "first");
    let first = mutate(
        store,
        "feed-register-first",
        register_command(&session, draft.clone()),
    )
    .await;
    let (changes, first_cursor) = store
        .subscriptions_changed_since(start, 1)
        .await
        .expect("first page");
    assert_eq!(
        changes,
        vec![crate::TriggerSubscriptionChange::from(
            &first.record_snapshot
        )]
    );
    assert!(first_cursor.store_sequence() > 0);
    assert_eq!(
        store
            .subscriptions_changed_since(start, 0)
            .await
            .expect("zero page"),
        (Vec::new(), start)
    );

    let second = mutate(
        store,
        "feed-register-second",
        register_command(
            &session,
            sample_draft(&session, "second", "source", "second"),
        ),
    )
    .await;
    draft.source = serde_json::json!({"button": "Red"});
    let updated = mutate(
        store,
        "feed-update",
        update_command(&session, "first", draft.clone(), first.revision),
    )
    .await;
    let (updated_changes, updated_cursor) = store
        .subscriptions_changed_since(first_cursor, 20)
        .await
        .expect("updated page");
    assert_eq!(
        updated_changes.last(),
        Some(&crate::TriggerSubscriptionChange::from(
            &updated.record_snapshot
        ))
    );
    let disabled = mutate(
        store,
        "feed-disable",
        revision_command(&session, "first", updated.revision, "disable"),
    )
    .await;
    let (disabled_changes, disabled_cursor) = store
        .subscriptions_changed_since(updated_cursor, 20)
        .await
        .expect("disabled page");
    assert_eq!(
        disabled_changes,
        vec![crate::TriggerSubscriptionChange::from(
            &disabled.record_snapshot
        )]
    );
    assert!(!disabled_changes[0].lifecycle.enabled());
    let enabled = mutate(
        store,
        "feed-enable",
        revision_command(&session, "first", disabled.revision, "enable"),
    )
    .await;
    assert!(enabled.enabled);
    assert_eq!(
        store
            .subscriptions_changed_since(disabled_cursor, 20)
            .await
            .expect("enabled page")
            .0,
        vec![crate::TriggerSubscriptionChange::from(
            &enabled.record_snapshot
        )]
    );
    let (changes, enabled_cursor) = store
        .subscriptions_changed_since(first_cursor, 20)
        .await
        .expect("coalesced page");
    assert_eq!(
        changes,
        vec![
            crate::TriggerSubscriptionChange::from(&second.record_snapshot),
            crate::TriggerSubscriptionChange::from(&enabled.record_snapshot)
        ]
    );
    assert_eq!(changes[1].source, draft.source);
    assert_eq!(
        store
            .subscriptions_changed_since(first_cursor, 20)
            .await
            .expect("repeat older cursor"),
        (changes.clone(), enabled_cursor)
    );
    assert_eq!(
        stores
            .reopen
            .subscriptions_changed_since(first_cursor, 20)
            .await
            .expect("reopened older cursor"),
        (changes, enabled_cursor)
    );
    let (one, page_cursor) = store
        .subscriptions_changed_since(first_cursor, 1)
        .await
        .expect("bounded page");
    assert_eq!(
        one,
        vec![crate::TriggerSubscriptionChange::from(
            &second.record_snapshot
        )]
    );
    assert_eq!(
        store
            .subscriptions_changed_since(page_cursor, 1)
            .await
            .expect("next bounded page")
            .0,
        vec![crate::TriggerSubscriptionChange::from(
            &enabled.record_snapshot
        )]
    );

    // Replays, identical definitions and losing revisions publish no change.
    let _ = mutate(
        store,
        "feed-enable",
        revision_command(&session, "first", disabled.revision, "enable"),
    )
    .await;
    let unchanged = mutate(store, "feed-identical", register_command(&session, draft)).await;
    assert_eq!(unchanged.revision, enabled.revision);
    assert!(
        execute(
            store,
            "feed-stale",
            revision_command(&session, "first", first.revision, "disable")
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .subscriptions_changed_since(enabled_cursor, 20)
            .await
            .expect("no spurious change"),
        (Vec::new(), enabled_cursor)
    );

    let deleted = mutate(
        store,
        "feed-delete",
        revision_command(&session, "second", second.revision, "delete"),
    )
    .await;
    assert_eq!(
        store
            .delete_session_subscriptions(&session)
            .await
            .expect("session tombstones"),
        1
    );
    let (changes, deleted_cursor) = store
        .subscriptions_changed_since(enabled_cursor, 20)
        .await
        .expect("deletion page");
    assert_eq!(changes.len(), 2);
    assert_eq!(
        changes[0],
        crate::TriggerSubscriptionChange::from(&deleted.record_snapshot)
    );
    assert_eq!(changes[1].subscription_id, first.subscription_id);
    assert_eq!(changes[1].revision, enabled.revision + 1);
    assert!(
        changes
            .iter()
            .all(|change| change.lifecycle.is_tombstoned())
    );
    assert_eq!(
        store
            .reconcile_trigger_retention(&[], std::slice::from_ref(&session))
            .await
            .expect("physical deletion")
            .reclaimed_subscription_count,
        2
    );
    assert_eq!(
        stores
            .reopen
            .subscriptions_changed_since(enabled_cursor, 20)
            .await
            .expect("tombstones survive physical deletion and reopen"),
        (changes, deleted_cursor)
    );

    let pruned_owner = SessionId::from("subscription-change-pruned");
    let pruned = mutate(
        store,
        "feed-register-prune",
        register_command(
            &pruned_owner,
            sample_draft(&pruned_owner, "pruned", "source", "pruned"),
        ),
    )
    .await;
    let outcome = execute(
        store,
        "feed-prune",
        crate::TriggerCommand::Prune {
            owner_scope: owner(&pruned_owner),
            actor: actor(&pruned_owner),
            subscription_keys: vec!["pruned".to_string()],
        },
    )
    .await
    .expect("prune");
    assert!(matches!(
        outcome,
        crate::TriggerCommandOutcome::Prune { .. }
    ));
    let retained_owner = SessionId::from("subscription-change-retention");
    let retained = mutate(
        store,
        "feed-register-retention",
        register_command(
            &retained_owner,
            sample_draft(&retained_owner, "retained", "source", "retained"),
        ),
    )
    .await;
    store
        .reconcile_trigger_retention(&[], std::slice::from_ref(&retained_owner))
        .await
        .expect("retention tombstones a live subscription");
    let (changes, compact_cursor) = store
        .subscriptions_changed_since(deleted_cursor, 20)
        .await
        .expect("prune and retention page");
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].subscription_id, pruned.subscription_id);
    assert_eq!(changes[0].revision, pruned.revision + 1);
    assert_eq!(changes[1].subscription_id, retained.subscription_id);
    assert_eq!(changes[1].revision, retained.revision + 1);
    assert!(
        changes
            .iter()
            .all(|change| change.lifecycle.is_tombstoned())
    );
    assert_eq!(
        store
            .compact_subscription_tombstones(0)
            .await
            .expect("retain horizon"),
        0
    );
    assert_eq!(
        store
            .compact_subscription_tombstones(u64::MAX)
            .await
            .expect("compact horizon"),
        4
    );
    let expired = stores
        .reopen
        .subscriptions_changed_since(start, 20)
        .await
        .expect_err("expired cursor is refused");
    let encoded = serde_json::to_vec(&expired).expect("encode typed refusal");
    let expired: crate::PluginError =
        serde_json::from_slice(&encoded).expect("decode typed refusal");
    assert!(expired.is_terminal());
    match expired {
        crate::PluginError::TriggerSubscriptionChangeCursorPruned {
            requested_cursor,
            tombstone_compaction_horizon,
        } => {
            assert_eq!(requested_cursor, start);
            assert_eq!(tombstone_compaction_horizon, compact_cursor);
        }
        error => panic!("expected typed cursor refusal, got {error:?}"),
    }
    assert_eq!(
        store
            .subscriptions_changed_since(compact_cursor, 20)
            .await
            .expect("horizon itself remains valid"),
        (Vec::new(), compact_cursor)
    );
    let survivor_owner = SessionId::from("subscription-change-survivor");
    let survivor = mutate(
        store,
        "feed-survivor",
        register_command(
            &survivor_owner,
            sample_draft(&survivor_owner, "survivor", "source", "survivor"),
        ),
    )
    .await;
    let (snapshot, resume) = stores
        .reopen
        .list_subscriptions_with_cursor()
        .await
        .expect("resync snapshot");
    assert_eq!(snapshot, vec![survivor.record_snapshot.clone()]);
    assert!(resume.store_sequence() > compact_cursor.store_sequence());
    assert_eq!(
        store
            .subscriptions_changed_since(resume, 20)
            .await
            .expect("snapshot continuation"),
        (Vec::new(), resume)
    );
    let disabled = mutate(
        store,
        "feed-survivor-disable",
        revision_command(&survivor_owner, "survivor", survivor.revision, "disable"),
    )
    .await;
    assert_eq!(
        store
            .subscriptions_changed_since(resume, 20)
            .await
            .expect("changes after resync")
            .0,
        vec![crate::TriggerSubscriptionChange::from(
            &disabled.record_snapshot
        )]
    );

    let replacement = mutate(
        store,
        "feed-first-new-incarnation",
        register_command(&session, sample_draft(&session, "first", "source", "first")),
    )
    .await;
    assert_ne!(replacement.incarnation, first.incarnation);
    let replacement_change = store
        .subscriptions_changed_since(resume, 20)
        .await
        .expect("new incarnation after retention")
        .0;
    assert_eq!(
        replacement_change.last(),
        Some(&crate::TriggerSubscriptionChange::from(
            &replacement.record_snapshot
        ))
    );
}
