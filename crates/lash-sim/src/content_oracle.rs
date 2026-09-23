//! Content-level durable-state oracles.
//!
//! The independent checkpoint checker (`state_checker`) and the usage
//! conservation law (`usage_oracle`) compare lash against itself: counts,
//! totals, and lash's own submitted usage rows. A fact lash never records is
//! invisible to both. These oracles compare three independently obtained
//! views instead:
//!
//! * **emitted** — what the scripted provider put on the wire and what the
//!   scripted tools returned, decoded by this module from the wire script
//!   itself, never through a lash provider adapter;
//! * **committed** — the per-delta usage rows lash submitted at the commit seam;
//! * **reopened** — the session graph and token ledger read back through a
//!   fresh store handle after the run (a fresh SQLite factory on the SQLite
//!   lane, so that read is genuinely cold).
//!
//! # Documented projections
//!
//! The comparison is byte-for-byte after these projections and no others:
//!
//! * An assistant message is the concatenation of its `Text`/`Prose` parts plus
//!   the `(tool_call_id, tool_name)` of its `ToolCall` parts, in order. An
//!   emitted attempt's text is the concatenation of its streamed text deltas
//!   (OpenAI chat `choices[0].delta.content`, OpenAI Responses
//!   `response.output_text.delta`, Anthropic `text_delta`, Google candidate
//!   part `text`), and its tool calls are the streamed native calls.
//! * A tool result is the committed `ToolResult` part's `content`: the tool's
//!   JSON value in `serde_json` compact form while it fits the tool-output
//!   budget (bytes and lines). Past the budget the standard stack's
//!   tool-output-budget step keeps a head window: the committed content is
//!   a preview of at most the byte and line budget that the emitted text (its
//!   lines rejoined with `\n`) starts with, followed by a
//!   `\n\n...N <unit> truncated...\n\n` marker and a hint. The marker's
//!   count is not part of the projection.
//! * Usage is decoded per provider convention into the ledger buckets:
//!   OpenAI reports prompt tokens inclusive of cached ones (input = prompt −
//!   cached) and reasoning inside the completion count; Anthropic reports input
//!   and cache buckets on `message_start` and output on `message_delta` (later
//!   non-zero fields overlay earlier ones); Google reports candidates and
//!   thoughts separately (output = candidates + thoughts). The last usage the
//!   wire reports wins, as it does for the providers.
//!
//! Only provider-*reported* usage is in scope. An attempt whose wire carries no
//! usage is an unreported hole (FIG-2765), classified elsewhere; its ledger row
//! carries a non-reported disposition and is excluded from both sides.
//!
//! # The two laws
//!
//! Both laws are registered as run-only oracles of the generated lane.
//!
//! [`durable_content`] checks that every committed assistant message and tool
//! result equals what was emitted, and that the reported usage of every
//! *completed* attempt reaches the ledger as its own delta. On sessions where no attempt failed after reporting usage, the
//! committed deltas and the reopened ledger equal the emitted usage exactly.
//!
//! [`failed_attempt_usage_ledgered`] is the same usage law extended to failed
//! attempts: every attempt that reported usage, including one that then
//! failed, reaches the ledger as its own delta. FIG-3514's fix made it hold.

use std::collections::BTreeMap;
use std::fmt;

use lash_core::SessionStoreFactory;
use lash_sansio::SessionId;
use serde::Serialize;
use serde_json::Value;

use crate::provider::ProviderWireScript;
use crate::store::CheckpointWriteEvent;
use crate::trace::OracleVerdict;

pub const DURABLE_CONTENT_ORACLE: &str = "sim.oracle.durable-content.v1";
pub const FAILED_ATTEMPT_USAGE_ORACLE: &str = "sim.oracle.failed-attempt-usage-ledgered.v1";

/// The ledger's usage buckets, in this module's own representation so the
/// oracle does not borrow lash's usage type.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct UsageBuckets {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl UsageBuckets {
    const FIELDS: [&'static str; 5] = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "reasoning_output_tokens",
    ];

    fn from_ledger_usage(usage: &Value) -> Result<Self, String> {
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("ledger usage has no integer `{name}`: {usage}"))
        };
        Ok(Self {
            input_tokens: field(Self::FIELDS[0])?,
            output_tokens: field(Self::FIELDS[1])?,
            cache_read_input_tokens: field(Self::FIELDS[2])?,
            cache_write_input_tokens: field(Self::FIELDS[3])?,
            reasoning_output_tokens: field(Self::FIELDS[4])?,
        })
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_add(other.cache_write_input_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_add(other.reasoning_output_tokens),
        }
    }
}

