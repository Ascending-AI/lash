use super::*;
use pretty_assertions::assert_eq;

/// Crash one scripted turn at `entry`'s point on the tier's runner, recover it
/// with the tier's next run of the same scope under `pressure`, and assert the
/// ruled durable end state.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn run_crash_matrix_case(
    law: &MatrixLaw<'_>,
    entry: &TurnCrashOutcome,
    scenario: &str,
    pressure: RenewalPressure,
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

    let predecessor_claimed = !matches!(
        (&entry.point.operation, entry.point.placement),
        (
            TurnSeamOperation::Store(StoreOperation::ClaimSessionExecutionLease),
            CrashPlacement::Boundary
        ) | (
            TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                releases_lease: true,
                ..
            }),
            CrashPlacement::InsideCall
        ) | (
            TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease),
            CrashPlacement::InsideCall
        )
    );
    wait_for_recovery_lease(&make, scenario, &entry.point, predecessor_claimed).await;
    let successor_store = make(scenario);
    let successor_timings = match pressure {
        RenewalPressure::Nominal => nominal_recovery_timings(),
        RenewalPressure::Starved => recovery_timings(),
    };
    let (successor, recovered) = ReferenceTurn::new(
        law.stores,
        Arc::clone(&successor_store),
        law.host,
        &identity,
        SeamControl::default(),
        &executions,
        successor_timings,
    )
    .before_drive(move |control| {
        control.clear();
        if pressure == RenewalPressure::Starved {
            control.starve_renewals();
        }
    })
    .reporting();
    law.runner.run_turn(admitted.clone(), successor).await;
    let recovered = reference_turn::reported(recovered)
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran);
    if pressure == RenewalPressure::Starved {
        let error = recovered.expect_err("a lapsed lane cannot commit the admitted run");
        assert_eq!(error.code, crate::RuntimeErrorCode::QueuedRunPending);
        assert!(
            error.is_retryable(),
            "lease loss keeps this run recoverable"
        );
        let admission = successor_store
            .pending_queued_run(&identity.session_id)
            .await
            .expect("read run retained after lease loss")
            .expect("a starved attempt retains durable ownership");
        assert_eq!(admission.scope.id(), identity.turn_id.as_str());
        assert_eq!(admission.position.physical_ordinal, 0);
        assert_eq!(admission.position.turn_id, identity.turn_id);
        assert!(
            admission
                .members
                .as_ref()
                .is_some_and(|members| !members.is_empty())
        );
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the starved attempt crosses the effect before losing its commit lane"
        );
        // The starved run aborted without an outcome; the tier's next run of
        // the scope redrives it.
        let (redrive, redriven) = ReferenceTurn::new(
            law.stores,
            make(scenario),
            law.host,
            &identity,
            SeamControl::default(),
            &executions,
            nominal_recovery_timings(),
        )
        .before_drive(SeamControl::clear)
        .reporting();
        law.runner.run_turn(admitted, redrive).await;
        reference_turn::reported(redriven)
            .await
            .unwrap_or_else(|error| panic!("starved run redrive failed for {scenario}: {error}"));
    } else {
        recovered
            .unwrap_or_else(|error| panic!("successor failed for {scenario} ({entry:?}): {error}"));
    }

    let reader = make(scenario);
    super::super::bind_conformance_session(&reader, &identity.session_id).await;
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
    let drain_turns = usize::from(!deferred_texts.is_empty());
    if drain_turns == 1 {
        Box::pin(drive_drain_turn(law, scenario, &identity, &executions)).await;
    }

    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read recovered state")
        .expect("recovered turn commits state");
    let read_model = state.session_graph.read_model(None).unwrap();
    let part_count = |content: &str| {
        read_model
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content() == content)
            .count()
    };
    // FIG-3157: queued work claimed at the terminal checkpoint no longer
    // re-prompts the finishing turn. It is withheld from that delivery and
    // drives a follow-on turn of the same logical run, so each seeded batch
    // that lands renders its own terminal output instead of replacing one.
    // The seeded work is a process wake, rendered as its event with the wake
    // input last. A wake claimed at a mid-turn checkpoint is delivered into
    // that turn; one withheld at the terminal checkpoint opens its own turn,
    // so its event directly follows a finished turn's output.
    let messages: Vec<(bool, bool)> = read_model
        .messages
        .iter()
        .map(|message| {
            let content = |matches: fn(&str) -> bool| {
                message.parts.iter().any(|part| matches(&part.content()))
            };
            (
                content(|text| text == "trace turn complete"),
                content(|text| text.ends_with("Wake input:\ntrace-source")),
            )
        })
        .collect();
    let terminal_follow_on_turns = messages
        .windows(2)
        .filter(|pair| pair[0].0 && pair[1].1)
        .count();
    assert_eq!(
        part_count("trace turn complete"),
        1 + drain_turns + terminal_follow_on_turns,
        "{scenario} ({entry:?}): recovery must expose one terminal assistant output per turn"
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
    assert!(
        reader
            .list_queued_work(&identity.session_id)
            .await
            .expect("list queued work")
            .is_empty(),
        "{scenario} ({entry:?}): queued-work claim settles exactly once"
    );

    assert!(
        reader
            .pending_queued_run(&identity.session_id)
            .await
            .expect("read recovered queued admission")
            .is_none(),
        "{scenario} ({entry:?}): recovery settles durable queued-run ownership"
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
) {
    // The drain turn is a new turn, not a recovery of the crashed one, so it
    // gets its own turn identity: reusing the recovered turn's id would collide
    // with the history nodes that turn already committed.
    let identity = ReferenceIdentity {
        session_id: identity.session_id.clone(),
        turn_id: crate::TurnId::from(format!("{}:drain", identity.turn_id)),
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
