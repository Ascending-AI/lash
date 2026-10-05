//! Relay (FIG-4441): an RLM execution policy in which each step hands the
//! next only the baton it passes to `control.next`.
//!
//! Contract:
//! - **The baton is the committed call.** A step commits when its cell runs
//!   without error and its last host call is a successful `control.next`.
//!   The driver then appends the step's trajectory entry, one `RlmSeed` event
//!   carrying the call's `vars` with its `context` under the reserved global
//!   [`CONTEXT_VAR`], and the step's buffered `control.send_user_output`
//!   texts as assistant messages with ids `{turn}.{step}.{n}`. There is no
//!   other record: the next step's prompt context and REPL globals are read
//!   from the last seed in the frame.
//! - **Nothing else survives a step.** Every relay cell starts on a fresh REPL
//!   rebuilt from the last seed's globals plus [`TRANSCRIPT_VAR`], so a step
//!   that throws, never calls `next` or is refused leaves no state behind. The
//!   attempt record (its trajectory entry, with the calls it made) still
//!   persists, and the next harness message lists those calls as receipts.
//! - **Prompt.** `[system]` + one user message holding the committed context,
//!   one content block per entry, the last one a cache breakpoint + the
//!   harness message ([`harness`]). Nothing is appended automatically: not the
//!   model's code, not tool output, not the user's message.

mod harness;
pub(crate) mod tools;

#[cfg(test)]
mod tests;

pub(crate) use harness::{LeftVariable, LeftVariables, RelayHarnessInput, build_relay_messages};
pub(crate) use tools::{next_tool_definition, send_user_output_tool_definition};

use std::fmt::Write as _;

use lash_core::facade_support::{ChronologicalPayload, ChronologicalProjection};
use lash_rlm_types::{RlmProtocolEvent, RlmSeedPluginBody, RlmTrajectoryEntry};
use serde_json::Value;

use crate::projection::decode_rlm_protocol_event;

/// The global a relay step finds its committed context under, and the seed key
/// the context is carried in.
pub(crate) const CONTEXT_VAR: &str = "context";
/// The global a relay step finds the committed user-facing transcript under.
pub(crate) const TRANSCRIPT_VAR: &str = "transcript";
/// The `control.next` tool's manifest name.
pub(crate) const NEXT_TOOL: &str = "next";
/// The `control.send_user_output` tool's manifest name.
pub(crate) const SEND_USER_OUTPUT_TOOL: &str = "send_user_output";
/// Characters per token for the relay context budget, which is configured in
/// tokens.
const CHARS_PER_TOKEN: usize = 4;
/// The budget when the session records no soft context budget.
const DEFAULT_BUDGET_TOKENS: usize = 100_000;

/// A relay session's settings, read from its recorded behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelaySettings {
    /// The most characters a committed context may hold: the session's soft
    /// context budget (`continue_as_soft_warn_tokens`) at four characters per
    /// token.
    pub(crate) context_budget_chars: usize,
    /// How much of a step's printed output the harness message shows.
    pub(crate) max_output_chars: usize,
}

impl RelaySettings {
    /// The settings of a session under `config`, or `None` for a
    /// chronological one.
    pub(crate) fn of(config: &crate::RlmProtocolPluginConfig) -> Option<Self> {
        config.execution_policy.is_relay().then(|| Self {
            context_budget_chars: config
                .continue_as_soft_warn_tokens
                .unwrap_or(DEFAULT_BUDGET_TOKENS)
                .saturating_mul(CHARS_PER_TOKEN),
            max_output_chars: config.max_output_chars,
        })
    }
}

/// The validated arguments of one `control.next` call.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelayNext {
    pub(crate) context: Vec<String>,
    pub(crate) vars: serde_json::Map<String, Value>,
    pub(crate) final_turn: bool,
}