/// A tool call as it was streamed or committed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolCallIdentity {
    pub call_id: String,
    pub tool_name: String,
}

/// What one provider attempt put on the wire.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EmittedAttempt {
    pub script_name: String,
    pub text: String,
    pub tool_calls: Vec<ToolCallIdentity>,
    /// `None` when the wire reported no usage at all.
    pub usage: Option<UsageBuckets>,
    /// `false` when the attempt ended in a transport fault or an unparseable
    /// event instead of a clean end of stream.
    pub completed: bool,
}

/// A tool result as the tool returned it, projected to the committed form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolResultContent {
    pub call_id: String,
    pub tool_name: String,
    pub content: String,
}

impl ToolResultContent {
    /// The documented projection of a JSON tool value into its committed form.
    pub fn from_tool_value(call_id: &str, tool_name: &str, value: &Value) -> Self {
        Self {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            content: value.to_string(),
        }
    }
}

/// An assistant message read back from the reopened graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommittedMessage {
    pub text: String,
    pub tool_calls: Vec<ToolCallIdentity>,
}

/// A session read back through a fresh store handle.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ReopenedSession {
    pub assistant_messages: Vec<CommittedMessage>,
    pub tool_results: Vec<ToolResultContent>,
    /// Summed usage of the provider-reported ledger rows.
    pub reported_ledger_total: UsageBuckets,
}

/// Everything the content oracles need about one session.
#[derive(Clone, Debug, Serialize)]
pub struct SessionContent {
    pub session: String,
    pub emitted_attempts: Vec<EmittedAttempt>,
    pub emitted_tool_results: Vec<ToolResultContent>,
    /// Provider-reported usage deltas submitted at the commit seam, one per
    /// delta, across every commit of the session.
    pub committed_usage: Vec<UsageBuckets>,
    /// `None` when the store holds no session under this id.
    pub reopened: Option<ReopenedSession>,
}

impl SessionContent {
    fn has_failed_reported_attempt(&self) -> bool {
        self.emitted_attempts
            .iter()
            .any(|attempt| !attempt.completed && attempt.usage.is_some())
    }
}

/// Decode what a scripted provider attempt emitted, from its wire timeline.
pub fn emitted_attempt(script: &ProviderWireScript) -> Result<EmittedAttempt, String> {
    let encoded = serde_json::to_value(script)
        .map_err(|err| format!("wire script `{}` does not encode: {err}", script.name))?;
    let timeline = encoded
        .get("timeline")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("wire script `{}` has no timeline", script.name))?;
    let mut decoder = WireDecoder::new(&script.provider_kind)?;
    let mut completed = true;
    for event in timeline {
        match event.get("event").and_then(Value::as_str) {
            Some("sse") => {
                let data = event.get("data").and_then(Value::as_str).unwrap_or("");
                if data == "[DONE]" {
                    continue;
                }
                match serde_json::from_str::<Value>(data) {
                    Ok(value) => decoder.observe(&value),
                    Err(_) => completed = false,
                }
            }
            Some("disconnect" | "timeout" | "http_error" | "transport_error") => completed = false,
            _ => {}
        }
    }
    Ok(decoder.finish(script.name.clone(), completed))
}

struct WireDecoder {
    kind: WireKind,
    text: String,
    tool_calls: BTreeMap<u64, ToolCallIdentity>,
    usage: Option<UsageBuckets>,
}

#[derive(Clone, Copy)]
enum WireKind {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Google,
}

impl WireDecoder {
    fn new(provider_kind: &str) -> Result<Self, String> {
        let kind = match provider_kind {
            "openai-compatible" => WireKind::OpenAiChat,
            "openai" => WireKind::OpenAiResponses,
            "anthropic" => WireKind::Anthropic,
            "google_oauth" => WireKind::Google,
            other => return Err(format!("no wire decoder for provider kind `{other}`")),
        };
        Ok(Self {
            kind,
            text: String::new(),
            tool_calls: BTreeMap::new(),
            usage: None,
        })
    }

