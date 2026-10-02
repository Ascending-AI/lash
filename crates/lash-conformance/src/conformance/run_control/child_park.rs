//! A run's stopped child parks the run (FIG-4607).

use super::*;

/// FIG-4607, FIG-4630: stopped work a run waits on parks that run, and the
/// park records each stopped child's engine handle once. Any number of
/// stopped children, over any number of passes, write one park. A redrive is
/// handed exactly the children its park recorded when it was requested and
/// owns them until it resumed them, and a child still stopped after the
/// redrive stopped again: it re-parks the run, so the operator can act on it
/// again.
pub async fn a_stopped_child_parks_its_run_once_and_reparks_only_after_a_settled_redrive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "child-exhaustion", &host, &stores, 8).await;
    let run = TurnId::from("waits-on-child");
    parts.enqueue("input", Some(run.as_str())).await;
    let factory = stores.session_store_factory();
    let writer =
        lash_core::shift::StoreParkRecovery::new(factory.as_ref(), parts.host.clock.as_ref());
    let child = ParkTarget::RunChild {
        session: parts.session_id.clone(),
        run: run.clone(),
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
        "a stale listing parks no running run"
    );
    let EngineParkRecorded::Parked(id) = writer
        .record_engine_park(
            &child,
            reason.clone(),
            EnginePark::new("child-a"),
            &Execution { stopped: true },
        )
        .await
        .expect("park the run of the stopped child")
    else {
        panic!("new park");
    };
    let held = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("park")
        .expect("held");
    assert_eq!((held.turn_id.clone(), held.park_id), (run.clone(), id));
    assert_eq!(held.reason, reason);
    assert_eq!(held.engine, None, "the run's own execution is not stopped");
    assert_eq!(
        held.children,
        [EnginePark::new("child-a")],
        "the park records the stopped child"
    );
    for (sibling, recorded) in [
        ("child-a", vec!["child-a"]),
        ("child-b", vec!["child-a", "child-b"]),
        ("child-b", vec!["child-a", "child-b"]),
    ] {
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
        assert_eq!(
            parts
                .store
                .load_turn_park(&parts.session_id)
                .await
                .expect("park"),
            Some(TurnPark {
                children: recorded.into_iter().map(EnginePark::new).collect(),
                ..held.clone()
            }),
            "a parked run's stopped children add their handle once and nothing more"
        );
    }
    assert_eq!(
        factory
            .list_turn_parks(&TurnParkQuery {
                reasons: None,
                session: Some(parts.session_id.clone()),
                parked_at_or_before_ms: None,
                after: None,
                limit: NonZeroUsize::MIN,
            })
            .await
            .expect("list parks")
            .into_iter()
            .map(|park| park.children)
            .collect::<Vec<_>>(),
        [[EnginePark::new("child-a"), EnginePark::new("child-b")]],
        "the park listing reads the recorded children too"
    );
    assert!(
        factory
            .run_terminal(&parts.session_id, &run)
            .await
            .expect("terminal")
            .is_none()
    );

    let f = Fixture::new(prefix, "child-redrive", &host, &stores).await;
    let writer =
        lash_core::shift::StoreParkRecovery::new(f.factory.as_ref(), f.parts.host.clock.as_ref());
    let child = ParkTarget::RunChild {
        session: f.parts.session_id.clone(),
        run: f.run.clone(),
    };
    let record = |handle: &'static str, stopped: bool| {
        let (writer, child) = (&writer, &child);
        async move {
            writer
                .record_engine_park(
                    child,
                    ParkReason::engine_retry_exhausted(8, None, "the child stopped".into()),
                    EnginePark::new(handle),
                    &Execution { stopped },
                )
                .await
                .expect("record the stopped child")
        }
    };
    let children = |handles: &[&str]| -> Vec<EnginePark> {
        handles.iter().copied().map(EnginePark::new).collect()
    };
    assert_eq!(
        record("child-a", true).await,
        EngineParkRecorded::AttachedToExisting(f.park.park_id),
        "a run that parked itself gains its stopped child's handle"
    );
    let attached = f.park().await.expect("still parked");
    assert_eq!(
        attached,
        TurnPark {
            children: children(&["child-a"]),
            ..f.park.clone()
        }
    );
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    assert!(
        matches!(
            &redrive.kind,
            ControlIntentKind::Redrive { children: recorded, .. }
                if *recorded == children(&["child-a"])
        ),
        "the redrive records its park's children: {redrive:?}"
    );
    assert_eq!(
        record("child-b", true).await,
        EngineParkRecorded::Redriven,
        "an open redrive owns the run's stopped children"
    );
    assert_eq!(
        f.park().await.expect("still parked"),
        TurnPark {
            resume_intent: Some(redrive.id),
            ..attached.clone()
        },
        "a child that stops while the redrive is on its way waits for it to settle"
    );
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        *work.0.resumed_children.lock().expect("resumed children"),
        [children(&["child-a"])],
        "the redrive is handed exactly the children its park recorded"
    );
    assert_eq!(
        record("child-b", false).await,
        EngineParkRecorded::Redriven,
        "a listing read before the redrive resumed the child is stale"
    );
    let park = f.park().await.expect("still parked");
    assert_eq!(park.resume_intent, Some(redrive.id));
    assert_eq!(park.attempts, f.park.attempts);
    assert_eq!(
        record("child-b", true).await,
        EngineParkRecorded::AttachedToExisting(f.park.park_id),
        "a child still stopped after the redrive re-parks the same park"
    );
    let again = f.park().await.expect("re-parked");
    assert_eq!(again.resume_intent, None);
    assert_eq!(again.attempts, f.park.attempts + 1);
    assert_eq!(again.children, children(&["child-a", "child-b"]));
    let second = f
        .verb(RunVerb::Redrive)
        .await
        .expect("the re-parked run can be redriven again");
    assert!(matches!(
        f.apply(&work, &close, &second).await,
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        work.0
            .resumed_children
            .lock()
            .expect("resumed children")
            .last(),
        Some(&children(&["child-a", "child-b"]))
    );
}

