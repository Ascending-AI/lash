//! A response's tool round (DESIGN §3, §4): which calls run, which are
//! refused before dispatch, and where the step's one control call goes.
//!
//! The round is planned by walking the response's calls in order, each
//! `batch` call's members in member order at the wrapper's place. The first
//! call naming a control-declaring tool reserves the step's one control
//! attempt before its arguments are parsed, so a malformed or unlisted first
//! control call still spends it; every later control call is refused on its
//! own (`ControlAttemptSpent`), which blocks nothing else. Every other call
//! refused before dispatch (malformed arguments, an unlisted tool, an
//! invalid `batch`, a nested `batch`) is a failed sibling: the control call
//! is then refused before its body runs (`ControlSiblingFailed`).
//!
//! A control call keeps its own identity and slot, a `batch` member's too:
//! it is held behind the round's other slots and answers at its own slot,
//! where its wrapper folds it like any other member.

use std::num::NonZeroUsize;

use lash_core::sansio::{ExpandedRow, ExpandedWrapper, PendingToolCall, ToolExpansionPlan};
use lash_core::{ToolCallOutput, ToolFailure, ToolFailureCause, ToolFailureClass};
use serde_json::Value;

use crate::BatchSugar;
use crate::batch::{BATCH_TOOL_NAME, parse_members};

/// A tool call of the response, with the parse verdict on its raw argument
/// text.
pub(crate) struct ResponseCall {
    pub(crate) call: PendingToolCall,
    /// The raw argument text, kept as the arguments of a call refused for
    /// it.
    pub(crate) input_json: String,
    pub(crate) parse_error: Option<String>,
}

/// What a round is planned against.
pub(crate) struct RoundRules<'a> {
    pub(crate) batch: BatchSugar,
    /// The tools the request listed, when the model discovers the others:
    /// a call naming an unlisted one is refused.
    pub(crate) listed: Option<&'a dyn Fn(&str) -> bool>,
    /// Whether a call to the named tool ends the turn.
    pub(crate) ends_the_turn: &'a dyn Fn(&str) -> bool,
}

/// A planned round.
#[derive(Default)]
pub(crate) struct RoundPlan {
    /// The ordinary slots, in slot order: every one when the step makes no
    /// control call, else the ones before it.
    pub(crate) calls: Vec<PendingToolCall>,
    /// How the slots, the control call's among them, fold back into the
    /// response's calls.
    pub(crate) plan: ToolExpansionPlan,
    /// The control call and the ordinary slots after it, in slot order.
    pub(crate) control: Option<(PendingToolCall, Vec<PendingToolCall>)>,
    /// Calls refused before dispatch, in response order.
    pub(crate) refused: Vec<(PendingToolCall, ToolCallOutput)>,
}

impl RoundPlan {
    /// Dispatch `call` at the next slot: the step's control call when
    /// `control`, an ordinary slot before or after it otherwise.
    fn dispatch(&mut self, call: PendingToolCall, control: bool) {
        match &mut self.control {
            Some((_, after)) => after.push(call),
            None if control => self.control = Some((call, Vec::new())),
            None => self.calls.push(call),
        }
    }
}

/// A dispatched call of the response, before slots are numbered.
enum Entry {
    Call {
        call: PendingToolCall,
        control: bool,
    },
    Wrapper {
        call: PendingToolCall,
        members: Vec<Member>,
    },
}

enum Member {
    Run {
        index: u32,
        tool: String,
        parameters: Value,
        control: bool,
    },
    Refused {
        index: u32,
        tool: String,
        error: Value,
    },
}