    fn observe(&mut self, event: &Value) {
        match self.kind {
            WireKind::OpenAiChat => {
                let delta = event.pointer("/choices/0/delta");
                if let Some(text) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
                    self.text.push_str(text);
                }
                for call in delta
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.tool_calls.entry(index).or_insert(ToolCallIdentity {
                        call_id: String::new(),
                        tool_name: String::new(),
                    });
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        entry.call_id = id.to_string();
                    }
                    if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                        entry.tool_name = name.to_string();
                    }
                }
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
                    self.usage = Some(openai_usage(usage));
                }
            }
            WireKind::OpenAiResponses => match event.get("type").and_then(Value::as_str) {
                Some("response.output_text.delta") => {
                    if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                        self.text.push_str(delta);
                    }
                }
                Some("response.completed") => {
                    if let Some(usage) = event.pointer("/response/usage") {
                        self.usage = Some(openai_usage(usage));
                    }
                }
                _ => {}
            },
            WireKind::Anthropic => match event.get("type").and_then(Value::as_str) {
                Some("content_block_delta") => {
                    if let Some(text) = event.pointer("/delta/text").and_then(Value::as_str) {
                        self.text.push_str(text);
                    }
                }
                Some("message_start") => {
                    if let Some(usage) = event.pointer("/message/usage") {
                        self.usage = Some(anthropic_usage(usage, UsageBuckets::default()));
                    }
                }
                Some("message_delta") => {
                    if let Some(usage) = event.get("usage") {
                        self.usage = Some(anthropic_usage(usage, self.usage.unwrap_or_default()));
                    }
                }
                _ => {}
            },
            WireKind::Google => {
                for part in event
                    .pointer("/response/candidates/0/content/parts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        self.text.push_str(text);
                    }
                }
                if let Some(usage) = event.pointer("/response/usageMetadata") {
                    self.usage = Some(google_usage(usage));
                }
            }
        }
    }

    fn finish(self, script_name: String, completed: bool) -> EmittedAttempt {
        EmittedAttempt {
            script_name,
            text: self.text,
            tool_calls: self.tool_calls.into_values().collect(),
            usage: self.usage,
            completed,
        }
    }
}

