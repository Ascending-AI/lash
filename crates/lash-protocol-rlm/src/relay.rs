//! Relay (FIG-4441): an RLM execution policy in which each step hands the
//! next only the baton it passes to `control.next`.
//!
//! Relay runs on the native channel: its one provider tool is
//! `execute_code`, and a reply's shape says what it is.
//! - **A work step is an `execute_code` call** whose program ends with
//!   `await control.next({ context, vars })`. It commits when the program
//!   runs without error and its last host call is a successful `next`. The
//!   driver then appends the step's trajectory entry and one `RlmSeed` event
//!   carrying the call's `vars` with its `context` under the reserved global
//!   [`CONTEXT_VAR`]. There is no other record: the next step's prompt context
//!   and REPL globals are read from the last seed in the frame.
//! - **A reply with no tool call is the answer.** It is the turn's reply and
//!   ends the turn; the context stays as last committed. Text beside a tool
//!   call is never delivered.
//! - **Nothing else survives a step.** Every relay program starts on a fresh
//!   REPL rebuilt from the last seed's globals plus [`TRANSCRIPT_VAR`], so a
//!   step that throws, never calls `next` or is refused leaves no state
//!   behind. The attempt record (its trajectory entry, with the calls it made)
//!   still persists, and the next step message lists those calls as receipts.
//! - **Prompt.** `[system + execute_code]` + one user message holding the
//!   committed context under a constant header, one `[i]`-numbered block per
//!   entry + the step message ([`harness`]). Nothing is appended
//!   automatically: not the model's code, not tool output, not the user's
//!   message.

mod harness;
pub(crate) mod tools;

#[cfg(test)]
mod tests;

pub(crate) use harness::{LeftVariable, LeftVariables, RelayHarnessInput, build_relay_messages};
pub(crate) use tools::RelayControlToolsProvider;

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
/// The id suffix of the note that says why an executed step did not commit;
/// the next step message shows it as the last step's status.
pub(crate) const NOT_COMMITTED_NOTE: &str = "relay_not_committed";
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
    /// How much of a step's printed output the step message shows.
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
        let size = context_chars(&context);
        if size > budget_chars {
            return Err(format!(
                "context is {size} characters, over the {budget_chars}-character budget; drop or shorten entries and call next again"
            ));
        }
        Ok(Self { context, vars })
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

/// One committed user input or delivered reply.
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
    /// The baton committed before it, for the unchanged context prefix.
    pub(crate) previous: Option<RelayBaton>,
    /// The committed user inputs and delivered replies, in order.
    pub(crate) transcript: Vec<TranscriptEntry>,
    /// The current turn's input: the last contiguous run of user messages.
    pub(crate) turn_input: Vec<usize>,
    /// The reply that answered the turn before the current one, when the
    /// transcript holds one right before this turn's input.
    pub(crate) last_reply: Option<usize>,
    /// User-role messages a host injected into this turn's request (no
    /// origin): runtime notes, never the user's words.
    pub(crate) host_notes: Vec<String>,
    /// The current turn's last step, if it ran one.
    pub(crate) last_step: Option<RlmTrajectoryEntry>,
    /// Whether the last step committed.
    pub(crate) last_step_committed: bool,
    /// Why the last step did not commit, when a relay rule refused it (a
    /// step that threw says why in its own outcome).
    pub(crate) last_step_refusal: Option<String>,
    /// Steps of the current turn after its last commit: they ran, and
    /// committed nothing.
    pub(crate) uncommitted: Vec<RlmTrajectoryEntry>,
    /// Protocol feedback written after the current turn's last step: repair
    /// copy for a malformed call, an output-limit retry.
    pub(crate) feedback: Vec<String>,
}

