//! Restate process commands replay by their recorded outcome, not by live
//! re-execution (FIG-3827).
//!
//! A start journals its registration (ADR 0107), so its replay reads the
//! recorded id. Every other process command but `Signal` runs as a direct
//! process execution that records no outcome, and `Signal`'s eager record
//! awaits its body before the record step: a replay re-runs each against the
//! registry as it is *now*. A replay after the store moved on (a signalled
//! child pruned, a listed process ended) answers differently from the run that
//! wrote the journal, or issues different commands, which Restate refuses as a
//! journal mismatch and parks the handler.
//!
//! Each law records one command live, moves the store on, replays the same
//! command against the recorded journal and requires the recorded answer and
//! the same command sequence. The start law holds; the others fail today and
//! are ignored under FIG-3827.

use super::*;

/// Whether the registry still holds `process_id`: a pruned process reads as
/// no longer retained.
async fn is_retained(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) -> bool {
    match registry.get_process(process_id).await {
        Ok(record) => record.is_some(),
        Err(PluginError::ProcessNoLongerRetained { .. }) => false,
        Err(error) => panic!("read `{process_id}`: {error:?}"),
    }
}

/// End `process_id` and prune it, as terminal retention does.
async fn end_and_prune(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
    let ended = registry
        .complete_process(
            process_id,
            process_success(serde_json::json!({ "done": true })),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("end the process");
    registry
        .prune_terminal_processes(
            ended.updated_at_ms.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the ended process");
    assert!(
        !is_retained(registry, process_id).await,
        "the ended process is pruned"
    );
}

fn replayed(
    replay: Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>,
    what: &str,
) -> RuntimeEffectOutcome {
    replay.unwrap_or_else(|error| panic!("the replayed {what} answers as recorded: {error:?}"))
}

/// The journal commands the context recorded from `from` on.
fn commands_since(context: &ReplayableRecordingContext, from: usize) -> Vec<String> {
    context.journal_commands.lock_recover()[from..].to_vec()
}

/// The replay issued exactly the journal commands the live run did.
fn assert_same_commands(context: &ReplayableRecordingContext, live: &[String], replay_from: usize) {
    assert_eq!(
        commands_since(context, replay_from),
        live,
        "the replay issues the recorded journal commands"
    );
}

/// A signal replayed after its target was pruned answers the recorded event
/// and resolves the same wait: the append and the ordinal it keys the
/// resolution by are the recorded run's, not a live re-read of a row that is
/// gone.
#[tokio::test]
#[ignore = "FIG-3827: a replayed Signal re-appends and re-counts live; once the target is pruned it fails before its resolve_event command"]
pub(super) async fn a_signal_replayed_after_its_target_is_pruned_answers_as_recorded() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let record = registry
        .register_process(external_registration().with_extra_event_types([
            lash_core::ProcessEventType {
                name: "signal.notify".to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            },
        ]))
        .await
        .expect("register the signalled process");
    let signal = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "fig3827-signal"),
            RuntimeEffectCommand::process(ProcessCommand::Signal {
                process_id: record.id.clone(),
                signal_name: "notify".to_string(),
                signal_id: "notify".to_string(),
                request: lash_core::ProcessEventAppendRequest::new(
                    "signal.notify",
                    serde_json::json!({ "signal": "notify" }),
                )
                .with_replay_key("signal:notify"),
            }),
        )
    };
    let first = host
        .execute_effect(signal(), registry_local_executor(registry.clone()))
        .await
        .expect("the live signal");
    let live_commands = commands_since(&context, 0);
    let resolved_live = context.events.resolved_events.lock_recover().clone();
    assert_eq!(resolved_live.len(), 1, "the live signal resolves one wait");

    end_and_prune(&registry, &record.id).await;
    context.start_replay();
    let replay_from = context.journal_commands.lock_recover().len();
    let replay = host
        .execute_effect(signal(), registry_local_executor(registry.clone()))
        .await;
    let replay = replayed(replay, "signal");
    assert_same_commands(&context, &live_commands, replay_from);
    assert_eq!(
        format!("{replay:?}"),
        format!("{first:?}"),
        "the replay answers the recorded signal"
    );
    let resolved = context.events.resolved_events.lock_recover().clone();
    assert_eq!(
        resolved.len(),
        2,
        "the replay issues the recorded resolve_event command again"
    );
    assert_eq!(
        resolved[1].key, resolved_live[0].key,
        "the replay resolves the recorded wait key"
    );
}