fn count(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

fn openai_usage(usage: &Value) -> UsageBuckets {
    let prompt = count(usage, "/prompt_tokens").max(count(usage, "/input_tokens"));
    let cached = count(usage, "/prompt_tokens_details/cached_tokens")
        .max(count(usage, "/input_tokens_details/cached_tokens"));
    UsageBuckets {
        input_tokens: prompt.saturating_sub(cached).max(0),
        output_tokens: count(usage, "/completion_tokens").max(count(usage, "/output_tokens")),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: count(usage, "/completion_tokens_details/reasoning_tokens")
            .max(count(usage, "/output_tokens_details/reasoning_tokens")),
    }
}

fn anthropic_usage(usage: &Value, earlier: UsageBuckets) -> UsageBuckets {
    let overlay = |pointer: &str, earlier: i64| match count(usage, pointer) {
        0 => earlier,
        value => value,
    };
    UsageBuckets {
        input_tokens: overlay("/input_tokens", earlier.input_tokens),
        output_tokens: overlay("/output_tokens", earlier.output_tokens),
        cache_read_input_tokens: overlay(
            "/cache_read_input_tokens",
            earlier.cache_read_input_tokens,
        ),
        cache_write_input_tokens: overlay(
            "/cache_creation_input_tokens",
            earlier.cache_write_input_tokens,
        ),
        reasoning_output_tokens: overlay(
            "/output_tokens_details/thinking_tokens",
            earlier.reasoning_output_tokens,
        ),
    }
}

fn google_usage(usage: &Value) -> UsageBuckets {
    let cached = count(usage, "/cachedContentTokenCount");
    let thoughts = count(usage, "/thoughtsTokenCount");
    UsageBuckets {
        input_tokens: count(usage, "/promptTokenCount")
            .saturating_sub(cached)
            .max(0),
        output_tokens: count(usage, "/candidatesTokenCount").saturating_add(thoughts),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: thoughts,
    }
}

/// The provider-reported usage deltas `session` submitted across its commits.
pub fn committed_usage(
    writes: &[CheckpointWriteEvent],
    session: &str,
) -> Result<Vec<UsageBuckets>, String> {
    let mut deltas = Vec::new();
    for write in writes
        .iter()
        .filter(|write| write.attribution.is_none() && write.attributed_session() == session)
    {
        let Some(state) = &write.state else {
            continue;
        };
        for row in state
            .submitted_usage_rows
            .as_array()
            .ok_or_else(|| format!("`{session}` submitted usage rows are not an array"))?
        {
            if is_reported(row) {
                deltas.push(UsageBuckets::from_ledger_usage(
                    row.get("usage").unwrap_or(&Value::Null),
                )?);
            }
        }
    }
    Ok(deltas)
}

/// A ledger row is provider-reported unless it names another disposition.
fn is_reported(row: &Value) -> bool {
    row.get("usage_disposition")
        .is_none_or(|disposition| disposition.as_str() == Some("reported"))
}

/// Read `session_id` back through a fresh handle from `factory`.
pub async fn reopen_session(
    factory: &dyn SessionStoreFactory,
    session_id: &str,
) -> Result<Option<ReopenedSession>, String> {
    let request = lash_core::SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: SessionId::from(session_id.to_string()),
        relation: lash_core::SessionRelation::Root,
        policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    };
    let Some(store) = factory
        .open_existing_store(&request)
        .await
        .map_err(|err| format!("reopen `{session_id}`: {err}"))?
    else {
        return Ok(None);
    };
    let Some(read) = store
        .load_session()
        .await
        .map_err(|err| format!("load reopened `{session_id}`: {err}"))?
    else {
        return Ok(None);
    };
    let graph = serde_json::to_value(&read.graph)
        .map_err(|err| format!("reopened `{session_id}` graph does not encode: {err}"))?;
    let ledger = serde_json::to_value(&read.token_ledger)
        .map_err(|err| format!("reopened `{session_id}` ledger does not encode: {err}"))?;
    let mut reopened = ReopenedSession::default();
    for message in active_path_messages(&graph, session_id)? {
        let parts = message
            .get("parts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let part_str = |part: &Value, key: &str| {
            part.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        for part in parts {
            if part.get("kind").and_then(Value::as_str) == Some("ToolResult") {
                reopened.tool_results.push(ToolResultContent {
                    call_id: part_str(part, "tool_call_id"),
                    tool_name: part_str(part, "tool_name"),
                    content: part_str(part, "content"),
                });
            }
        }
        if message.get("role").and_then(Value::as_str) != Some("Assistant") {
            continue;
        }
        let mut committed = CommittedMessage {
            text: String::new(),
            tool_calls: Vec::new(),
        };
        for part in parts {
            match part.get("kind").and_then(Value::as_str) {
                Some("Text" | "Prose") => committed.text.push_str(&part_str(part, "content")),
                Some("ToolCall") => committed.tool_calls.push(ToolCallIdentity {
                    call_id: part_str(part, "tool_call_id"),
                    tool_name: part_str(part, "tool_name"),
                }),
                _ => {}
            }
        }
        reopened.assistant_messages.push(committed);
    }
    for row in ledger.as_array().map(Vec::as_slice).unwrap_or_default() {
        if is_reported(row) {
            reopened.reported_ledger_total =
                reopened
                    .reported_ledger_total
                    .saturating_add(UsageBuckets::from_ledger_usage(
                        row.get("usage").unwrap_or(&Value::Null),
                    )?);
        }
    }
    Ok(Some(reopened))
}

/// Conversation records on the active path, root first.
fn active_path_messages(graph: &Value, session_id: &str) -> Result<Vec<Value>, String> {
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("reopened `{session_id}` graph has no nodes"))?;
    let by_id = nodes
        .iter()
        .filter_map(|node| Some((node.get("node_id")?.as_str()?, node)))
        .collect::<BTreeMap<_, _>>();
    let mut path = Vec::new();
    let mut cursor = graph.get("leaf_node_id").and_then(Value::as_str);
    while let Some(node_id) = cursor {
        let node = by_id
            .get(node_id)
            .ok_or_else(|| format!("reopened `{session_id}` leaf path misses node `{node_id}`"))?;
        path.push(*node);
        if path.len() > nodes.len() {
            return Err(format!("reopened `{session_id}` graph has a cycle"));
        }
        cursor = node.get("parent_node_id").and_then(Value::as_str);
    }
    path.reverse();
    Ok(path
        .into_iter()
        .filter_map(|node| node.pointer("/event/Conversation").cloned())
        .collect())
}

/// Registered law: committed content equals emitted content after a reopen.
pub fn durable_content(sessions: &[SessionContent]) -> OracleVerdict {
    match check_durable_content(sessions) {
        Ok(counts) => OracleVerdict::passed(DURABLE_CONTENT_ORACLE, counts.to_string()),
        Err(message) => OracleVerdict::failed(DURABLE_CONTENT_ORACLE, message),
    }
}