/// A stalled execution whose probe waits at `gate` and then answers
/// stopped: a reconcile pass held between its read of the park and its write.
struct GatedExecution(Arc<Gate>);
#[async_trait::async_trait]
impl StalledExecution for GatedExecution {
    async fn still_stopped(&self) -> Result<bool, EngineRefusal> {
        self.0.pass().await;
        Ok(true)
    }
}

/// One stopped-child reconcile pass over `execution`. A [`GatedExecution`]
/// holds it at its engine probe: after it read the run's park, before it
/// writes.
pub(super) fn held_pass<'a>(
    writer: &'a lash_core::shift::StoreParkRecovery<'a>,
    child: &'a ParkTarget,
    reason: &ParkReason,
    execution: &'a dyn StalledExecution,
) -> impl Future<Output = EngineParkRecorded> + 'a {
    let reason = reason.clone();
    async move {
        writer
            .record_engine_park(child, reason, EnginePark::new("child-a"), execution)
            .await
            .expect("record the stopped child")
    }
}

/// FIG-4626: a child reconcile's write is fenced by the redrive its run's
/// park names when the write lands, not by what the pass read. Two passes
/// both read no park and probe; one parks the run; an operator's redrive is
/// admitted; then the delayed pass writes. The redrive stays open and the
/// park keeps naming it, with the one refusal the first pass counted, so the
/// redrive's engine half still runs and resumes the child.
pub async fn a_delayed_child_reconcile_never_settles_a_redrive_admitted_since_its_probe(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let admitted = AdmittedRun::new(prefix, "child-delayed", &host, &stores).await;
    let factory = stores.session_store_factory();
    let clock = Arc::clone(&admitted.parts.host.clock);
    let writer = lash_core::shift::StoreParkRecovery::new(factory.as_ref(), clock.as_ref());
    let child = ParkTarget::RunChild {
        session: admitted.parts.session_id.clone(),
        run: admitted.run.clone(),
    };
    let gate = Arc::new(Gate::new("the engine probe"));
    let execution = GatedExecution(Arc::clone(&gate));
    let first = ParkReason::engine_retry_exhausted(8, None, "the first pass".into());
    let late = ParkReason::engine_retry_exhausted(9, None, "the delayed pass".into());

    // Both passes read no park and probe the engine; the first one's write
    // lands, the delayed one is held after its probe.
    let mut delayed = std::pin::pin!(held_pass(&writer, &child, &late, &execution));
    gate.reached_by(&mut delayed, 1).await;
    let EngineParkRecorded::Parked(id) =
        held_pass(&writer, &child, &first, &Execution { stopped: true }).await
    else {
        panic!("the first pass parks the run");
    };
    let park = admitted
        .parts
        .store
        .load_turn_park(&admitted.parts.session_id)
        .await
        .expect("park")
        .expect("held");
    assert_eq!((park.park_id, park.attempts), (id, 1));
    let f = Fixture::parked(admitted, &stores, park).await;

    // The operator's redrive is admitted before its engine half runs.
    let redrive = f.verb(RunVerb::Redrive).await.expect("redrive");
    assert!(f.owed(redrive.id).await);

    // The delayed pass writes what it prepared before the redrive existed.
    gate.open_one();
    let delayed = delayed.await;
    let held = f.park().await.expect("still parked");
    assert_eq!(
        held.resume_intent,
        Some(redrive.id),
        "the delayed write leaves the redrive on the park"
    );
    assert_eq!(
        (held.park_id, held.attempts, &held.reason),
        (id, 1, &first),
        "the delayed write counts no refusal and keeps the reason"
    );
    assert!(
        f.owed(redrive.id).await,
        "a reconcile never acknowledges a redrive whose engine half has not run"
    );
    assert_eq!(
        delayed,
        EngineParkRecorded::Redriven,
        "the redrive owns the run's stopped children"
    );

    // The redrive's engine half still runs: the child resumes.
    let (work, close) = f.control(false, false);
    assert!(matches!(
        f.apply(&work, &close, &redrive).await,
        ControlIntentState::Acknowledged { .. }
    ));
    let resumes = work
        .0
        .events
        .lock()
        .expect("events")
        .iter()
        .filter(|event| **event == "resume")
        .count();
    assert_eq!(resumes, 1, "the engine resumed the run's stopped work once");
    assert_eq!(
        f.park()
            .await
            .expect("parked until its run commits")
            .resume_intent,
        Some(redrive.id)
    );
}