impl RelayView {
    /// Read `projection` for the turn `turn_id`.
    ///
    /// The turn's input is the last run of user input messages (origin
    /// `TurnInput`). Only an earlier turn's records end a run: its steps and
    /// its reply. A user-role message with no origin is a host's note, and a
    /// user-role message another plugin wrote is not user input.
    pub(crate) fn read(
        projection: &ChronologicalProjection,
        turn_id: &str,
    ) -> Result<Self, lash_core::StoredDataCorruption> {
        let step_prefix = format!("lashlang_step_{turn_id}_");
        let feedback_prefix = format!("m_rlm_{turn_id}_");
        let mut view = Self::default();
        let mut input_open = false;
        for entry in projection.entries() {
            match &entry.payload {
                ChronologicalPayload::Message(message) => match (message.role, &message.origin) {
                    (
                        lash_core::MessageRole::User,
                        Some(lash_core::MessageOrigin::TurnInput { .. }),
                    ) => {
                        if !input_open {
                            view.turn_input.clear();
                            view.host_notes.clear();
                            view.last_reply = view
                                .transcript
                                .len()
                                .checked_sub(1)
                                .filter(|&index| view.transcript[index].role == "assistant");
                            input_open = true;
                        }
                        view.turn_input.push(view.transcript.len());
                        view.transcript.push(TranscriptEntry {
                            role: "user",
                            id: message.id.clone(),
                            text: message_text(&message.parts),
                        });
                    }
                    (lash_core::MessageRole::User, None) => {
                        view.host_notes.push(message_text(&message.parts));
                    }
                    (
                        lash_core::MessageRole::Assistant,
                        Some(lash_core::MessageOrigin::TurnOutput { .. }),
                    ) => {
                        input_open = false;
                        view.transcript.push(TranscriptEntry {
                            role: "assistant",
                            id: message.id.clone(),
                            text: message_text(&message.parts),
                        });
                    }
                    (lash_core::MessageRole::System, origin)
                        if crate::projection::is_rlm_protocol_output(origin.as_ref())
                            && message.id.starts_with(&feedback_prefix) =>
                    {
                        let text = message_text(&message.parts);
                        if message.id.ends_with(NOT_COMMITTED_NOTE) {
                            view.last_step_refusal = Some(text);
                        } else {
                            view.feedback.push(text);
                        }
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
                                view.last_step_refusal = None;
                                view.uncommitted.push(step.clone());
                                view.last_step = Some(step);
                            } else {
                                input_open = false;
                            }
                        }
                        Some(RlmProtocolEvent::RlmDiagnostic(diagnostic)) => {
                            // A malformed `execute_code` call's repair copy.
                            if let Some(text) =
                                crate::native::transport::repair_copy(diagnostic, turn_id)
                            {
                                view.feedback.push(text);
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

/// A value's kind and size, without its contents: `string, 12 chars`.
pub(crate) fn json_summary(value: &Value) -> String {
    let mut out = json_kind(value).to_string();
    match value {
        Value::String(text) => {
            let _ = write!(out, ", {} chars", text.chars().count());
        }
        Value::Array(items) => {
            let _ = write!(out, ", {} items", items.len());
        }
        Value::Object(fields) => {
            let _ = write!(out, ", {} keys", fields.len());
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    out
}

/// The execution prose a relay session's system prompt carries in place of
/// the chronological one: the two reply shapes, what survives a step, the
/// commit rule and how to keep context. It is authored for TypeScript on the
/// native channel, the only one relay runs on.
pub(crate) fn relay_execution_prose() -> String {
    format!(
        r#"You work in steps. Every response is one of two shapes:

- **Work:** call `{tool}` once, with a program that ends with `await control.next({{ context, vars }})`. Call tools inside it as `await module.operation({{ ... }})`, only those listed under **Tools**. Text you write beside the call is discarded.
- **Answer:** reply in plain text, with no tool call. That text is your answer to the user and ends the turn. Ask a question or say you are blocked the same way.

### What survives a step

Only what you pass to `control.next`:

- `context: string[]` is your whole working memory. The next request shows it right after this system prompt, one numbered entry per block. Your code, its output, tool results and the user's message are gone unless you write what matters into `context`.
- `vars` (default `{{}}`) is a record of plain values the next step finds as top-level variables. Every other variable is wiped. Functions cannot be carried: redefine helpers when you need them.
- Each program starts with `context` bound to your current context, so edit it with code: `[...context, "port is 8080 (config.toml)"]`. `transcript` holds the committed user messages and your answers (`{{ role, id, text }}`).

An answer changes neither: the next turn starts from the context your last step committed, and its first step shows your answer as `<last_reply>`.

### Commit

A step commits only when its program runs without error and its last call is a successful `control.next`. Otherwise nothing commits: no context, no vars. The tool calls it made did happen, though: the next step message lists them under `<receipts>`, so check their results instead of repeating them.

### The step message

The last message of every request: this turn's `<user_request>`, your last step (its code, printed output from `console.log(value)`, truncated, its calls, whether it committed), the size of your memory and the vars kept or dropped, and runtime notes.

### Keeping context

Put stable facts first and append by default (`[...context, note]`): an unchanged prefix stays in the provider cache. When much of it is stale, rewrite it in one go rather than a little every step, and tidy up when the step message says `cache: cold`. Record what you learned and what is left to do; anything you leave out is gone.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes and generators are not supported. A failed tool call throws an `Error` whose `cause` is `{{ code, details }}`.

### Example work step

```typescript
const total = 2 + 3;
await control.next({{ context: [...context, `2 + 3 = ${{total}}`] }});
```"#,
        tool = crate::native::NATIVE_EXECUTE_TOOL_NAME,
    )
}