/// The per-attempt usage law extended to attempts that failed after reporting
/// usage. Not registered until FIG-3514 lands; see the module docs.
pub fn failed_attempt_usage_ledgered(sessions: &[SessionContent]) -> OracleVerdict {
    let mut checked = 0usize;
    for session in sessions.iter().filter(|s| s.has_failed_reported_attempt()) {
        let reported = session
            .emitted_attempts
            .iter()
            .filter_map(|attempt| attempt.usage)
            .collect::<Vec<_>>();
        if let Err(message) = require_same_multiset(
            &reported,
            &session.committed_usage,
            &format!(
                "`{}` every reported attempt's usage, failed attempts included, vs committed deltas",
                session.session
            ),
        )
        .and_then(|()| {
            require_ledger_total(session, &reported, "every reported attempt, failed included")
        }) {
            return OracleVerdict::failed(FAILED_ATTEMPT_USAGE_ORACLE, message);
        }
        checked += 1;
    }
    if checked == 0 {
        return OracleVerdict::failed(
            FAILED_ATTEMPT_USAGE_ORACLE,
            "no session had an attempt that failed after reporting usage; the law is vacuous",
        );
    }
    OracleVerdict::passed(
        FAILED_ATTEMPT_USAGE_ORACLE,
        format!("{checked} session(s) ledgered every failed attempt's reported usage"),
    )
}

#[derive(Default)]
struct ContentCounts {
    sessions: usize,
    messages: usize,
    tool_results: usize,
    usage_attempts: usize,
    exact_usage_sessions: usize,
}

impl fmt::Display for ContentCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} assistant messages, {} tool results and {} reported attempt usages matched byte for byte after reopen across {} sessions ({} with exact per-attempt ledgers)",
            self.messages,
            self.tool_results,
            self.usage_attempts,
            self.sessions,
            self.exact_usage_sessions
        )
    }
}

fn check_durable_content(sessions: &[SessionContent]) -> Result<ContentCounts, String> {
    let mut counts = ContentCounts::default();
    for session in sessions {
        let reopened = session.reopened.as_ref().ok_or_else(|| {
            format!(
                "`{}` emitted {} attempt(s) but a reopened store holds no session",
                session.session,
                session.emitted_attempts.len()
            )
        })?;
        let completed = session
            .emitted_attempts
            .iter()
            .filter(|attempt| attempt.completed)
            .collect::<Vec<_>>();

        let emitted_messages = completed
            .iter()
            .map(|attempt| CommittedMessage {
                text: attempt.text.clone(),
                tool_calls: attempt.tool_calls.clone(),
            })
            .collect::<Vec<_>>();
        if reopened.assistant_messages.len() != emitted_messages.len() {
            return Err(format!(
                "`{}` committed {} assistant message(s) after reopen but its provider completed {} attempt(s)",
                session.session,
                reopened.assistant_messages.len(),
                emitted_messages.len()
            ));
        }
        for (index, (committed, emitted)) in reopened
            .assistant_messages
            .iter()
            .zip(&emitted_messages)
            .enumerate()
        {
            if committed != emitted {
                return Err(format!(
                    "`{}` assistant message {index} diverged after reopen: {}",
                    session.session,
                    describe_divergence(&emitted.text, &committed.text).unwrap_or_else(|| format!(
                        "tool calls emitted={:?} committed={:?}",
                        emitted.tool_calls, committed.tool_calls
                    ))
                ));
            }
        }

        if let Err(detail) =
            tool_results_match(&session.emitted_tool_results, &reopened.tool_results)
        {
            return Err(format!(
                "`{}` tool results diverged after reopen: {detail}",
                session.session
            ));
        }

        let completed_usage = completed
            .iter()
            .filter_map(|attempt| attempt.usage)
            .collect::<Vec<_>>();
        if session.has_failed_reported_attempt() {
            // A failed attempt's reported usage is FIG-3514's law; here every
            // completed attempt's usage must still be its own committed delta.
            require_subset(
                &completed_usage,
                &session.committed_usage,
                &format!(
                    "`{}` completed attempts' usage vs committed deltas",
                    session.session
                ),
            )?;
        } else {
            require_same_multiset(
                &completed_usage,
                &session.committed_usage,
                &format!(
                    "`{}` reported attempt usage vs committed deltas",
                    session.session
                ),
            )?;
            require_ledger_total(session, &completed_usage, "every reported attempt")?;
            counts.exact_usage_sessions += 1;
        }

        counts.sessions += 1;
        counts.messages += emitted_messages.len();
        counts.tool_results += session.emitted_tool_results.len();
        counts.usage_attempts += completed_usage.len();
    }
    if counts.messages == 0 || counts.tool_results == 0 || counts.usage_attempts == 0 {
        return Err(format!(
            "durable content law is vacuous: {counts}; every run must commit assistant messages, tool results and reported usage"
        ));
    }
    Ok(counts)
}

