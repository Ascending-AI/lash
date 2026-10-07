//! The relay driver's commit decision (FIG-4441): whether an executed step
//! commits its baton, or keeps only its attempt record and says why.

use super::*;

use crate::relay::{NEXT_TOOL, NOT_COMMITTED_NOTE, RelayNext, RelaySettings};

/// Interpret an executed relay step: commit it, or keep only its attempt
/// record and the reason nothing else was kept.
pub(super) fn relay_exec_result(
    ctx: &DriverContextView<'_>,
    mut state: RlmDriverState,
    result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    settings: RelaySettings,
) -> Vec<DriverAction> {
    let transport_version = lash_core::driver_writer_version!(ctx, NATIVE_TRANSPORT_VERSION);
    let version = lash_core::driver_writer_version!(ctx, crate::RLM_PROTOCOL_EVENT_VERSION);
    let mut actions = Vec::new();
    let decision = match result {
        Ok(response) => {
            let outcome = CellOutcome::from_parts(response.error, response.terminal_finish);
            if !response.degraded_bindings.is_empty() {
                actions.push(DriverAction::AppendEvents(vec![diagnostic_event(
                    "projection_rehydration",
                    serde_json::json!({
                        "degraded_bindings": response.degraded_bindings,
                    }),
                    version,
                )]));
            }
            let terminal_outcome = response
                .calls
                .iter()
                .filter_map(|call| call.host_record.as_ref())
                .find_map(terminal_outcome_from_tool_result);
            let (host_records, omitted) = bounded_exec_tool_call_records(&response.calls);
            actions.extend(
                host_records
                    .into_iter()
                    .map(tool_call_event)
                    .map(DriverAction::Emit),
            );
            if let Some(summary) = omitted {
                actions.push(DriverAction::Emit(SessionStreamEvent::ToolCallsOmitted {
                    summary,
                }));
            }
            (state.calls, state.calls_omitted) = executed_call_ledger(&response.calls);
            state.images.extend(response.printed_images);
            state.output_archive = response.output_archive;
            for observation in response.observations {
                state.output.push(lash_rlm_types::RlmPrint {
                    text: observation.text,
                    value: observation.value,
                });
            }
            if !matches!(outcome, CellOutcome::Running) {
                state.outcome = outcome;
            }
            // A host terminal (a cancelled call, a failing or aborting
            // control) ends the turn as it does in a chronological session.
            if let Some(outcome) = terminal_outcome {
                actions.push(DriverAction::AppendEvents(trajectory_events(
                    ctx.turn_id(),
                    ctx.protocol_iteration(),
                    &state,
                    None,
                    transport_version,
                    version,
                )));
                actions.push(DriverAction::Start(PendingWork::Checkpoint {
                    checkpoint: CheckpointKind::BeforeCompletion,
                    on_empty: CheckpointResumeAction::Finish(outcome),
                }));
                return actions;
            }
            read_commit(&response.calls, &state.outcome, settings)
        }
        Err(failure) => {
            state.outcome = CellOutcome::Failed(failure.into());
            Err(None)
        }
    };
    // A `finish(value)` the step called records as a run step (the entry
    // never carries a pending finish); the note says how a relay turn ends.
    let mut events = trajectory_events(
        ctx.turn_id(),
        ctx.protocol_iteration(),
        &state,
        None,
        transport_version,
        version,
    );
    let next = match decision {
        Ok(next) => next,
        Err(reason) => {
            if let Some(reason) = reason {
                events.push(conversation_event(not_committed_note(
                    rlm_message_id(ctx.turn_id(), ctx.protocol_iteration(), NOT_COMMITTED_NOTE),
                    reason,
                )));
            }
            if let Err(err) = continue_or_stop_after_nonterminal(
                ctx,
                &mut actions,
                events,
                Vec::new(),
                AttemptProgress::Stalled,
            ) {
                return invalid_turn_options_actions(err);
            }
            return actions;
        }
    };
    events.push(SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmSeed(next.seed_body()),
        version,
    )));
    if let Err(err) = continue_or_stop_after_nonterminal(
        ctx,
        &mut actions,
        events,
        Vec::new(),
        AttemptProgress::Executed,
    ) {
        return invalid_turn_options_actions(err);
    }
    actions
}

/// The baton an executed step commits, or why it commits none: `Err(None)`
/// when the step's own failure says it, `Err(Some(reason))` when a relay rule
/// does.
fn read_commit(
    calls: &[lash_core::ExecutedCall],
    outcome: &CellOutcome<lash_core::CellFailure>,
    settings: RelaySettings,
) -> Result<RelayNext, Option<String>> {
    match outcome {
        CellOutcome::Failed(_) => return Err(None),
        CellOutcome::Finished(_) => {
            return Err(Some(
                "finish() does not end a relay step; to answer, reply in plain text with no tool call".to_string(),
            ));
        }
        CellOutcome::Running => {}
    }
    let records = calls
        .iter()
        .filter_map(|call| call.host_record.as_ref())
        .collect::<Vec<_>>();
    let is_next = |record: &ToolCallRecord| record.tool == NEXT_TOOL && record.output.is_success();
    let nexts = records.iter().filter(|record| is_next(record)).count();
    let Some(position) = records.iter().rposition(|record| is_next(record)) else {
        return Err(Some(
            "the program never called control.next successfully".to_string(),
        ));
    };
    if nexts > 1 || position + 1 != records.len() {
        return Err(Some(
            "control.next must be the program's last call, and its only one".to_string(),
        ));
    }
    RelayNext::from_args(&records[position].args, settings.context_budget_chars)
        .map_err(|error| Some(format!("next refused: {error}")))
}

/// Why a relay step did not commit, for the next step message's status.
fn not_committed_note(id: String, reason: String) -> Message {
    Message {
        id: id.clone(),
        role: lash_core::session_model::MessageRole::System,
        parts: lash_core::session_model::shared_parts(vec![lash_core::session_model::Part::text(
            format!("{id}.p0"),
            reason,
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    }
}
