//! The relay driver's two decisions (FIG-4441): what a reply without a cell
//! gets, and whether an executed step commits.

use super::*;

use crate::relay::{NEXT_TOOL, RelayNext, RelaySettings, SEND_USER_OUTPUT_TOOL};

/// A relay reply's class. A cell runs; anything else is a repair round whose
/// prose is not kept, because prose is never relay output.
pub(super) fn classify_relay_reply<'a>(
    dialect: &SessionDialect,
    attempt: &AttemptContext<'_>,
    extraction: Result<Option<CellExtraction>, CellExtractionError>,
    terminal_reason: LlmTerminalReason,
) -> ReplyClass<'a> {
    let tags = dialect.cell_tags();
    let (decision, copy) = match extraction {
        Ok(Some(cell)) => return ReplyClass::Cell(cell),
        Err(CellExtractionError::UnclosedCell)
            if terminal_reason == LlmTerminalReason::OutputLimit =>
        {
            (
                "relay_retry_output_limit_cell",
                dialect.output_limit_cell_copy(attempt.output_token_cap),
            )
        }
        Err(error) => (
            "relay_retry_unclosed_cell",
            dialect.cell_error_message(error),
        ),
        Ok(None) => (
            "relay_request_cell",
            format!(
                "No program ran, so this step committed nothing. Every step is one program between `{}` and `{}` on their own lines, ending with `await control.next({{ context, vars }})`. Prose outside it is never shown to the user. To answer, ask a question or say you are blocked, send it with `control.send_user_output` and end the step with `await control.next({{ context, final: true }})`.",
                tags.open, tags.close
            ),
        ),
    };
    ReplyClass::Repair(Box::new(RepairPrompt {
        decision,
        assistant_message: None,
        correction: relay_note(attempt.message_id("relay_no_cell"), copy),
    }))
}

/// One committed step: its baton and the outputs it delivers.
struct RelayCommit {
    next: RelayNext,
    outputs: Vec<String>,
}

/// Interpret an executed relay step: commit it, or keep only its attempt
/// record and say why nothing else was kept.
pub(super) fn relay_exec_result(
    ctx: &DriverContextView<'_>,
    mut state: RlmDriverState,
    result: Result<ExecResponse, lash_core::ExecCodeFailure>,
    settings: RelaySettings,
) -> Vec<DriverAction> {
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
        version,
    );
    let commit = match decision {
        Ok(commit) => commit,
        Err(note) => {
            let retry = note
                .map(|note| {
                    vec![conversation_event(relay_note(
                        rlm_message_id(
                            ctx.turn_id(),
                            ctx.protocol_iteration(),
                            "relay_not_committed",
                        ),
                        note,
                    ))]
                })
                .unwrap_or_default();
            if let Err(err) = continue_or_stop_after_nonterminal(
                ctx,
                &mut actions,
                events,
                retry,
                AttemptProgress::Stalled,
            ) {
                return invalid_turn_options_actions(err);
            }
            return actions;
        }
    };
    events.push(SessionHistoryRecord::Protocol(rlm_protocol_event(
        RlmProtocolEvent::RlmSeed(commit.next.seed_body()),
        version,
    )));
    let step = ctx.protocol_iteration() + 1;
    for (index, text) in commit.outputs.iter().enumerate() {
        events.push(conversation_event(
            internal_assistant_prose_message_for_turn(
                ctx.turn_id(),
                format!("{}.{step}.{}", ctx.turn_id(), index + 1),
                text.clone(),
                &[],
            ),
        ));
    }
    if commit.next.final_turn {
        actions.push(DriverAction::AppendEvents(events));
        actions.push(DriverAction::Start(PendingWork::Checkpoint {
            checkpoint: CheckpointKind::BeforeCompletion,
            on_empty: CheckpointResumeAction::Finish(TurnOutcome::Finished(
                TurnFinish::AssistantMessage {
                    text: commit.outputs.join("\n\n"),
                },
            )),
        }));
        return actions;
    }
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

/// The commit an executed step earns, or why it earns none: `Err(None)` when
/// the step's own failure says it, `Err(Some(note))` when a relay rule does.
fn read_commit(
    calls: &[lash_core::ExecutedCall],
    outcome: &CellOutcome<lash_core::CellFailure>,
    settings: RelaySettings,
) -> Result<RelayCommit, Option<String>> {
    match outcome {
        CellOutcome::Failed(_) => return Err(None),
        CellOutcome::Finished(_) => {
            return Err(Some(
                "`finish(...)` does not end a relay step, so nothing from it was kept. End every step with `await control.next({ context, vars })`, and pass `final: true` to end the turn.".to_string(),
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
            "The step never called `control.next`, so nothing from it was kept. End every step with `await control.next({ context, vars })`.".to_string(),
        ));
    };
    if nexts > 1 || position + 1 != records.len() {
        return Err(Some(
            "`control.next` must be the step's last call, and only call. Calls after it ran, but the step committed nothing.".to_string(),
        ));
    }
    let next = RelayNext::from_args(&records[position].args, settings.context_budget_chars)
        .map_err(|error| Some(format!("next refused: {error}")))?;
    let outputs = records[..position]
        .iter()
        .filter(|record| record.tool == SEND_USER_OUTPUT_TOOL && record.output.is_success())
        .filter_map(|record| record.args.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if next.final_turn && outputs.is_empty() {
        return Err(Some(
            "`final: true` needs the same step to send the user its answer with `control.send_user_output`, so nothing from it was kept.".to_string(),
        ));
    }
    Ok(RelayCommit { next, outputs })
}

/// A relay harness note: protocol feedback the next harness message shows.
fn relay_note(id: String, text: String) -> Message {
    Message {
        id: id.clone(),
        role: lash_core::session_model::MessageRole::System,
        parts: lash_core::session_model::shared_parts(vec![lash_core::session_model::Part::text(
            format!("{id}.p0"),
            text,
            None,
        )]),
        origin: Some(lash_core::MessageOrigin::Plugin {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_string(),
            transient: false,
        }),
        reply_marker: None,
    }
}