fn require_ledger_total(
    session: &SessionContent,
    attempts: &[UsageBuckets],
    what: &str,
) -> Result<(), String> {
    let expected = attempts
        .iter()
        .fold(UsageBuckets::default(), |total, usage| {
            total.saturating_add(*usage)
        });
    let reopened = session
        .reopened
        .as_ref()
        .map(|reopened| reopened.reported_ledger_total)
        .unwrap_or_default();
    if reopened != expected {
        return Err(format!(
            "`{}` reopened ledger total diverged from {what}: emitted={expected:?} ledger={reopened:?}",
            session.session
        ));
    }
    Ok(())
}

fn require_same_multiset(
    emitted: &[UsageBuckets],
    committed: &[UsageBuckets],
    what: &str,
) -> Result<(), String> {
    let mut emitted = emitted.to_vec();
    let mut committed = committed.to_vec();
    emitted.sort();
    committed.sort();
    if emitted != committed {
        return Err(format!(
            "{what} diverged: emitted={emitted:?} committed={committed:?}"
        ));
    }
    Ok(())
}

fn require_subset(
    emitted: &[UsageBuckets],
    committed: &[UsageBuckets],
    what: &str,
) -> Result<(), String> {
    let mut remaining = committed.to_vec();
    for usage in emitted {
        let Some(index) = remaining.iter().position(|candidate| candidate == usage) else {
            return Err(format!(
                "{what}: emitted {usage:?} has no committed delta among {committed:?}"
            ));
        };
        remaining.swap_remove(index);
    }
    Ok(())
}

fn tool_results_match(
    emitted: &[ToolResultContent],
    committed: &[ToolResultContent],
) -> Result<(), String> {
    let identities = |results: &[ToolResultContent]| {
        results
            .iter()
            .map(|result| (result.call_id.clone(), result.tool_name.clone()))
            .collect::<Vec<_>>()
    };
    if identities(emitted) != identities(committed) {
        return Err(format!(
            "emitted {} tool result(s) {:?}, committed {} {:?}",
            emitted.len(),
            call_ids(emitted),
            committed.len(),
            call_ids(committed)
        ));
    }
    let budget = lash::plugins::ToolOutputBudgetConfig::default();
    for (emitted, committed) in emitted.iter().zip(committed) {
        budget_projection_matches(
            &emitted.content,
            &committed.content,
            budget.limit,
            budget.max_lines,
        )
        .map_err(|detail| format!("`{}` {detail}", emitted.call_id))?;
    }
    Ok(())
}

/// The tool-output budget projection, as documented in the module docs.
fn budget_projection_matches(
    emitted: &str,
    committed: &str,
    max_bytes: usize,
    max_lines: usize,
) -> Result<(), String> {
    if emitted.len() <= max_bytes && emitted.lines().count() <= max_lines {
        return describe_divergence(emitted, committed).map_or(Ok(()), Err);
    }
    let preview_end = committed
        .match_indices("\n\n...")
        .map(|(index, _)| index)
        .find(|index| truncation_marker_at(&committed[index + 5..]))
        .ok_or_else(|| {
            format!(
                "emitted {} bytes over the {max_bytes}-byte/{max_lines}-line budget but the committed {} bytes carry no truncation marker",
                emitted.len(),
                committed.len()
            )
        })?;
    let preview = &committed[..preview_end];
    let rejoined = emitted.lines().collect::<Vec<_>>().join("\n");
    if preview.is_empty()
        || preview.len() > max_bytes
        || preview.lines().count() > max_lines
        || !rejoined.starts_with(preview)
    {
        return Err(format!(
            "truncated preview of {} bytes is not a head window within the budget: {}",
            preview.len(),
            describe_divergence(&rejoined[..rejoined.len().min(preview.len())], preview)
                .unwrap_or_else(|| "preview exceeds the budget".to_string())
        ));
    }
    Ok(())
}

