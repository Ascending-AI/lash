//! A root's stopped child parks the root (FIG-4607).

use super::*;

/// FIG-4607: stopped work a root waits on parks that root, with no engine
/// handle. Any number of stopped children, over any number of passes, write
/// one park. A redrive owns the root's children until it resumed them, and a
/// child still stopped after that stopped again: it re-parks the root, so the
/// operator can act on it again.
pub async fn a_stopped_child_parks_its_root_once_and_reparks_only_after_a_settled_redrive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = DriveParts::new(prefix, "child-exhaustion", &host, &stores, 8).await;
    let root = TurnId::from("waits-on-child");
    parts.enqueue("input", Some(root.as_str())).await;
    let factory = stores.session_store_factory();
    let writer =
        lash_core::drive::StoreParkRecovery::new(factory.as_ref(), parts.host.clock.as_ref());
    let child = ParkTarget::RootChild {
        session: parts.session_id.clone(),
        root: root.clone(),
    };
    let reason =
        ParkReason::engine_retry_exhausted(8, Some("500".into()), "child retries exhausted".into());
    assert_eq!(
        writer
            .record_engine_park(
                &child,
                reason.clone(),
                EnginePark::new("child-a"),
                // An operator resumed the child after the listing: it runs.
                &Execution { stopped: false },
            )
            .await
            .expect("record from a stale listing"),
        EngineParkRecorded::Redriven
    );
    assert_eq!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("park"),
        None,
        "a stale listing parks no running root"
    );
    let EngineParkRecorded::Parked(id) = writer
        .record_engine_park(
            &child,
            reason.clone(),
            EnginePark::new("child-a"),
            &Execution { stopped: true },
        )
        .await
        .expect("park the root of the stopped child")
    else {
        panic!("new park");
    };
    let held = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("park")
        .expect("held");
    assert_eq!((held.turn_id.clone(), held.park_id), (root.clone(), id));
    assert_eq!(held.reason, reason);
    assert_eq!(held.engine, None, "the engine finds the children itself");
    for sibling in ["child-a", "child-b"] {
        assert_eq!(
            writer
                .record_engine_park(
                    &child,
                    ParkReason::engine_retry_exhausted(9, None, "a later pass".into()),
                    EnginePark::new(sibling),
                    &Execution { stopped: true },
                )
                .await
                .expect("repeat"),
            EngineParkRecorded::AttachedToExisting(id)
        );
    }
    assert_eq!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("park"),
        Some(held),
        "a parked root's stopped children write nothing more"
    );
    assert!(
        factory
            .root_terminal(&parts.session_id, &root)
            .await
            .expect("terminal")
            .is_none()
    );

    let f = Fixture::new(prefix, "child-redrive", &host, &stores).await;
    let writer =
        lash_core::drive::StoreParkRecovery::new(f.factory.as_ref(), f.parts.host.clock.as_ref());
    let child = ParkTarget::RootChild {
        session: f.parts.session_id.clone(),
        root: f.root.clone(),
    };
    let record = |stopped: bool| {
        let (writer, child) = (&writer, &child);
        async move {
            writer
                .record_engine_park(
                    child,
                    ParkReason::engine_retry_exhausted(8, None, "the child stopped".into()),
                    EnginePark::new("child-a"),
                    &Execution { stopped },
                )
                .await
                .expect("record the stopped child")
        }
    };
    let redrive = f.verb(RootVerb::Redrive).await.expect("redrive");
    assert_eq!(
        record(true).await,
        EngineParkRecorded::Redriven,
        "an open redrive owns the root's stopped children"
    );
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        record(false).await,
        EngineParkRecorded::Redriven,
        "a listing read before the redrive resumed the child is stale"
    );
    let park = f.park().await.expect("still parked");
    assert_eq!(park.resume_intent, Some(redrive.id));
    assert_eq!(park.attempts, f.park.attempts);
    assert_eq!(
        record(true).await,
        EngineParkRecorded::AttachedToExisting(f.park.park_id),
        "a child still stopped after the redrive re-parks the same park"
    );
    let again = f.park().await.expect("re-parked");
    assert_eq!(again.resume_intent, None);
    assert_eq!(again.attempts, f.park.attempts + 1);
    f.verb(RootVerb::Redrive)
        .await
        .expect("the re-parked root can be redriven again");
}
