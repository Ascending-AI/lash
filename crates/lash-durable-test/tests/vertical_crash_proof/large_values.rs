//! The large-value transmission law (PC-LARGE-ONCE, FIG-5887 §11) on the
//! simulated deployment: what each park transmits of a kept large text.

use super::*;

/// PC-LARGE-ONCE (FIG-5887 §11, the FIG-5880 amplification): a cell keeps a
/// [`PAD`]-byte text across each of [`AWAITS`] awaits. Every park installs
/// what changed since the last, never again the large text an earlier one
/// installed: a repeated text is installed once, a fresh one once per
/// await, and no park transmits an earlier park's large text again. After
/// the turn and an immediate collection, the session's execution state
/// hydrates whole.
async fn large_values_install_once(pad: Pad) {
    let uncut = V0::new(Protocol::Large(pad), Dialect::SqliteMemory, None);
    let clock = SimClock::new();
    let database = uncut.database(Arc::clone(&clock)).await;
    let script = Script::new();
    let installs = Arc::new(Mutex::new(Transmissions::new(pad)));
    script.observe_commits({
        let installs = Arc::clone(&installs);
        Arc::new(move |label, tx| installs.lock_recover().observe(label, tx))
    });
    script.observe_candidates({
        let installs = Arc::clone(&installs);
        Arc::new(move |_, _, pages| installs.lock_recover().observe_candidate(pages))
    });
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        uncut.config(),
        uncut.activation(),
    );
    uncut.send().await.expect("the turn is sent");
    nodes.start("a");
    let horizon = clock.logical_ms() + 600_000;
    while !uncut.done(&nodes).await {
        if let Ok(Some(snapshot)) = database.actor(&actor()).await
            && snapshot.state == ActorState::Parked
        {
            panic!(
                "{pad:?}: the session parked: {:?}\n{}",
                facade_park(&uncut).await,
                nodes.script().rendered_trace()
            );
        }
        assert!(
            clock.logical_ms() < horizon && nodes.step().await.is_some(),
            "{pad:?}: the turn stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;

    let (parks, literal, large, payload) = {
        let sent = installs.lock_recover();
        (
            sent.parks.clone(),
            sent.literal.clone(),
            sent.large.clone(),
            sent.payload.clone(),
        )
    };
    // The cell's source holds the pad once; the heap pages each text the
    // cell keeps: the pad itself, and for a fresh text each pad+i,
    // including the last one, captured at the cell's end after its final
    // await and published by the turn's working commit as the session's
    // candidate (FIG-6418).
    assert_eq!(
        literal.len(),
        1,
        "{pad:?}: the cell's program stages its literal text: {literal:?}"
    );
    let expected_large = if pad == Pad::Repeated { 1 } else { AWAITS + 1 };
    assert_eq!(
        large.len(),
        expected_large,
        "{pad:?}: the heap pages its known large texts: {large:?}"
    );
    assert!(
        literal
            .values()
            .chain(large.values())
            .all(|commits| commits.len() == 1),
        "{pad:?}: each large text is transmitted once, including non-park commits: {literal:?} {large:?}"
    );
    assert!(
        payload.values().all(|commits| commits.len() == 1),
        "{pad:?}: each page of a paged text is transmitted once: {payload:?}"
    );
    assert_eq!(
        parks.len(),
        AWAITS,
        "{pad:?}: one park per await: {parks:?}"
    );
    assert!(
        parks[0].1 >= PAD,
        "{pad:?}: the first park installs the large text: {parks:?}"
    );
    // A later park installs its new admission, the host's summary and the
    // heap pages the await dirtied, each up to a leaf's target size, plus,
    // for a fresh text, that one text: the pages the earlier texts do not
    // share. It stays under one large text, so it never holds an earlier
    // park's large text again.
    let bound = match pad {
        Pad::Repeated => PAD,
        Pad::Fresh => PAD + PAD / 2,
    };
    for (index, (commit, bytes)) in parks.iter().enumerate().skip(1) {
        assert!(
            *bytes < bound,
            "{pad:?}: park {} installs {bytes} bytes, not under {bound}: an earlier park's large text again: {parks:?}",
            index + 1
        );
        if pad == Pad::Fresh {
            assert!(
                large
                    .values()
                    .any(|commits| commits.iter().any(|(_, at)| at == commit)),
                "{pad:?}: park {} installs its fresh text: {parks:?} {large:?}",
                index + 1
            );
        }
    }

    let catalog = uncut.backend().session_store_factory();
    let execution = |when: &'static str| {
        let catalog = Arc::clone(&catalog);
        async move {
            let view = lash_core::store::SessionStore::new(catalog, session())
                .expect("the session's store");
            lash_core::store::load_session_window_state(
                &view,
                lash_core::store::WindowSelector::Current,
            )
            .await
            .unwrap_or_else(|error| panic!("{pad:?}: the head loads {when}: {error:?}"))
            .expect("the session has a head")
            .state
            .execution_state_hydration()
            .unwrap_or_else(|error| {
                panic!("{pad:?}: the execution state hydrates {when}: {error:?}")
            })
        }
    };
    let before_gc = execution("after the turn").await;
    assert!(
        before_gc.is_some(),
        "{pad:?}: the cell's globals are the session's execution state"
    );
    lash_core::StoreMaintenance::gc_unreachable(catalog.as_ref())
        .await
        .expect("the collection runs");
    assert_eq!(
        execution("after an immediate collection").await,
        before_gc,
        "{pad:?}: collection preserves the pre-GC execution state",
    );
}

#[tokio::test]
async fn a_repeated_large_text_kept_across_awaits_is_written_once() {
    large_values_install_once(Pad::Repeated).await;
}

#[tokio::test]
async fn a_fresh_large_text_per_await_is_written_once_per_await() {
    large_values_install_once(Pad::Fresh).await;
}
