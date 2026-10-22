//! The scripted provider and the deterministic tool fixtures a replay runs.
//!
//! The provider is stateless: it finds the turn a request answers from the
//! marker the runner puts at the start of every user input, and the step from
//! how many assistant messages follow that input. A resumed or replayed turn
//! asks the same step again and gets the same reply. A call past the turn's
//! script, a compaction summary included, is answered with plain text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::LlmOutputPart;
use lash_core::ToolCall;
use lash_core::ToolOutcome;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockIdentity,
};
use lash_protocol_types::sync::MutexExt as _;
use lash_protocol_types::{llm::types::StreamBlockEvent, llm::types::StreamBlockKind};

use lash_vocabulary::tool_contract::ExecutionPolicy;

use super::script::{Protocol, Reply, ScriptedTurn};

/// The text a call past its turn's script receives.
pub const FALLBACK_TEXT: &str = "Replay: the scripted turn is complete.";
/// The model key every replay session names.
pub const MODEL: &str = "lash-perf-replay-model";
const MARKER_OPEN: &str = "[replay ";

/// The marker that opens session `session`'s turn `ordinal`.
pub fn marker(session: usize, ordinal: usize) -> String {
    format!("{MARKER_OPEN}s{session} t{ordinal}]")
}

/// Every scripted turn of a run, by its marker, and the calls each received.
#[derive(Default)]
pub struct ScriptBook {
    protocol: Option<Protocol>,
    turns: HashMap<String, Vec<Reply>>,
    calls: Mutex<HashMap<String, TurnCalls>>,
}

/// The model calls one marked turn received.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnCalls {
    /// Calls answered from the turn's script.
    pub scripted: usize,
    /// Calls past the turn's script, answered with [`FALLBACK_TEXT`].
    pub past_script: usize,
    /// The tail of the last request past the script: what the model was
    /// shown, a failed cell's error included.
    pub last_past_script: Option<String>,
}

/// How much of an unscripted request a deviation report quotes.
const QUOTED_TAIL: usize = 2_048;

impl ScriptBook {
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol: Some(protocol),
            ..Self::default()
        }
    }

    /// Register session `session`'s `turn`, returning the user input that
    /// starts it.
    pub fn register(&mut self, session: usize, turn: &ScriptedTurn) -> String {
        let marker = marker(session, turn.ordinal);
        self.turns.insert(marker.clone(), turn.replies.clone());
        format!("{marker} {}", turn.user)
    }

    /// Take the calls session `session`'s turn `ordinal` received so far.
    pub fn take_calls(&self, session: usize, ordinal: usize) -> TurnCalls {
        self.calls
            .lock_recover()
            .remove(&marker(session, ordinal))
            .unwrap_or_default()
    }

    /// Count calls without consuming the per-turn deviation evidence.
    pub(crate) fn total_calls(&self) -> u64 {
        self.calls
            .lock_recover()
            .values()
            .map(|calls| (calls.scripted + calls.past_script) as u64)
            .sum()
    }

    fn respond(&self, request: &LlmRequest) -> LlmResponse {
        let Some((marker, at)) = latest_marker(request) else {
            return text(request, FALLBACK_TEXT);
        };
        let step = request.messages[at..]
            .iter()
            .filter(|message| message.role == LlmRole::Assistant)
            .count();
        let reply = self
            .turns
            .get(&marker)
            .and_then(|replies| replies.get(step));
        {
            let mut calls = self.calls.lock_recover();
            let calls = calls.entry(marker.clone()).or_default();
            if reply.is_some() {
                calls.scripted += 1;
            } else {
                calls.past_script += 1;
                let rendered = serde_json::to_string(&request.messages).unwrap_or_default();
                let start = rendered.len().saturating_sub(QUOTED_TAIL);
                let start = (start..rendered.len())
                    .find(|index| rendered.is_char_boundary(*index))
                    .unwrap_or(rendered.len());
                calls.last_past_script = Some(rendered[start..].to_owned());
            }
        }
        let Some(reply) = reply else {
            return text(request, FALLBACK_TEXT);
        };
        match reply {
            Reply::Text(body) => text(request, body),
            Reply::ToolCalls(calls) => response(
                calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| LlmOutputPart::ToolCall {
                        call_id: format!("{}-{step}-{index}", marker_slug(&marker)),
                        tool_name: call.tool.replace('.', "__"),
                        input_json: call.args.to_string(),
                        replay: None,
                    })
                    .collect(),
            ),
            Reply::Cell(super::script::Source(source)) => {
                let (open, close) = self
                    .protocol
                    .and_then(Protocol::fence)
                    .unwrap_or(("<typescript>", "</typescript>"));
                response(vec![LlmOutputPart::Text {
                    text: format!("{open}\n{source}\n{close}"),
                    response_meta: None,
                }])
            }
        }
    }
}

fn marker_slug(marker: &str) -> String {
    marker
        .trim_start_matches(MARKER_OPEN)
        .trim_end_matches(']')
        .replace(' ', "-")
}

/// The latest user message carrying a replay marker, and its index.
fn latest_marker(request: &LlmRequest) -> Option<(String, usize)> {
    request
        .messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == LlmRole::User)
        .find_map(|(index, message)| {
            let rendered = serde_json::to_value(message).ok()?;
            let mut texts = Vec::new();
            texts_in(&rendered, &mut texts);
            texts.iter().find_map(|text| {
                let start = text.find(MARKER_OPEN)?;
                let end = text[start..].find(']')? + start + 1;
                Some((text[start..end].to_owned(), index))
            })
        })
}