pub(crate) fn plan_round(calls: Vec<ResponseCall>, rules: &RoundRules<'_>) -> RoundPlan {
    let mut round = RoundPlan::default();
    let mut entries = Vec::new();
    // Whether the step's one control attempt is spent, and whether a
    // sibling of the control call was refused before dispatch.
    let mut spent = false;
    let mut blocked = false;
    for ResponseCall {
        mut call,
        input_json,
        parse_error,
    } in calls
    {
        let control = (rules.ends_the_turn)(&call.tool_name);
        if control && spent {
            if parse_error.is_some() {
                call.args = Value::String(input_json);
            }
            let output = attempt_spent(&call.tool_name);
            round.refused.push((call, output));
            continue;
        }
        spent |= control;
        if let Some(parse_error) = parse_error {
            let output = ToolCallOutput::failure(ToolFailure::runtime(
                ToolFailureClass::InvalidRequest,
                "invalid_tool_call_json",
                format!(
                    "Tool `{}` was not executed: its arguments were not valid JSON ({parse_error}).",
                    call.tool_name
                ),
            ));
            call.args = Value::String(input_json);
            round.refused.push((call, output));
            blocked |= !control;
            continue;
        }
        // A `batch` wrapper is listed whenever the sugar is on, and its
        // members resolve against the session's callable catalog, not the
        // request's listed tools.
        if let Some(listed) = rules.listed
            && !listed(&call.tool_name)
        {
            let output = unlisted(&call.tool_name, rules.batch);
            round.refused.push((call, output));
            blocked |= !control;
            continue;
        }
        match rules.batch {
            BatchSugar::Enabled { max_members } if call.tool_name == BATCH_TOOL_NAME => {
                match members(&call.args, max_members, rules, &mut spent, &mut blocked) {
                    Ok(members) => entries.push(Entry::Wrapper { call, members }),
                    Err(message) => {
                        let output = ToolCallOutput::failure(ToolFailure::runtime(
                            ToolFailureClass::InvalidRequest,
                            "invalid_batch",
                            message,
                        ));
                        round.refused.push((call, output));
                        blocked = true;
                    }
                }
            }
            _ => entries.push(Entry::Call { call, control }),
        }
    }
    // Slots count dispatched calls and members; positions count the
    // response's dispatched calls, a refused control call not among them.
    let mut slot = 0_u32;
    let mut source_position = 0_u32;
    for entry in entries {
        match entry {
            Entry::Call { call, control } if control && blocked => {
                let output =
                    ToolCallOutput::failure(ToolFailure::control_sibling_failed(&call.tool_name));
                round.refused.push((call, output));
            }
            Entry::Call { call, control } => {
                round.dispatch(call, control);
                slot += 1;
                source_position += 1;
            }
            Entry::Wrapper { call, members } => {
                let mut rows = Vec::with_capacity(members.len());
                for member in members {
                    match member {
                        Member::Run {
                            index,
                            tool,
                            control,
                            ..
                        } if control && blocked => rows.push(ExpandedRow::Refused {
                            member_index: index,
                            error: failure_value(ToolFailure::control_sibling_failed(&tool)),
                            tool,
                        }),
                        Member::Run {
                            index,
                            tool,
                            parameters,
                            control,
                        } => {
                            rows.push(ExpandedRow::Slot {
                                member_index: index,
                                tool: tool.clone(),
                                slot,
                            });
                            // A member is named under its wrapper by its
                            // original member index, counted before
                            // refusals (ADR 0117 §2).
                            let member = PendingToolCall {
                                call_id: call.call_id.child(u64::from(index)),
                                provider_call_id: None,
                                tool_name: tool,
                                args: parameters,
                                replay: None,
                            };
                            round.dispatch(member, control);
                            slot += 1;
                        }
                        Member::Refused { index, tool, error } => {
                            rows.push(ExpandedRow::Refused {
                                member_index: index,
                                tool,
                                error,
                            });
                        }
                    }
                }
                round.plan.wrappers.push(ExpandedWrapper {
                    source_position,
                    call_id: call.call_id,
                    provider_call_id: call.provider_call_id,
                    tool_name: call.tool_name,
                    args: call.args,
                    replay: call.replay,
                    rows,
                });
                source_position += 1;
            }
        }
    }
    round
}

/// A `batch` call's members, in member order, with the step's control
/// attempt reserved by the first that names a control-declaring tool.
fn members(
    args: &Value,
    max_members: NonZeroUsize,
    rules: &RoundRules<'_>,
    spent: &mut bool,
    blocked: &mut bool,
) -> Result<Vec<Member>, String> {
    let members = parse_members(args, max_members)?;
    Ok(members
        .into_iter()
        .enumerate()
        .map(|(index, member)| {
            let index = index_u32(index);
            if member.tool == BATCH_TOOL_NAME {
                *blocked = true;
                return Member::Refused {
                    index,
                    tool: member.tool,
                    error: Value::String("`batch` cannot run inside `batch`".to_string()),
                };
            }
            let control = (rules.ends_the_turn)(&member.tool);
            if control && *spent {
                return Member::Refused {
                    index,
                    error: failure_value(attempt_spent_failure(&member.tool)),
                    tool: member.tool,
                };
            }
            *spent |= control;
            Member::Run {
                index,
                tool: member.tool,
                parameters: member.parameters,
                control,
            }
        })
        .collect())
}

fn attempt_spent_failure(tool_name: &str) -> ToolFailure {
    ToolFailure::invalid_request(
        "control_attempt_spent",
        format!("`{tool_name}` was not called: this step already makes its one turn-ending call"),
    )
    .with_cause(ToolFailureCause::ControlAttemptSpent)
}

fn attempt_spent(tool_name: &str) -> ToolCallOutput {
    ToolCallOutput::failure(attempt_spent_failure(tool_name))
}

fn unlisted(tool_name: &str, batch: BatchSugar) -> ToolCallOutput {
    ToolCallOutput::failure(ToolFailure::runtime(
        ToolFailureClass::Unavailable,
        "unknown_tool",
        match batch {
            BatchSugar::Enabled { .. } => format!(
                "Tool `{tool_name}` was not listed in this request; use a listed discovery operation or batch."
            ),
            BatchSugar::Disabled => format!(
                "Tool `{tool_name}` was not listed in this request; use a listed discovery operation."
            ),
        },
    ))
}

/// A refused member's row error: the typed failure, as the model reads a
/// refused call's.
#[expect(
    clippy::expect_used,
    reason = "a ToolFailure is a crate-owned tree whose serde_json encoding cannot fail"
)]
fn failure_value(failure: ToolFailure) -> Value {
    serde_json::to_value(failure).expect("a tool failure encodes")
}

/// Slot and member counts are bounded by admission, far below `u32::MAX`.
fn index_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}
