use super::*;
use pretty_assertions::assert_eq;

/// Crash one scripted turn at `entry`'s point on the tier's runner, recover it
/// with the tier's next run of the same scope, and assert the
/// ruled durable end state.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn run_crash_matrix_case(
    law: &MatrixLaw<'_>,
    entry: &TurnCrashOutcome,
    scenario: &str,
) {
    let make = law.make;
    let identity = ReferenceIdentity::for_scenario(scenario);
    let admitted = reference_admitted_scope(&identity);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let crash = crash_at_armed_point(&control);
    let point = entry.point.clone();
    law.runner
        .run_turn_until_crash(
            admitted.clone(),
            ReferenceTurn::new(
                law.stores,
                raw,
                law.host,
                &identity,
                control,
                &executions,
                crashed_turn_timings(),
            )
            .before_drive(move |control| control.arm(point.clone()))
            .attempt(),
            crash,
        )
        .await;

    let successor_store = make(scenario);
    let (successor, recovered) = ReferenceTurn::new(
        law.stores,
        Arc::clone(&successor_store),
        law.host,
        &identity,
        SeamControl::default(),
        &executions,
        nominal_recovery_timings(),
    )
    .before_drive(SeamControl::clear)
    .reporting();
    law.runner.run_turn(admitted.clone(), successor).await;
    let recovered = reference_turn::reported(recovered)
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran);
    recovered
        .unwrap_or_else(|error| panic!("successor failed for {scenario} ({entry:?}): {error}"));

    let reader = make(scenario);
    super::super::admit_conformance_session(&reader, &identity.session_id).await;
    let recovered_pending = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list pending inputs");
    let deferred_texts: Vec<String> = if recovered_pending.is_empty() {
        Vec::new()
    } else {
        assert!(
            deferred_to_next_turn(&recovered_pending),
            "{scenario} ({entry:?}): all input claims settle exactly once or defer to the next \
             turn; pending={recovered_pending:?}"
        );
        let texts: Vec<String> = recovered_pending.iter().map(pending_input_text).collect();
        // Deferral is tolerated only for the demoted active-turn input. A
        // seeded next-turn input showing up here would mean the recovered turn
        // failed to deliver work it was never blocked on, which the state-only
        // check above cannot tell apart from the designed deferral.
        assert!(
            texts
                .iter()
                .all(|text| text.as_str() == "active checkpoint input"),
            "{scenario} ({entry:?}): only the demoted active-turn input may defer to the next \
             turn; pending={recovered_pending:?}"
        );
        texts
    };
    // The recovered root is one engine admission. Work left in the other
    // ingress table belongs to a later admission, including a terminal
    // follow-on or a deferred active-turn input.
    let mut drain_turns = 0;
    loop {
        let reader = make(scenario);
        super::super::admit_conformance_session(&reader, &identity.session_id).await;
        let pending = reader
            .list_pending_turn_inputs(&identity.session_id)
            .await
            .expect("read pending inputs before follow-on drive");
        let queued = reader
            .list_queued_work(&identity.session_id)
            .await
            .expect("read queued work before follow-on drive");
        if pending.is_empty() && queued.is_empty() {
            break;
        }
        assert!(
            drain_turns < 3,
            "{scenario}: follow-on drive made no progress; pending={pending:?}; queued={queued:?}"
        );
        drain_turns += 1;
        Box::pin(drive_drain_turn(
            law,
            scenario,
            &identity,
            &executions,
            drain_turns,
        ))
        .await;
    }

    let state = crate::conformance::helpers::load_window_state(&reader, &identity.session_id)
        .await
        .expect("read recovered state")
        .expect("recovered turn commits state");
    let read_model = state.session_graph.read_model();
    let part_count = |content: &str| {
        read_model
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content() == content)
            .count()
    };
    assert_eq!(
        part_count("trace turn complete"),
        state.turn_index,
        "{scenario} ({entry:?}): every committed physical turn adds one terminal assistant output"
    );
    let wake_count = read_model
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.content().ends_with("Wake input:\ntrace-source"))
        .count();
    assert_eq!(
        wake_count, 1,
        "{scenario} ({entry:?}): wake input is delivered once"
    );
    for text in &deferred_texts {
        assert_eq!(
            part_count(text),
            1,
            "{scenario} ({entry:?}): the deferred input is delivered exactly once by the next turn"
        );
    }
    let pending_inputs = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list pending inputs");
    assert!(
        pending_inputs.is_empty(),
        "{scenario} ({entry:?}): all input claims settle exactly once; pending={pending_inputs:?}"
    );
    let queued = reader
        .list_queued_work(&identity.session_id)
        .await
        .expect("list queued work");
    assert!(
        queued.is_empty(),
        "{scenario} ({entry:?}): queued-work claim settles exactly once; queued={queued:?}"
    );

    assert!(
        reader
            .unfinished_root(&identity.session_id)
            .await
            .expect("read the recovered unfinished root")
            .is_none(),
        "{scenario} ({entry:?}): recovery ends every admitted root"
    );
    let effect_count = executions.load(std::sync::atomic::Ordering::SeqCst);
    // The successor replays every effect its predecessor completed from the
    // journal; only an effect whose outcome was lost after it ran externally
    // executes a second time.
    let expected_effect_count = usize::from(matches!(
        entry.point.placement,
        CrashPlacement::AfterExternalEffectBeforeOutcome
    )) + 1
        + drain_turns * DRAIN_TURN_EFFECT_EXECUTIONS;
    assert_eq!(
        effect_count, expected_effect_count,
        "{scenario}: {}",
        entry.outcome
    );
}

/// Drive one further clean turn to absorb inputs the recovered turn deferred to
/// the next turn.
async fn drive_drain_turn(
    law: &MatrixLaw<'_>,
    scenario: &str,
    identity: &ReferenceIdentity,
    executions: &Arc<std::sync::atomic::AtomicUsize>,
    ordinal: usize,
) {
    // The drain turn is a new turn, not a recovery of the crashed one, so it
    // gets its own turn identity: reusing the recovered turn's id would collide
    // with the history nodes that turn already committed.
    let identity = ReferenceIdentity {
        session_id: identity.session_id.clone(),
        turn_id: crate::TurnId::from(format!("{}:drain:{ordinal}", identity.turn_id)),
    };
    let (drain, drained) = ReferenceTurn::new(
        law.stores,
        (law.make)(scenario),
        law.host,
        &identity,
        SeamControl::default(),
        executions,
        nominal_recovery_timings(),
    )
    .before_drive(SeamControl::clear)
    .reporting();
    law.runner
        .run_turn(reference_admitted_scope(&identity), drain)
        .await;
    reference_turn::reported(drained)
        .await
        .unwrap_or_else(|error| panic!("drain turn failed for {scenario}: {error}"));
}