fn texts_in(value: &serde_json::Value, into: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match value {
                    serde_json::Value::String(text) if key == "text" => into.push(text.clone()),
                    value => texts_in(value, into),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| texts_in(item, into)),
        _ => {}
    }
}

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        ..LlmResponse::default()
    }
}

/// A text answer, streamed as one delta when the request streams.
fn text(request: &LlmRequest, body: &str) -> LlmResponse {
    if let Some(stream) = request.stream_events.as_ref() {
        stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
            kind: StreamBlockKind::AssistantText,
            block: StreamBlockIdentity::new("text:0", 0),
            text: body.to_owned(),
        }));
    }
    response(vec![LlmOutputPart::Text {
        text: body.to_owned(),
        response_meta: None,
    }])
}

/// The scripted provider over `book`.
pub fn model(book: Arc<ScriptBook>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("lash-perf-replay")
        .requires_streaming(true)
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..lash_core::provider::ProviderOptions::default()
        })
        .complete(move |request: LlmRequest| {
            let book = Arc::clone(&book);
            async move { Ok(book.respond(&request)) }
        })
        .build()
        .into_handle()
}

/// The model's metadata: a window no replay outgrows.
pub fn metadata() -> anyhow::Result<lash_core::LlmProfileMetadata> {
    lash_core::LlmProfileMetadata::builder(MODEL)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(1_000_000)
        .build()
        .map_err(|error| anyhow::anyhow!("the replay model's metadata: {error}"))
}

/// A small deterministic record keyed by its argument.
pub const LOOKUP: &str = "fixture_lookup";
/// A deterministic text of a requested size (a large value).
pub const LARGE: &str = "fixture_large";
/// A small answer after a scripted delay; optionally kills the code-mode
/// workers first, so the cell awaiting it resumes on a fresh worker.
pub const SLOW: &str = "fixture_slow";

type FixtureObserver = dyn Fn(&str, &serde_json::Value) + Send + Sync;
struct Fixtures(Option<Arc<FixtureObserver>>);

#[async_trait::async_trait]
impl StaticToolExecute for Fixtures {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let args = call.args;
        let name = call.name();
        let name = name.split_once('.').map_or(name.as_str(), |(_, tool)| tool);
        if let Some(observe) = &self.0 {
            observe(name, args);
        }
        let key = args
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let answer = match name {
            LOOKUP => serde_json::json!({
                "key": key,
                "title": format!("Record {key}"),
                "score": checksum(key.as_bytes()) % 1000,
                "tags": ["replay", "fixture"],
            }),
            LARGE => {
                let bytes = args
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1024)
                    .min(8 * 1024 * 1024);
                serde_json::json!({
                    "key": key,
                    "bytes": bytes,
                    "text": large_text(&key, usize::try_from(bytes).unwrap_or(0)),
                })
            }
            SLOW => {
                if args
                    .get("kill_workers")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    super::workers::kill_code_workers();
                }
                let ms = args
                    .get("ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(10)
                    .min(10_000);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                serde_json::json!({ "key": key, "waited_ms": ms })
            }
            other => serde_json::json!({ "unknown_fixture": other }),
        };
        ToolOutcome::ok(answer).into()
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(1_469_598_103_934_665_603_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
        })
}

/// `bytes` of printable text derived from `key`: the same key always yields
/// the same text, and it does not compress to a repeated literal.
pub fn large_text(key: &str, bytes: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 ";
    let mut state = checksum(key.as_bytes()) | 1;
    (0..bytes)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(ALPHABET[usize::try_from(state % ALPHABET.len() as u64).unwrap_or(0)])
        })
        .collect()
}

/// A fixture's definition. `open` fixtures take any further arguments: a
/// recorded cell's calls are pointed at one with their original arguments.
fn definition(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    open: bool,
) -> anyhow::Result<lash_vocabulary::tool_contract::ToolDefinition> {
    Ok(lash_vocabulary::tool_contract::ToolDefinition::raw(
        name,
        format!("tools.{name}"),
        description,
        serde_json::json!({
            "type": "object",
            "additionalProperties": open,
            "properties": properties,
            "required": if open { serde_json::json!([]) } else { serde_json::json!(["key"]) },
        }),
        serde_json::json!({ "type": "object" }),
    )
    .map_err(|error| anyhow::anyhow!("{name}'s schemas: {error}"))?
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once))
}

/// The fixture tools, bound as `tools.<name>` in code mode.
pub fn fixtures() -> anyhow::Result<Arc<dyn lash_core::ToolProvider>> {
    observed_fixtures(None)
}

/// The same fixtures with a benchmark-owned observation at body entry.
pub(crate) fn observed_fixtures(
    observer: Option<Arc<FixtureObserver>>,
) -> anyhow::Result<Arc<dyn lash_core::ToolProvider>> {
    let key = serde_json::json!({ "type": "string" });
    let number = serde_json::json!({ "type": "number" });
    Ok(Arc::new(StaticToolProvider::new(
        vec![
            definition(
                LOOKUP,
                "Looks up a small record by key.",
                serde_json::json!({ "key": key }),
                true,
            )?,
            definition(
                LARGE,
                "Returns a text of the requested size.",
                serde_json::json!({ "key": key, "bytes": number }),
                false,
            )?,
            definition(
                SLOW,
                "Answers after a delay.",
                serde_json::json!({
                    "key": key,
                    "ms": number,
                    "kill_workers": { "type": "boolean" },
                }),
                false,
            )?,
        ],
        Fixtures(observer),
    )))
}