/// `N <unit> truncated...\n\n` right after the marker's leading dots.
fn truncation_marker_at(rest: &str) -> bool {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && [
            " bytes truncated...\n\n",
            " lines truncated...\n\n",
            " tokens truncated...\n\n",
        ]
        .iter()
        .any(|unit| rest[digits..].starts_with(unit))
}

fn call_ids(results: &[ToolResultContent]) -> Vec<&str> {
    results
        .iter()
        .map(|result| result.call_id.as_str())
        .collect()
}

/// Where two texts first differ, escaped so control characters stay legible.
fn describe_divergence(emitted: &str, committed: &str) -> Option<String> {
    if emitted == committed {
        return None;
    }
    let at = emitted
        .bytes()
        .zip(committed.bytes())
        .position(|(left, right)| left != right)
        .unwrap_or_else(|| emitted.len().min(committed.len()));
    let window = |text: &str| {
        let start = floor_char_boundary(text, at.saturating_sub(16));
        let end = floor_char_boundary(text, at.saturating_add(32).min(text.len()));
        format!("{:?}", &text[start..end])
    };
    Some(format!(
        "first byte difference at {at} (emitted {} bytes, committed {} bytes): emitted {} vs committed {}",
        emitted.len(),
        committed.len(),
        window(emitted),
        window(committed)
    ))
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i64, output: i64) -> UsageBuckets {
        UsageBuckets {
            input_tokens: input,
            output_tokens: output,
            ..UsageBuckets::default()
        }
    }

    fn attempt(text: &str, usage: Option<UsageBuckets>, completed: bool) -> EmittedAttempt {
        EmittedAttempt {
            script_name: "script".to_string(),
            text: text.to_string(),
            tool_calls: Vec::new(),
            usage,
            completed,
        }
    }

    fn message(text: &str) -> CommittedMessage {
        CommittedMessage {
            text: text.to_string(),
            tool_calls: Vec::new(),
        }
    }

    fn tool_result(content: &str) -> ToolResultContent {
        ToolResultContent {
            call_id: "call-1".to_string(),
            tool_name: "tool".to_string(),
            content: content.to_string(),
        }
    }

    /// One healthy session whose provider completed two attempts and whose
    /// tool returned one result, all committed faithfully.
    fn healthy() -> SessionContent {
        SessionContent {
            session: "session-001".to_string(),
            emitted_attempts: vec![
                attempt("caf\u{e9} \u{0} \u{1f980}", Some(usage(1 << 40, 3)), true),
                attempt("e\u{301}", Some(usage(u32::MAX.into(), 0)), true),
            ],
            emitted_tool_results: vec![tool_result("{\"payload\":\"\\u0000\"}")],
            committed_usage: vec![usage(u32::MAX.into(), 0), usage(1 << 40, 3)],
            reopened: Some(ReopenedSession {
                assistant_messages: vec![message("caf\u{e9} \u{0} \u{1f980}"), message("e\u{301}")],
                tool_results: vec![tool_result("{\"payload\":\"\\u0000\"}")],
                reported_ledger_total: usage((1 << 40) + i64::from(u32::MAX), 3),
            }),
        }
    }

    /// A session whose first attempt reported usage and then failed, and whose
    /// retry completed. Today only the retry's usage reaches the ledger.
    fn retried_like_today() -> SessionContent {
        SessionContent {
            session: "probe".to_string(),
            emitted_attempts: vec![
                attempt("", Some(usage(7, 0)), false),
                attempt("done", Some(usage(9, 4)), true),
            ],
            emitted_tool_results: Vec::new(),
            committed_usage: vec![usage(9, 4)],
            reopened: Some(ReopenedSession {
                assistant_messages: vec![message("done")],
                tool_results: Vec::new(),
                reported_ledger_total: usage(9, 4),
            }),
        }
    }

    #[test]
    fn faithful_content_passes_and_every_corruption_is_named() {
        let passed = durable_content(&[healthy()]);
        assert!(passed.is_passed(), "{}", passed.message);

        let mut dropped_combining = healthy();
        dropped_combining
            .reopened
            .as_mut()
            .expect("reopened")
            .assistant_messages[1] = message("e");
        let verdict = durable_content(&[dropped_combining]);
        assert!(!verdict.is_passed());
        assert!(
            verdict.message.contains("assistant message 1 diverged"),
            "{}",
            verdict.message
        );

        let mut stripped_nul = healthy();
        stripped_nul
            .reopened
            .as_mut()
            .expect("reopened")
            .tool_results[0] = tool_result("{\"payload\":\"\"}");
        let verdict = durable_content(&[stripped_nul]);
        assert!(
            verdict.message.contains("tool results diverged"),
            "{}",
            verdict.message
        );

        let mut summed_delta = healthy();
        summed_delta.committed_usage = vec![usage((1 << 40) + i64::from(u32::MAX), 3)];
        let verdict = durable_content(&[summed_delta]);
        assert!(
            verdict.message.contains("vs committed deltas"),
            "{}",
            verdict.message
        );

        let mut ledger_drift = healthy();
        ledger_drift
            .reopened
            .as_mut()
            .expect("reopened")
            .reported_ledger_total = usage(1, 1);
        let verdict = durable_content(&[ledger_drift]);
        assert!(
            verdict.message.contains("reopened ledger total"),
            "{}",
            verdict.message
        );

        let mut lost = healthy();
        lost.reopened = None;
        let verdict = durable_content(&[lost]);
        assert!(
            verdict.message.contains("holds no session"),
            "{}",
            verdict.message
        );
    }

    #[test]
    fn content_law_is_not_vacuous() {
        let verdict = durable_content(&[]);
        assert!(!verdict.is_passed());
        assert!(verdict.message.contains("vacuous"), "{}", verdict.message);
    }

    #[test]
    fn registered_law_tolerates_today_failed_attempt_ledgering_but_not_a_lost_completion() {
        let today = durable_content(&[healthy(), retried_like_today()]);
        assert!(today.is_passed(), "{}", today.message);

        let mut lost_completion = retried_like_today();
        lost_completion.committed_usage.clear();
        let verdict = durable_content(&[healthy(), lost_completion]);
        assert!(
            verdict.message.contains("has no committed delta"),
            "{}",
            verdict.message
        );
    }

    #[test]
    fn failed_attempt_law_requires_every_reported_attempt_as_its_own_delta() {
        let today = failed_attempt_usage_ledgered(&[healthy(), retried_like_today()]);
        assert!(!today.is_passed(), "the FIG-3514 shape must be red");
        assert!(
            today.message.contains("failed attempts included"),
            "{}",
            today.message
        );

        let mut fixed = retried_like_today();
        fixed.committed_usage = vec![usage(7, 0), usage(9, 4)];
        fixed
            .reopened
            .as_mut()
            .expect("reopened")
            .reported_ledger_total = usage(16, 4);
        let verdict = failed_attempt_usage_ledgered(&[healthy(), fixed.clone()]);
        assert!(verdict.is_passed(), "{}", verdict.message);
        // The fixed shape still satisfies the registered law, so registering
        // the failed-attempt law does not have to touch it.
        assert!(durable_content(&[healthy(), fixed]).is_passed());

        let vacuous = failed_attempt_usage_ledgered(&[healthy()]);
        assert!(vacuous.message.contains("vacuous"), "{}", vacuous.message);
    }

    #[test]
    fn over_budget_tool_results_must_be_a_head_window_with_a_marker() {
        let emitted = format!("{}\u{1f980}tail", "a".repeat(14));
        // 22 emitted bytes over a 16-byte budget: the crab straddles byte 16.
        let committed = format!("{}\n\n...5 bytes truncated...\n\nhint", "a".repeat(14));
        assert_eq!(
            budget_projection_matches(&emitted, &committed, 16, 400),
            Ok(())
        );
        let wrong_head = format!("{}\n\n...5 bytes truncated...\n\nhint", "b".repeat(14));
        assert!(budget_projection_matches(&emitted, &wrong_head, 16, 400).is_err());
        assert!(budget_projection_matches(&emitted, &emitted, 16, 400).is_err());
        assert_eq!(budget_projection_matches("fits", "fits", 16, 400), Ok(()));
        assert!(budget_projection_matches("fits", "fit", 16, 400).is_err());
    }

    #[test]
    fn divergence_reports_escape_controls_and_stay_on_char_boundaries() {
        let detail = describe_divergence("ab\u{1f980}\u{0}x", "ab\u{1f980}x").expect("differs");
        assert!(detail.contains("first byte difference at 6"), "{detail}");
        assert!(detail.contains("\\0"), "{detail}");
    }
}