impl RelayNext {
    /// Read `next`'s arguments, refusing a shape the baton cannot carry or a
    /// context over `budget_chars`.
    pub(crate) fn from_args(args: &Value, budget_chars: usize) -> Result<Self, String> {
        let context = match args.get("context") {
            Some(Value::Array(entries)) => entries
                .iter()
                .enumerate()
                .map(|(index, entry)| match entry {
                    Value::String(text) => Ok(text.clone()),
                    other => Err(format!(
                        "context[{index}] must be a string, got {}",
                        json_kind(other)
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(other) => {
                return Err(format!(
                    "context must be an array of strings, got {}",
                    json_kind(other)
                ));
            }
            None => return Err("missing required parameter: context".to_string()),
        };
        let vars = match args.get("vars") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(value @ Value::Object(_)) => {
                // `next` materializes projected values: a carried var is a
                // copy (FIG-4441 known limit), so a projected entry lands as
                // a plain global.
                let seed = crate::projection::RlmSeed::from_seed_value(value)
                    .map_err(|error| format!("vars {error}"))?;
                let mut vars = seed.globals;
                for (name, entry) in seed.projected.entries {
                    let lash_rlm_types::RlmProjectedSeedEntry::Materialized(value) = entry;
                    vars.insert(name, value);
                }
                vars
            }
            Some(other) => {
                return Err(format!(
                    "vars must be a record of values, got {}",
                    json_kind(other)
                ));
            }
        };
        for reserved in [CONTEXT_VAR, TRANSCRIPT_VAR, "history"] {
            if vars.contains_key(reserved) {
                return Err(format!(
                    "vars cannot carry `{reserved}`: it is bound by the harness every step"
                ));
            }
        }
        let final_turn = match args.get("final") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(other) => {
                return Err(format!("final must be a boolean, got {}", json_kind(other)));
            }
        };
        let size = context_chars(&context);
        if size > budget_chars {
            return Err(format!(
                "context is {size} characters, over the {budget_chars}-character budget; drop or shorten entries and call next again"
            ));
        }
        Ok(Self {
            context,
            vars,
            final_turn,
        })
    }

    /// The seed event body this call commits: its vars, with the context under
    /// [`CONTEXT_VAR`].
    pub(crate) fn seed_body(&self) -> RlmSeedPluginBody {
        let mut globals = self.vars.clone();
        globals.insert(
            CONTEXT_VAR.to_string(),
            Value::Array(self.context.iter().cloned().map(Value::String).collect()),
        );
        RlmSeedPluginBody {
            globals,
            projected: Default::default(),
        }
    }
}

/// A committed baton, read back from its seed event.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RelayBaton {
    pub(crate) context: Vec<String>,
    pub(crate) vars: serde_json::Map<String, Value>,
}

impl RelayBaton {
    fn from_seed(seed: &RlmSeedPluginBody) -> Self {
        let mut vars = seed.globals.clone();
        let context = match vars.remove(CONTEXT_VAR) {
            Some(Value::Array(entries)) => entries
                .into_iter()
                .map(|entry| match entry {
                    Value::String(text) => text,
                    other => other.to_string(),
                })
                .collect(),
            _ => Vec::new(),
        };
        for (name, entry) in &seed.projected.entries {
            let lash_rlm_types::RlmProjectedSeedEntry::Materialized(value) = entry;
            vars.insert(name.clone(), value.clone());
        }
        Self { context, vars }
    }

    /// The globals a step starts with: the vars, the context and the
    /// transcript.
    pub(crate) fn step_globals(
        &self,
        transcript: &[TranscriptEntry],
    ) -> serde_json::Map<String, Value> {
        let mut globals = self.vars.clone();
        globals.insert(
            CONTEXT_VAR.to_string(),
            Value::Array(self.context.iter().cloned().map(Value::String).collect()),
        );
        globals.insert(
            TRANSCRIPT_VAR.to_string(),
            Value::Array(
                transcript
                    .iter()
                    .map(|entry| {
                        serde_json::json!({
                            "role": entry.role,
                            "id": entry.id,
                            "text": entry.text,
                        })
                    })
                    .collect(),
            ),
        );
        globals
    }
}

/// One committed user input or delivered output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptEntry {
    pub(crate) role: &'static str,
    pub(crate) id: String,
    pub(crate) text: String,
}

