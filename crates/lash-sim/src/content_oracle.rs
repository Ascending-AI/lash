//! Content-level durable-state oracles.
//!
//! The independent checkpoint checker (`state_checker`) compares lash
//! against itself: counts and totals. A fact lash never records is invisible
//! to it. These oracles compare three independently obtained views instead:
//!
//! * **emitted** — what the scripted provider put on the wire and what the
//!   scripted tools returned, decoded by this module from the wire script
//!   itself, never through a lash provider adapter;
//! * **delivered** — the per-attempt usage facts the engine delivered for the
//!   session's owner (ADR 0125), paged fact by fact;
//! * **reopened** — the session graph and the owner's aggregated usage read
//!   back through a fresh store handle after the run (a fresh SQLite factory
//!   on the SQLite lane, so that read is genuinely cold).
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
//! * A tool result is the committed `ToolResult` part's `content`: the value
//!   rendered with the standard renderer's default parameters. A cut includes
//!   a head and tail within the shared character and line limits, plus a
//!   notice naming the retained full output.
//! * Usage is decoded per provider convention into the accounting buckets:
//!   OpenAI reports prompt tokens inclusive of cached ones (input = prompt −
//!   cached) and reasoning inside the completion count; Anthropic reports input
//!   and cache buckets on `message_start` and output on `message_delta` (later
//!   non-zero fields overlay earlier ones); Google reports candidates and
//!   thoughts separately (output = candidates + thoughts). The last usage the
//!   wire reports wins, as it does for the providers.
//!
//! Only provider-*reported* usage is in scope. An attempt whose wire carries no
//! usage is an unreported attempt (FIG-2765), classified elsewhere; its fact
//! carries a non-reported disposition and is excluded from both sides.
//!
//! # The two laws
//!
//! Both laws are registered as run-only oracles of the generated lane.
//!
//! [`durable_content`] checks that every committed assistant message and tool
//! result equals what was emitted, and that the reported usage of every
//! *completed* attempt reaches the accounting as its own fact. On sessions
//! where no attempt failed after reporting usage, the delivered facts and the
//! reopened owner total equal the emitted usage exactly.
//!
//! [`failed_attempt_usage_ledgered`] is the same usage law extended to failed
//! attempts: every attempt that reported usage, including one that then
//! failed, reaches the accounting as its own fact. FIG-3514's fix made it
//! hold.

use std::collections::BTreeMap;
use std::fmt;

use lash_core::{DeploymentStore, UsageAccountingStore};
use lash_protocol_standard::{BuiltinToolOutputRenderer, ToolOutputRenderer, ToolRenderParams};
use lash_sansio::SessionId;
use serde::Serialize;
use serde_json::Value;

use crate::provider::ProviderWireScript;
use crate::trace::OracleVerdict;

pub const DURABLE_CONTENT_ORACLE: crate::trace::OracleId<'static> =
    crate::trace::OracleId::real("sim.oracle.durable-content.v1");
pub const FAILED_ATTEMPT_USAGE_ORACLE: crate::trace::OracleId<'static> =
    crate::trace::OracleId::real("sim.oracle.failed-attempt-usage-ledgered.v1");

/// The accounting's usage buckets, in this module's own representation so
/// the oracle does not borrow lash's usage type.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct UsageBuckets {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
}

impl UsageBuckets {
    fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    const FIELDS: [&'static str; 5] = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "reasoning_output_tokens",
    ];