/// FIG-4626: two first reconciles of one run's stopped children, both past
/// their read of no park and their probe before either writes, record one
/// park: the first write opens it, and the second counts no second refusal
/// and keeps the first one's reason.
pub async fn concurrent_first_child_reconciles_park_their_run_once(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "child-first-writes", &host, &stores, 8).await;
    let run = TurnId::from("waits-on-children");
    parts.enqueue("input", Some(run.as_str())).await;
    let factory = stores.session_store_factory();
    let writer =
        lash_core::shift::StoreParkRecovery::new(factory.as_ref(), parts.host.clock.as_ref());
    let child = ParkTarget::RunChild {
        session: parts.session_id.clone(),
        run: run.clone(),
    };
    let gate = Arc::new(Gate::new("the engine probe"));
    let execution = GatedExecution(Arc::clone(&gate));
    let reasons = [
        ParkReason::engine_retry_exhausted(8, None, "one pass".into()),
        ParkReason::engine_retry_exhausted(9, None, "another pass".into()),
    ];
    let mut passes = std::pin::pin!(futures_util::future::join(
        held_pass(&writer, &child, &reasons[0], &execution),
        held_pass(&writer, &child, &reasons[1], &execution),
    ));
    gate.reached_by(&mut passes, 2).await;
    gate.open_one();
    gate.open_one();
    let recorded = passes.await;
    let park = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("park")
        .expect("held");
    assert_eq!(park.turn_id, run);
    assert_eq!(
        (park.attempts, park.since_ms, park.resume_intent),
        (1, park.last_refused_ms, None),
        "two first writes count one refusal"
    );
    let opened = reasons
        .iter()
        .position(|reason| *reason == park.reason)
        .expect("the park carries one pass's reason");
    let recorded = [recorded.0, recorded.1];
    assert_eq!(
        recorded[opened],
        EngineParkRecorded::Parked(park.park_id),
        "the pass whose write opened the park parked the run"
    );
    assert_eq!(
        recorded[1 - opened],
        EngineParkRecorded::AttachedToExisting(park.park_id),
        "the other pass found the run parked"
    );
}