/// A listing replayed after a listed process ended answers the recorded
/// entries: the body that branched on them takes the same path.
#[tokio::test]
#[ignore = "FIG-3827: a replayed List re-reads the registry live and answers the current listing"]
pub(super) async fn a_list_replayed_after_a_listed_process_ended_answers_as_recorded() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let scope = lash_core::SessionScope::new("fig3827-list-session");
    let listed = registry
        .register_process(external_registration())
        .await
        .expect("register the listed process")
        .id;
    registry
        .add_observer(
            &scope.session_id,
            &listed,
            lash_core::ProcessObserverBy::host("fig3827-list"),
        )
        .await
        .expect("observe the listed process");
    let list = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "fig3827-list"),
            RuntimeEffectCommand::process(ProcessCommand::List {
                session_scope: scope.clone(),
                mode: lash_core::ProcessListMode::Live,
            }),
        )
    };
    let first = host
        .execute_effect(list(), registry_local_executor(registry.clone()))
        .await
        .expect("the live listing");
    let live_commands = commands_since(&context, 0);
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::List { entries },
    } = &first
    else {
        panic!("a list outcome: {first:?}");
    };
    assert_eq!(entries.len(), 1, "the live listing names the process");

    registry
        .complete_process(
            &listed,
            process_success(serde_json::json!({ "done": true })),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("the listed process ends");
    context.start_replay();
    let replay_from = context.journal_commands.lock_recover().len();
    let replay = host
        .execute_effect(list(), registry_local_executor(registry.clone()))
        .await;
    let replay = replayed(replay, "listing");
    assert_same_commands(&context, &live_commands, replay_from);
    assert_eq!(
        format!("{replay:?}"),
        format!("{first:?}"),
        "the replay answers the recorded listing"
    );
}

/// A start replayed after its child ended and was pruned answers the recorded
/// start and issues the recorded submission: it never registers the child
/// again.
#[tokio::test]
pub(super) async fn a_start_replayed_after_its_child_is_pruned_answers_as_recorded() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    context
        .defer_process_workflows
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let start = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "fig3827-start"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: external_registration().with_start_key(Some(
                    lash_core::StartKey::for_host(
                        lash_core::StartKeyOwner::HOST,
                        "fig3827-started-child",
                    ),
                )),
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        )
    };
    let first = host
        .execute_effect(start(), registry_local_executor(registry.clone()))
        .await
        .expect("the live start");
    let live_commands = commands_since(&context, 0);
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = &first
    else {
        panic!("a start outcome: {first:?}");
    };
    let child = record.id.clone();

    end_and_prune(&registry, &child).await;
    context.start_replay();
    let replay_from = context.journal_commands.lock_recover().len();
    let replay = host
        .execute_effect(start(), registry_local_executor(registry.clone()))
        .await;
    let replay = replayed(replay, "start");
    assert_same_commands(&context, &live_commands, replay_from);
    assert_eq!(
        format!("{replay:?}"),
        format!("{first:?}"),
        "the replay answers the recorded start"
    );
    assert!(
        !is_retained(&registry, &child).await,
        "the replay never registers the pruned child again"
    );
}

/// An observer transfer replayed after the transferred process was pruned
/// answers the recorded transfer.
#[tokio::test]
#[ignore = "FIG-3827: a replayed Transfer re-runs its observer writes live against the current registry"]
pub(super) async fn a_transfer_replayed_after_its_process_is_pruned_answers_as_recorded() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let from = lash_core::SessionScope::new("fig3827-transfer-from");
    let to = lash_core::SessionScope::new("fig3827-transfer-to");
    let moved = registry
        .register_process(external_registration())
        .await
        .expect("register the transferred process")
        .id;
    registry
        .add_observer(
            &from.session_id,
            &moved,
            lash_core::ProcessObserverBy::host("fig3827-transfer"),
        )
        .await
        .expect("observe the transferred process");
    let transfer = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "fig3827-transfer"),
            RuntimeEffectCommand::process(ProcessCommand::Transfer {
                from_scope: from.clone(),
                to_scope: to.clone(),
                process_ids: vec![moved.clone()],
            }),
        )
    };
    let first = host
        .execute_effect(transfer(), registry_local_executor(registry.clone()))
        .await
        .expect("the live transfer");
    let live_commands = commands_since(&context, 0);

    end_and_prune(&registry, &moved).await;
    context.start_replay();
    let replay_from = context.journal_commands.lock_recover().len();
    let replay = host
        .execute_effect(transfer(), registry_local_executor(registry.clone()))
        .await;
    let replay = replayed(replay, "transfer");
    assert_same_commands(&context, &live_commands, replay_from);
    assert_eq!(
        format!("{replay:?}"),
        format!("{first:?}"),
        "the replay answers the recorded transfer"
    );
}
