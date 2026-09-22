use super::*;
use pretty_assertions::assert_eq;

/// Crash one scripted turn at `entry`'s point, recover it with a successor
/// turn under `pressure`, and assert the ruled durable end state.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn run_crash_matrix_case<F, I>(
    make: &F,
    make_invocation: &I,
    entry: &TurnCrashOutcome,
    scenario: &str,
    pressure: RenewalPressure,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
    I: Fn(&str) -> super::super::ConformanceInvocation,
{
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let invocation = make_invocation(scenario);
    let effect_redrive = invocation.effect_redrive();
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
    });
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&effect_controller),
        &identity,
        TraceTool::default(),
    ))
    .await;
    control.arm(entry.point.clone());
    let task_identity = identity.clone();
    let task = crate::task::spawn(async move {
        Box::pin(drive_turn(runtime, effect_controller, &task_identity)).await
    });
    control.wait_for_hit().await;
    control.simulate_process_crash();
    task.abort();
    let _ = task.await;
    let successor_invocation = invocation.redrive();

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
    wait_for_recovery_lease(make, scenario, &entry.point, predecessor_claimed).await;
    let successor_control = SeamControl::default();
    let successor_store = SeamStore::wrap(make(scenario), successor_control.clone());
    let successor_timings = match pressure {
        RenewalPressure::Nominal => nominal_recovery_timings(),
        RenewalPressure::Starved => recovery_timings(),
    };
    let successor_effect_controller: Arc<dyn RuntimeEffectController> =
        Arc::new(SeamEffectController {
            inner: successor_invocation.controller_handle(),
            control: successor_control.clone(),
            executions: Arc::clone(&executions),
        });
    let successor = Box::pin(build_runtime_with_lease_timings(
        Arc::clone(&successor_store),
        successor_control.clone(),
        Arc::clone(&successor_effect_controller),
        &identity,
        TraceTool::default(),
        successor_timings,
    ))
    .await;
    successor_control.clear();
    if pressure == RenewalPressure::Starved {
        successor_control.starve_renewals();
    }
    let recovered = Box::pin(drive_turn(
        successor,
        successor_effect_controller,
        &identity,
    ))
    .await;
    let starved_reexecution = if pressure == RenewalPressure::Starved {
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
        let redrive = successor_invocation.redrive();
        let control = SeamControl::default();
        let controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
            inner: redrive.controller_handle(),
            control: control.clone(),
            executions: Arc::clone(&executions),
        });
        let runtime = Box::pin(build_runtime_with_lease_timings(
            SeamStore::wrap(make(scenario), control.clone()),
            control,
            Arc::clone(&controller),
            &identity,
            TraceTool::default(),
            nominal_recovery_timings(),
        ))
        .await;
        Box::pin(drive_turn(runtime, controller, &identity))
            .await
            .unwrap_or_else(|error| panic!("starved run redrive failed for {scenario}: {error}"));
        redrive.end();
        usize::from(matches!(
            effect_redrive,
            super::super::ConformanceEffectRedrive::ReexecutesUncommitted
        ))
    } else {
        recovered
            .unwrap_or_else(|error| panic!("successor failed for {scenario} ({entry:?}): {error}"));
        successor_invocation.end();
        0
    };

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
        Box::pin(drive_drain_turn(
            make,
            make_invocation,
            scenario,
            &identity,
            &executions,
        ))
        .await;
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
    let terminal_follow_on_turns = part_count("trace-source");
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
    let expected_effect_count = match effect_redrive {
        super::super::ConformanceEffectRedrive::ReplaysJournal => {
            usize::from(matches!(
                entry.point.placement,
                CrashPlacement::AfterExternalEffectBeforeOutcome
            )) + 1
        }
        super::super::ConformanceEffectRedrive::ReexecutesUncommitted => entry.effect_executions_l1,
    } + starved_reexecution
        + drain_turns * DRAIN_TURN_EFFECT_EXECUTIONS;
    assert_eq!(
        effect_count, expected_effect_count,
        "{scenario}: {}",
        entry.outcome
    );
}

/// Drive one further clean turn to absorb inputs the recovered turn deferred to
/// the next turn.
async fn drive_drain_turn<F, I>(
    make: &F,
    make_invocation: &I,
    scenario: &str,
    identity: &ReferenceIdentity,
    executions: &Arc<std::sync::atomic::AtomicUsize>,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
    I: Fn(&str) -> super::super::ConformanceInvocation,
{
    // The drain turn is a new turn, not a recovery of the crashed one, so it
    // gets its own turn identity: reusing the recovered turn's id would collide
    // with the history nodes that turn already committed.
    let identity = ReferenceIdentity {
        session_id: identity.session_id.clone(),
        turn_id: crate::TurnId::from(format!("{}:drain", identity.turn_id)),
    };
    let identity = &identity;
    let control = SeamControl::default();
    let store = SeamStore::wrap(make(scenario), control.clone());
    let invocation = make_invocation(&identity.turn_id);
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(executions),
    });
    let runtime = Box::pin(build_runtime_with_lease_timings(
        store,
        control.clone(),
        Arc::clone(&effect_controller),
        identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await;
    control.clear();
    let _ = Box::pin(drive_turn(runtime, effect_controller, identity))
        .await
        .unwrap_or_else(|error| panic!("drain turn failed for {scenario}: {error}"));
    invocation.end();
}