/// What relay reads from a session's chronological view.
#[derive(Clone, Debug, Default)]
pub(crate) struct RelayView {
    /// The last committed baton; `None` for a session that never committed.
    pub(crate) committed: Option<RelayBaton>,
    /// The baton committed before it, for the cache cost of the last edit.
    pub(crate) previous: Option<RelayBaton>,
    /// The committed user inputs and delivered outputs, in order.
    pub(crate) transcript: Vec<TranscriptEntry>,
    /// The current turn's input: the last contiguous run of user messages.
    pub(crate) turn_input: Vec<usize>,
    /// The current turn's last step, if it ran one.
    pub(crate) last_step: Option<RlmTrajectoryEntry>,
    /// Whether the last step committed.
    pub(crate) last_step_committed: bool,
    /// Steps of the current turn after its last commit: they ran, and
    /// committed nothing.
    pub(crate) uncommitted: Vec<RlmTrajectoryEntry>,
    /// Protocol feedback written after the current turn's last step.
    pub(crate) feedback: Vec<String>,
}

impl RelayView {
    /// Read `projection` for the turn `turn_id`.
    ///
    /// The turn's input is the last run of user input messages (origin
    /// `TurnInput`, or none for input a host executes in hand). Only the
    /// protocol records of an earlier turn end a run: this turn's own steps,
    /// seeds, outputs and notes interleave with its input in the view, and a
    /// user-role message another plugin wrote is not user input.
    pub(crate) fn read(
        projection: &ChronologicalProjection,
        turn_id: &str,
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        let step_prefix = format!("lashlang_step_{turn_id}_");
        let feedback_prefix = format!("m_rlm_{turn_id}_");
        let output_prefix = format!("{turn_id}.");
        let mut view = Self::default();
        let mut input_open = false;
        for entry in projection.entries() {
            match &entry.payload {
                ChronologicalPayload::Message(message) => match message.role {
                    lash_core::MessageRole::User if is_user_input(message.origin.as_ref()) => {
                        if !input_open {
                            view.turn_input.clear();
                            input_open = true;
                        }
                        view.turn_input.push(view.transcript.len());
                        view.transcript.push(TranscriptEntry {
                            role: "user",
                            id: message.id.clone(),
                            text: message_text(&message.parts),
                        });
                    }
                    lash_core::MessageRole::Assistant
                        if crate::projection::is_rlm_protocol_output(message.origin.as_ref()) =>
                    {
                        if turn_id.is_empty() || !message.id.starts_with(&output_prefix) {
                            input_open = false;
                        }
                        view.transcript.push(TranscriptEntry {
                            role: "assistant",
                            id: message.id.clone(),
                            text: message_text(&message.parts),
                        });
                    }
                    lash_core::MessageRole::System
                        if crate::projection::is_rlm_protocol_output(message.origin.as_ref())
                            && message.id.starts_with(&feedback_prefix) =>
                    {
                        view.feedback.push(message_text(&message.parts));
                    }
                    _ => {}
                },
                ChronologicalPayload::ProtocolEvent(event) => {
                    match decode_rlm_protocol_event(event)? {
                        Some(RlmProtocolEvent::RlmSeed(seed)) => {
                            view.previous = view.committed.take();
                            view.committed = Some(RelayBaton::from_seed(&seed));
                            view.uncommitted.clear();
                            if view.last_step.is_some() {
                                view.last_step_committed = true;
                            }
                        }
                        Some(RlmProtocolEvent::RlmTrajectoryEntry(step)) => {
                            if step.id.starts_with(&step_prefix) {
                                view.feedback.clear();
                                view.last_step_committed = false;
                                view.uncommitted.push(step.clone());
                                view.last_step = Some(step);
                            } else {
                                input_open = false;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // A committed step's own entry precedes its seed, so the seed cleared
        // it from `uncommitted` above; what is left never committed.
        Ok(view)
    }

    /// The globals a step of this view starts with.
    pub(crate) fn step_globals(&self) -> serde_json::Map<String, Value> {
        self.committed
            .clone()
            .unwrap_or_default()
            .step_globals(&self.transcript)
    }
}

/// Whether a user-role message is user input rather than another plugin's
/// message on the user channel.
fn is_user_input(origin: Option<&lash_core::MessageOrigin>) -> bool {
    matches!(
        origin,
        None | Some(lash_core::MessageOrigin::TurnInput { .. })
    )
}

fn message_text(parts: &[lash_core::Part]) -> String {
    parts
        .iter()
        .filter(|part| {
            matches!(
                part.kind(),
                lash_core::PartKind::Text | lash_core::PartKind::Prose
            )
        })
        .filter_map(|part| part.text_content())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The characters a context holds.
pub(crate) fn context_chars(context: &[String]) -> usize {
    context.iter().map(|entry| entry.chars().count()).sum()
}

/// A JSON value's kind, for refusals and the vars summary.
pub(crate) fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "record",
    }
}

/// A value's kind and size, without its contents: `string (12 chars)`.
pub(crate) fn json_summary(value: &Value) -> String {
    let mut out = json_kind(value).to_string();
    match value {
        Value::String(text) => {
            let _ = write!(out, " ({} chars)", text.chars().count());
        }
        Value::Array(items) => {
            let _ = write!(out, " ({} items)", items.len());
        }
        Value::Object(fields) => {
            let _ = write!(out, " ({} keys)", fields.len());
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    out
}

/// The execution prose a relay session's system prompt carries in place of
/// the chronological one: how a step runs, what survives it and how it ends.
/// It is authored for the TypeScript cell channel, the only one relay runs on.
pub(crate) fn relay_execution_prose(tags: crate::dialect::CellTags) -> String {
    let open = tags.open;
    let close = tags.close;
    format!(
        r#"You work in steps. Each response is one step: optional prose, then exactly one program between `{open}` and `{close}` on their own lines. Call tools as `await module.operation({{ ... }})`, only those listed under **Tools**. Prose outside the program is never shown to anyone.

### What survives a step

Nothing carries from one step to the next except what you pass to `control.next`:

- `await control.next({{ context, vars, final }})` ends the step and must be the last call in the {noun}.
- `context: string[]` becomes your entire working memory. The next request shows it right after this system prompt, one block per entry, exactly as you passed it. Your code, its output, tool results and the user's message are not kept unless you write what matters into `context`.
- `vars` (default `{{}}`) is a record of plain values that the next step finds as top-level variables. Every other variable is wiped. Functions cannot be carried: redefine helpers when you need them.
- `final: true` ends the turn once the step commits. Send the user the answer in that same step.
- Each step starts with `context` bound to your current context, so edit it with code: `await control.next({{ context: [...context, "port is 8080 (config.toml)"] }})`. `transcript` holds the committed user messages and outputs (`{{ role, id, text }}`), for when you need an old one.
- `await control.send_user_output({{ text }})` sends text to the user. It is delivered only if the step commits. A turn must send the user something before it ends. To ask the user a question or tell them you are blocked, send it and end the turn with `final: true`; their reply arrives as the next turn.

### Commit

A step commits only when its program runs without error and its last call is a successful `control.next`. A step that throws, never calls `next`, or passes a context over its budget commits nothing: no context, no vars, no output. The tool calls it made did happen, though: the harness message lists them, so check their results instead of repeating them.

### The harness message

The last message of every request is the harness message: the user's message for this turn, your last step's code, printed output (`console.log(value)`, truncated) and error, the effects of steps that did not commit, which vars your last commit kept and which variables it dropped, and the context's size against its budget.

### Keeping context

Put stable facts first and append by default (`[...context, note]`): an unchanged prefix stays in the provider cache. When much of it is stale, rewrite it in one go rather than a little every step. Tidy up when the harness says `cache: cold`. Record what you learned and what is left to do; anything you leave out is gone.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes and generators are not supported. A failed tool call throws an `Error` whose `cause` is `{{ code, details }}`.

### Example step

{open}
const total = 2 + 3;
await control.send_user_output({{ text: `2 + 3 = ${{total}}` }});
await control.next({{ context: [...context, `answered 2 + 3 = ${{total}}`], final: true }});
{close}"#,
        noun = "program",
    )
}