    /// Decode the accounting's usage from its JSON rendering, so the oracle
    /// reads the counters by name rather than through lash's type.
    fn from_accounting_usage(usage: &lash_core::TokenUsage) -> Result<Self, String> {
        let usage = serde_json::to_value(usage)
            .map_err(|err| format!("accounting usage does not encode: {err}"))?;
        let field = |name: &str| {
            usage
                .get(name)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("accounting usage has no integer `{name}`: {usage}"))
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
            content: lash_core::facade_support::tool_result_text(
                &BuiltinToolOutputRenderer
                    .tool_output(
                        &lash_core::ToolCallOutput::success(value.clone()),
                        &lash_core::ToolId::new(tool_name),
                        &ToolRenderParams::default(),
                    )
                    .body,
            )
            .into_owned(),
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
    /// Each committed assistant message, its tool calls under the provider's
    /// correlation, as the wire carried them.
    pub assistant_messages: Vec<CommittedMessage>,
    /// Each committed tool result, answering its call's provider
    /// correlation: the result pairs with its call by `ToolCallId`, the way
    /// an adapter answers the provider.
    pub tool_results: Vec<ToolResultContent>,
    /// The `ToolCallId` lash named each committed call by, per assistant
    /// message, in the order of that message's `tool_calls`.
    pub call_ids: Vec<Vec<String>>,
    /// The `ToolCallId` each committed tool result answers, in the order of
    /// `tool_results`.
    pub result_call_ids: Vec<String>,
    /// The owner's aggregated usage over its provider-reported and reconciled
    /// facts, read through the accounting's totals rather than its fact pages.
    pub reported_usage_total: UsageBuckets,
}

/// Everything the content oracles need about one session.
#[derive(Clone, Debug, Serialize)]
pub struct SessionContent {
    pub session: String,
    pub emitted_attempts: Vec<EmittedAttempt>,
    pub emitted_tool_results: Vec<ToolResultContent>,
    /// The session owner's provider-reported usage facts, one per attempt,
    /// as the engine delivered them.
    pub delivered_usage: Vec<UsageBuckets>,
    /// `None` when the store holds no session under this id.
    pub reopened: Option<ReopenedSession>,
}

impl SessionContent {
    fn has_failed_reported_attempt(&self) -> bool {
        self.emitted_attempts.iter().any(|attempt| {
            !attempt.completed && attempt.usage.is_some_and(|usage| !usage.is_zero())
        })
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

/// Fresh handles on one engine's storage, for reading its sessions back.
#[derive(Clone)]
pub struct ReopenHandles {
    pub sessions: std::sync::Arc<dyn DeploymentStore>,
    pub usage: std::sync::Arc<dyn UsageAccountingStore>,
}

impl ReopenHandles {
    pub fn over(backend: &lash_core::Backend) -> Self {
        Self {
            sessions: backend.session_store_factory(),
            usage: backend.usage_accounting(),
        }
    }
}

/// How long a read waits for the engine to deliver a session's open runs.
const DELIVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until every usage run `session`'s owner admitted is resolved:
/// delivery is asynchronous to the turn (ADR 0125), so a read taken the
/// moment a turn ends can precede its last settlement.
async fn await_settled_usage(
    usage: &dyn UsageAccountingStore,
    session: &str,
) -> Result<lash_core::OwnerUsage, String> {
    let owner = lash_core::RuntimeOwner::Session(SessionId::from(session.to_string()));
    let deadline = std::time::Instant::now() + DELIVERY_WAIT;
    loop {
        let read = usage
            .load_owner_usage(&owner)
            .await
            .map_err(|err| format!("read `{session}` usage: {err}"))?;
        if read.completeness.is_settled() {
            return Ok(read);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "`{session}` usage did not settle within {DELIVERY_WAIT:?}: {:?}",
                read.completeness
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The provider-reported usage facts the engine delivered for `session`'s
/// owner, one per attempt. A zero report carries no charge and is outside
/// both sides of every comparison, as it is on the emitted side.
#[expect(
    clippy::expect_used,
    reason = "the fixed fact page limit is a nonzero constant"
)]
pub async fn delivered_usage(
    usage: &dyn UsageAccountingStore,
    session: &str,
) -> Result<Vec<UsageBuckets>, String> {
    await_settled_usage(usage, session).await?;
    let owner = lash_core::RuntimeOwner::Session(SessionId::from(session.to_string()));
    let mut facts = Vec::new();
    let mut after = None;
    loop {
        let page = usage
            .load_usage_fact_page(
                &owner,
                after.as_ref(),
                std::num::NonZeroU32::new(256).expect("nonzero page size"),
            )
            .await
            .map_err(|err| format!("page `{session}` usage facts: {err}"))?;
        for fact in page.facts {
            if fact.disposition() == lash_core::UsageReporting::Reported {
                let buckets = UsageBuckets::from_accounting_usage(&fact.usage())?;
                if !buckets.is_zero() {
                    facts.push(buckets);
                }
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(facts)
}

/// Read the committed history through explicit graph pages, and the owner's
/// settled usage through the accounting's totals.
#[expect(
    clippy::expect_used,
    reason = "the fixed history page limits are nonzero constants"
)]
pub async fn reopen_session(
    store: &dyn DeploymentStore,
    usage: &dyn UsageAccountingStore,
    session_id: &str,
) -> Result<Option<ReopenedSession>, String> {
    use lash_core::store::{HistoryAnchor, HistoryBudget};
    use std::num::{NonZeroU32, NonZeroU64};

    let session_id = SessionId::from(session_id.to_string());
    let Some(head) = store
        .load_session_head_meta(&session_id)
        .await
        .map_err(|err| format!("reopen `{session_id}`: {err}"))?
    else {
        return Ok(None);
    };
    let mut anchor = HistoryAnchor::Head;
    let mut nodes = Vec::new();
    loop {
        let page = store
            .load_ancestors(
                &session_id,
                anchor,
                HistoryBudget {
                    max_nodes: NonZeroU32::new(256).expect("nonzero page size"),
                    max_bytes: NonZeroU64::new(32 * 1024 * 1024).expect("nonzero byte limit"),
                },
            )
            .await
            .map_err(|err| format!("page reopened `{session_id}` graph: {err}"))?;
        nodes.extend(page.nodes.into_iter().map(|node| node.record));
        match page.next {
            Some(next) => anchor = HistoryAnchor::Cursor(next),
            None => break,
        }
    }
    nodes.reverse();
    let graph = lash_core::SessionGraph::from_nodes(nodes, head.leaf_node_id)
        .and_then(|graph| {
            serde_json::to_value(graph)
                .map_err(|err| lash_core::StoreError::Backend(err.to_string()))
        })
        .map_err(|err| format!("reopened `{session_id}` graph does not encode: {err}"))?;
    let mut reopened = ReopenedSession::default();
    let messages = active_path_messages(&graph, session_id.as_str())?;
    let provider_call_ids = messages
        .iter()
        .flat_map(|message| {
            message
                .get("parts")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
        })
        .filter_map(|part| {
            Some((
                part.get("call_id")?.as_str()?.to_string(),
                part.get("provider_call_id")?.as_str()?.to_string(),
            ))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    for message in messages {
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
                // A tool result's content is its ordered `blocks`; compare the
                // text the model reads, rendered the one way the runtime does.
                let blocks = serde_json::from_value::<Vec<lash_sansio::ModelToolReturnPart>>(
                    part.get("blocks").cloned().unwrap_or(Value::Null),
                )
                .map_err(|err| {
                    format!("reopened `{session_id}` tool result blocks do not decode: {err}")
                })?;
                reopened.result_call_ids.push(part_str(part, "call_id"));
                reopened.tool_results.push(ToolResultContent {
                    call_id: provider_call_ids
                        .get(&part_str(part, "call_id"))
                        .cloned()
                        .unwrap_or_default(),
                    tool_name: part_str(part, "tool_name"),
                    content: lash_sansio::tool_result_text(&blocks).into_owned(),
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
        let mut call_ids = Vec::new();
        for part in parts {
            match part.get("kind").and_then(Value::as_str) {
                Some("Text" | "Prose") => committed.text.push_str(&part_str(part, "content")),
                Some("ToolCall") => {
                    committed.tool_calls.push(ToolCallIdentity {
                        call_id: part_str(part, "provider_call_id"),
                        tool_name: part_str(part, "tool_name"),
                    });
                    call_ids.push(part_str(part, "call_id"));
                }
                _ => {}
            }
        }
        reopened.assistant_messages.push(committed);
        reopened.call_ids.push(call_ids);
    }
    for row in await_settled_usage(usage, session_id.as_str()).await?.rows {
        reopened.reported_usage_total = reopened
            .reported_usage_total
            .saturating_add(UsageBuckets::from_accounting_usage(&row.usage)?);
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
/// usage; see the module docs.
///
/// An attempt that reported all-zero usage carries no charge, so zero reports
/// are outside both sides of the comparison ([`delivered_usage`] drops them
/// too).
pub fn failed_attempt_usage_ledgered(sessions: &[SessionContent]) -> OracleVerdict {
    let mut checked = 0usize;
    for session in sessions.iter().filter(|s| s.has_failed_reported_attempt()) {
        let reported = session
            .emitted_attempts
            .iter()
            .filter_map(|attempt| attempt.usage)
            .filter(|usage| !usage.is_zero())
            .collect::<Vec<_>>();
        if let Err(message) = require_same_multiset(
            &reported,
            &session.delivered_usage,
            &format!(
                "`{}` every reported attempt's usage, failed attempts included, vs delivered facts",
                session.session
            ),
        )
        .and_then(|()| {
            require_reopened_total(
                session,
                &reported,
                "every reported attempt, failed included",
            )
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
        format!("{checked} session(s) delivered every failed attempt's reported usage"),
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
            "{} assistant messages, {} tool results and {} reported attempt usages matched byte for byte after reopen across {} sessions ({} with exact per-attempt accounting)",
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

        // An all-zero report carries no charge and is outside both sides
        // (see `failed_attempt_usage_ledgered`).
        let completed_usage = completed
            .iter()
            .filter_map(|attempt| attempt.usage)
            .filter(|usage| !usage.is_zero())
            .collect::<Vec<_>>();
        if session.has_failed_reported_attempt() {
            // A failed attempt's reported usage is FIG-3514's law; here every
            // completed attempt's usage must still be its own delivered fact.
            require_subset(
                &completed_usage,
                &session.delivered_usage,
                &format!(
                    "`{}` completed attempts' usage vs delivered facts",
                    session.session
                ),
            )?;
        } else {
            require_same_multiset(
                &completed_usage,
                &session.delivered_usage,
                &format!(
                    "`{}` reported attempt usage vs delivered facts",
                    session.session
                ),
            )?;
            require_reopened_total(session, &completed_usage, "every reported attempt")?;
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

fn require_reopened_total(
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
        .map(|reopened| reopened.reported_usage_total)
        .unwrap_or_default();
    if reopened != expected {
        return Err(format!(
            "`{}` reopened usage total diverged from {what}: emitted={expected:?} reopened={reopened:?}",
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
                "{what}: emitted {usage:?} has no delivered fact among {committed:?}"
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
    let budget = lash_protocol_standard::ToolRenderParams::default();
    for (emitted, committed) in emitted.iter().zip(committed) {
        rendered_projection_matches(
            &emitted.content,
            &committed.content,
            budget.value.max_chars,
            budget.max_lines,
        )
        .map_err(|detail| format!("`{}` {detail}", emitted.call_id))?;
    }
    Ok(())
}

/// Check the committed head and tail against the emitted rendering.
fn rendered_projection_matches(
    emitted: &str,
    committed: &str,
    max_chars: usize,
    max_lines: usize,
) -> Result<(), String> {
    if emitted.chars().count() <= max_chars && emitted.lines().count() <= max_lines {
        return describe_divergence(emitted, committed).map_or(Ok(()), Err);
    }
    let notice_start = committed.find("[output cut:").ok_or_else(|| {
        format!(
            "rendered output exceeded {max_chars} chars/{max_lines} lines but has no cut notice"
        )
    })?;
    let notice_end = committed[notice_start..]
        .find(']')
        .map(|index| notice_start + index + 1)
        .ok_or_else(|| "cut notice has no closing bracket".to_string())?;
    let head = &committed[..notice_start];
    let tail = &committed[notice_end..];
    if committed.chars().count() > max_chars
        || committed.lines().count() > max_lines
        || !emitted.starts_with(head)
        || !emitted.ends_with(tail)
    {
        return Err(format!(
            "committed head/tail is not within the cap or the emitted rendering: {}",
            describe_divergence(emitted, committed).unwrap_or_default()
        ));
    }
    Ok(())
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
            delivered_usage: vec![usage(u32::MAX.into(), 0), usage(1 << 40, 3)],
            reopened: Some(ReopenedSession {
                assistant_messages: vec![message("caf\u{e9} \u{0} \u{1f980}"), message("e\u{301}")],
                tool_results: vec![tool_result("{\"payload\":\"\\u0000\"}")],
                reported_usage_total: usage((1 << 40) + i64::from(u32::MAX), 3),
                call_ids: Vec::new(),
                result_call_ids: Vec::new(),
            }),
        }
    }

    /// A session whose first attempt reported usage and then failed, and whose
    /// retry completed, as FIG-3514 found it: only the retry's usage delivered.
    fn retried_like_today() -> SessionContent {
        SessionContent {
            session: "probe".to_string(),
            emitted_attempts: vec![
                attempt("", Some(usage(7, 0)), false),
                attempt("done", Some(usage(9, 4)), true),
            ],
            emitted_tool_results: Vec::new(),
            delivered_usage: vec![usage(9, 4)],
            reopened: Some(ReopenedSession {
                assistant_messages: vec![message("done")],
                tool_results: Vec::new(),
                reported_usage_total: usage(9, 4),
                call_ids: Vec::new(),
                result_call_ids: Vec::new(),
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
        summed_delta.delivered_usage = vec![usage((1 << 40) + i64::from(u32::MAX), 3)];
        let verdict = durable_content(&[summed_delta]);
        assert!(
            verdict.message.contains("vs delivered facts"),
            "{}",
            verdict.message
        );

        let mut ledger_drift = healthy();
        ledger_drift
            .reopened
            .as_mut()
            .expect("reopened")
            .reported_usage_total = usage(1, 1);
        let verdict = durable_content(&[ledger_drift]);
        assert!(
            verdict.message.contains("reopened usage total"),
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
        lost_completion.delivered_usage.clear();
        let verdict = durable_content(&[healthy(), lost_completion]);
        assert!(
            verdict.message.contains("has no delivered fact"),
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
        fixed.delivered_usage = vec![usage(7, 0), usage(9, 4)];
        fixed
            .reopened
            .as_mut()
            .expect("reopened")
            .reported_usage_total = usage(16, 4);
        let verdict = failed_attempt_usage_ledgered(&[healthy(), fixed.clone()]);
        assert!(verdict.is_passed(), "{}", verdict.message);
        // The fixed shape still satisfies the registered law, so registering
        // the failed-attempt law does not have to touch it.
        assert!(durable_content(&[healthy(), fixed]).is_passed());

        let vacuous = failed_attempt_usage_ledgered(&[healthy()]);
        assert!(vacuous.message.contains("vacuous"), "{}", vacuous.message);
    }

    #[test]
    fn failed_attempt_law_ignores_a_zero_usage_report() {
        // A failed attempt that reported all-zero usage owes no charge, and
        // both sides drop zero reports, so a session whose only failed report
        // is zero leaves nothing for the law to check.
        let mut zero = retried_like_today();
        zero.emitted_attempts[0].usage = Some(usage(0, 0));
        let only_zero = failed_attempt_usage_ledgered(&[healthy(), zero.clone()]);
        assert!(
            only_zero.message.contains("vacuous"),
            "{}",
            only_zero.message
        );

        let mut fixed = retried_like_today();
        fixed.delivered_usage = vec![usage(7, 0), usage(9, 4)];
        fixed
            .reopened
            .as_mut()
            .expect("reopened")
            .reported_usage_total = usage(16, 4);
        let verdict = failed_attempt_usage_ledgered(&[healthy(), fixed, zero]);
        assert!(verdict.is_passed(), "{}", verdict.message);
    }

    #[test]
    fn over_budget_tool_results_keep_head_and_tail_with_a_notice() {
        let emitted = format!("{}tail", "a".repeat(300));
        let notice = "[output cut: showing head and tail; full output: attachment full]";
        let committed = format!("{}{}{}", "a".repeat(40), notice, "tail");
        assert_eq!(
            rendered_projection_matches(&emitted, &committed, 180, 400),
            Ok(())
        );
        let wrong_head = format!("{}{}{}", "b".repeat(40), notice, "tail");
        assert!(rendered_projection_matches(&emitted, &wrong_head, 180, 400).is_err());
        assert!(rendered_projection_matches(&emitted, &emitted, 180, 400).is_err());
        assert_eq!(
            rendered_projection_matches("fits", "fits", 180, 400),
            Ok(())
        );
        assert!(rendered_projection_matches("fits", "fit", 180, 400).is_err());
    }

    #[test]
    fn divergence_reports_escape_controls_and_stay_on_char_boundaries() {
        let detail = describe_divergence("ab\u{1f980}\u{0}x", "ab\u{1f980}x").expect("differs");
        assert!(detail.contains("first byte difference at 6"), "{detail}");
        assert!(detail.contains("\\0"), "{detail}");
    }
}
