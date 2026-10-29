//! Capture a real session's committed transcript as a replay script, so the
//! corpus grows from real usage.
//!
//! Shapes survive, content does not: every user and assistant text becomes
//! deterministic filler of the same byte length, a native tool call becomes
//! a `fixture_large` call answering as many bytes as the real result did,
//! and a cell keeps its source (it must still run) with secret- and
//! PII-shaped literals replaced and its tool calls pointed at the fixtures.
//!
//! The capture reads the typed transcript the session's own protocol decodes
//! (ADR 0129): its turns by their identity, its cells as the records they
//! committed and its tool results by the call they answer.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use lash_core::CellControl;
use lash_core::CellOutcome;
use lash_core::CellRecord;
use lash_core::ToolCallId;
use lash_core::transcript::{
    ToolResultBlock, TranscriptBlock, TranscriptEntry, TranscriptItem, TranscriptMessage,
    TranscriptRole,
};
use lash_vocabulary::identity::TurnId;

use super::provider::{LARGE, LOOKUP, large_text};
use super::script::{FORMAT, Protocol, Reply, Source, ToolCallSpec, TurnGroup, Workload};

/// What `lash-perf replay-record` captures.
#[derive(Debug, clap::Args)]
pub struct RecordArgs {
    /// The SQLite database of the host or workbench run.
    #[arg(long)]
    pub db: std::path::PathBuf,
    /// The session to capture.
    #[arg(long)]
    pub session: String,
    /// The new workload's name (lowercase kebab-case).
    #[arg(long)]
    pub name: String,
    /// The session's protocol.
    #[arg(long, value_enum)]
    pub protocol: RecordedProtocol,
    /// Where the script lands.
    #[arg(long)]
    pub out: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum RecordedProtocol {
    Standard,
    Typescript,
    Python,
}

impl From<RecordedProtocol> for Protocol {
    fn from(protocol: RecordedProtocol) -> Self {
        match protocol {
            RecordedProtocol::Standard => Self::Standard,
            RecordedProtocol::Typescript => Self::Typescript,
            RecordedProtocol::Python => Self::Python,
        }
    }
}

/// Capture `args.session` from `args.db` into `args.out`.
pub async fn record(args: &RecordArgs) -> Result<()> {
    let stores =
        lash::sqlite::SqliteStoreSet::open(&args.db, lash::sqlite::SqliteSynchronous::Normal)
            .await
            .map_err(|error| anyhow::anyhow!("open {}: {error}", args.db.display()))?;
    let backend = lash::durable::DurableBackendBuilder::new(std::sync::Arc::new(stores))
        .config(lash::durable::DurableSettings::standard())
        .build()
        .map_err(|error| anyhow::anyhow!("build the reading backend: {error}"))?;
    let core = reading_builder(args.protocol, backend)
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lash-perf-replay-recorder"),
            lash::persistence::LeaseIncarnationId::new(format!("{}", std::process::id())),
        ))
        .map_err(|error| anyhow::anyhow!("build the reading core: {error}"))?;
    let id = lash_kernel_value::identity::SessionId::try_from(args.session.clone())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let transcript = core
        .session(id)
        .durable()
        .await
        .map_err(|error| anyhow::anyhow!("open {}: {error}", args.session))?
        .transcript()
        .await
        .map_err(|error| anyhow::anyhow!("read the transcript: {error}"))?;
    let workload = script(&args.name, args.protocol.into(), transcript.visible())?;
    let text = serde_json::to_string_pretty(&workload)?;
    Workload::parse(&text).context("the captured script does not validate")?;
    std::fs::write(&args.out, format!("{text}\n"))
        .with_context(|| format!("write {}", args.out.display()))?;
    core.shutdown()
        .await
        .map_err(|error| anyhow::anyhow!("shut the reading core down: {error}"))?;
    println!(
        "replay record: {} turns of `{}` -> {}",
        workload.turn_count(),
        args.session,
        args.out.display()
    );
    Ok(())
}

/// A core over `backend` holding the recorded protocol, whose offline
/// decoder reads the session's protocol records. It never runs a cell; its
/// bounds are only ones the default worker pool admits.
fn reading_builder(protocol: RecordedProtocol, backend: lash::Backend) -> lash::LashCoreBuilder {
    let dialect = match protocol {
        RecordedProtocol::Standard => return lash::LashCore::standard_builder(backend),
        RecordedProtocol::Typescript => lash::rlm::CellDialect::typescript(),
        RecordedProtocol::Python => lash::rlm::CellDialect::python(),
    };
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .instruction_limit(lash::rlm::InstructionBound::instructions(50_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        dialect,
    );
    lash::LashCore::rlm_builder(backend, factory)
}

/// The script `entries` (a committed transcript's visible entries, in
/// order) replay as.
///
/// A turn is its entries' turn identity. Its scripted user input is as long
/// as its inputs' texts joined by newlines. Its replies are the model's, in
/// order: an assistant text, a native tool batch (standard) or a cell (code
/// mode). Not replies: reasoning alone, a cell's companion assistant context
/// (its cell is the reply) and the reply a cell's `finish` sealed (no model
/// call followed it). A tool result sizes the exact call its id names; one
/// naming no open call sizes nothing. An entry outside every turn (a
/// plugin's context, a refused exchange's results) is not scripted. A turn
/// the model never answered is left out.
pub fn script<'a>(
    name: &str,
    protocol: Protocol,
    entries: impl IntoIterator<Item = &'a TranscriptEntry>,
) -> Result<Workload> {
    let mut capture = Capture::default();
    for entry in entries {
        let turn = entry.provenance.turn_id.as_ref();
        match &entry.item {
            TranscriptItem::Message(message) if message.role == TranscriptRole::Tool => {
                capture.answer(message);
            }
            TranscriptItem::Message(message) => {
                let Some(turn) = turn else {
                    continue;
                };
                if entry.provenance.cell_id.is_none() {
                    capture.message(protocol, turn, entry.provenance.is_turn_reply, message)?;
                }
            }
            TranscriptItem::Cell(cell) => {
                ensure!(
                    protocol != Protocol::Standard,
                    "the session ran code cells; record it with its cell dialect's protocol"
                );
                let turn = turn.with_context(|| format!("cell `{}` is in no turn", cell.id))?;
                capture.cell(protocol, turn, cell);
            }
            TranscriptItem::Suppressed(_) => {}
        }
    }
    if let Some(call) = capture.open_calls.keys().next() {
        bail!("tool call `{call}` has no committed result; capture a settled session");
    }
    let turns: Vec<TurnGroup> = capture
        .turns
        .into_iter()
        .filter(|turn| !turn.replies.is_empty())
        .map(|turn| TurnGroup {
            capability_turn: None,
            repeat: 1,
            setup: false,
            oracle: None,
            user: filler("user", turn.user_bytes),
            replies: turn.replies,
        })
        .collect();
    ensure!(!turns.is_empty(), "the transcript has no answered turn");
    Ok(Workload {
        format: FORMAT.to_owned(),
        name: name.to_owned(),
        description: format!("Captured by `lash-perf replay-record`: {name}."),
        protocol,
        host: None,
        instruction_limit: None,
        memory_limit_mib: None,
        sessions: 1,
        compact_every: None,
        turns,
    })
}

/// The turns captured so far, and the native calls still awaiting their
/// result.
#[derive(Default)]
struct Capture {
    turns: Vec<CapturedTurn>,
    turn_index: HashMap<TurnId, usize>,
    open_calls: HashMap<ToolCallId, CallSlot>,
}

#[derive(Default)]
struct CapturedTurn {
    /// Its inputs' texts joined by newlines, in bytes.
    user_bytes: usize,
    inputs: usize,
    replies: Vec<Reply>,
    /// A cell's `finish` ended the turn: the sealed reply that follows is
    /// its value, not a model reply.
    finished_by_cell: bool,
}

/// Where an open native call sits: its turn, its reply and its place in
/// that reply's batch.
struct CallSlot {
    turn: usize,
    reply: usize,
    call: usize,
}

impl Capture {
    fn turn(&mut self, id: &TurnId) -> usize {
        *self.turn_index.entry(id.clone()).or_insert_with(|| {
            self.turns.push(CapturedTurn::default());
            self.turns.len() - 1
        })
    }

    fn message(
        &mut self,
        protocol: Protocol,
        turn: &TurnId,
        is_turn_reply: bool,
        message: &TranscriptMessage,
    ) -> Result<()> {
        let index = self.turn(turn);
        let turn = &mut self.turns[index];
        match message.role {
            TranscriptRole::User => {
                let bytes = texts(&message.blocks).len();
                turn.user_bytes += bytes + usize::from(turn.inputs > 0);
                turn.inputs += 1;
            }
            TranscriptRole::Assistant => {
                let calls: Vec<(&ToolCallId, &str)> = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        TranscriptBlock::ToolCall {
                            call_id, tool_name, ..
                        } => Some((call_id, tool_name.as_str())),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    ensure!(
                        protocol == Protocol::Standard,
                        "a code-mode reply makes native tool calls that ran no cell; a replay \
                         script cannot express it"
                    );
                    let reply = turn.replies.len();
                    for (call, (id, _)) in calls.iter().enumerate() {
                        let slot = CallSlot {
                            turn: index,
                            reply,
                            call,
                        };
                        ensure!(
                            self.open_calls.insert((*id).clone(), slot).is_none(),
                            "tool call `{id}` is made twice"
                        );
                    }
                    turn.replies.push(Reply::ToolCalls(
                        calls
                            .iter()
                            .map(|(_, tool)| ToolCallSpec {
                                tool: format!("tools.{LARGE}"),
                                // `answer` sizes it; an unanswered call refuses the capture.
                                args: serde_json::json!({ "key": tool, "bytes": 0 }),
                            })
                            .collect(),
                    ));
                } else if message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, TranscriptBlock::Text { .. }))
                    && !(is_turn_reply && turn.finished_by_cell)
                {
                    turn.replies
                        .push(Reply::Text(filler("reply", texts(&message.blocks).len())));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn cell(&mut self, protocol: Protocol, turn: &TurnId, cell: &CellRecord) {
        let index = self.turn(turn);
        let turn = &mut self.turns[index];
        turn.replies
            .push(Reply::Cell(Source(redact_code(protocol, &cell.code))));
        turn.finished_by_cell = matches!(
            cell.result,
            CellOutcome::Controlled {
                control: CellControl::Finish { .. },
                ..
            }
        );
    }

    /// Size each open call `message` answers with its result's text bytes;
    /// an observed empty result answers one byte.
    fn answer(&mut self, message: &TranscriptMessage) {
        for block in &message.blocks {
            let TranscriptBlock::ToolResult {
                call_id, content, ..
            } = block
            else {
                continue;
            };
            let Some(slot) = self.open_calls.remove(call_id) else {
                continue;
            };
            let bytes: usize = content
                .iter()
                .map(|block| match block {
                    ToolResultBlock::Text { text } => text.len(),
                    _ => 0,
                })
                .sum();
            if let Reply::ToolCalls(calls) = &mut self.turns[slot.turn].replies[slot.reply] {
                calls[slot.call].args["bytes"] = serde_json::json!(bytes.max(1));
            }
        }
    }
}

fn texts(blocks: &[TranscriptBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Deterministic printable filler of `bytes` bytes, keyed by its role.
fn filler(role: &str, bytes: usize) -> String {
    large_text(role, bytes.max(1))
}

/// Replace secret- and PII-shaped tokens in cell source, and point its tool
/// calls at the fixture that accepts any arguments, spelled as `protocol`'s
/// dialect writes it.
pub fn redact_code(protocol: Protocol, code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if looks_sensitive(token) {
            out.push_str(&"x".repeat(token.chars().count()));
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for c in code.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '@' | '.' | '+') {
            token.push(c);
        } else {
            flush(&mut token, &mut out);
            out.push(c);
        }
    }
    flush(&mut token, &mut out);
    let lookup = match protocol {
        Protocol::Typescript => "fixtureLookup",
        _ => LOOKUP,
    };
    rewrite_tools(&out, lookup)
}

/// An email address, a long key- or token-like run, or a long digit run (a
/// phone or card number).
fn looks_sensitive(token: &str) -> bool {
    let digits = token.chars().filter(char::is_ascii_digit).count();
    let letters = token.chars().filter(char::is_ascii_alphabetic).count();
    (token.contains('@') && token.contains('.') && !token.starts_with('@'))
        || (token.len() >= 20 && digits >= 4 && letters >= 4 && !token.contains('.'))
        || digits >= 9
}

/// `tools.<name>(` becomes `tools.<lookup>(`.
fn rewrite_tools(code: &str, lookup: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(at) = rest.find("tools.") {
        out.push_str(&rest[..at + "tools.".len()]);
        rest = &rest[at + "tools.".len()..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if rest[name_len..].starts_with('(') {
            out.push_str(lookup);
            rest = &rest[name_len..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::transcript::EntryProvenance;

    /// A committed entry of turn `turn` (none: outside every turn).
    fn entry(turn: Option<&str>, item: TranscriptItem) -> TranscriptEntry {
        let provenance = EntryProvenance {
            turn_id: turn.map(lash_vocabulary::identity::TurnId::fixture),
            ..EntryProvenance::default()
        };
        serde_json::from_value(serde_json::json!({
            "entry_id": "node-1",
            "provenance": provenance,
            "timestamp": "2026-01-01T00:00:00.000000000Z",
            "item": item,
        }))
        .expect("an entry")
    }

    fn message(
        turn: Option<&str>,
        role: TranscriptRole,
        blocks: Vec<TranscriptBlock>,
    ) -> TranscriptEntry {
        entry(
            turn,
            TranscriptItem::Message(TranscriptMessage { role, blocks }),
        )
    }

    fn text(text: &str) -> TranscriptBlock {
        TranscriptBlock::Text {
            text: text.to_owned(),
        }
    }

    fn call(id: &str, tool: &str) -> TranscriptBlock {
        TranscriptBlock::ToolCall {
            call_id: lash_core::ToolCallId::fixture(id),
            tool_name: tool.to_owned(),
            arguments: "{}".to_owned(),
        }
    }

    fn result(id: &str, bytes: usize) -> TranscriptBlock {
        TranscriptBlock::ToolResult {
            call_id: lash_core::ToolCallId::fixture(id),
            tool_name: "read".to_owned(),
            content: vec![ToolResultBlock::Text {
                text: "r".repeat(bytes),
            }],
        }
    }

    fn sizes(reply: &Reply) -> Vec<(String, u64)> {
        let Reply::ToolCalls(calls) = reply else {
            panic!("not a tool batch: {reply:?}")
        };
        calls
            .iter()
            .map(|call| {
                (
                    call.args["key"].as_str().unwrap().to_owned(),
                    call.args["bytes"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    /// REPLAY-RECORD: a captured standard session keeps its turn and reply
    /// shapes and its tool results' sizes, and none of its content: no user
    /// or assistant word and no tool argument survives verbatim.
    #[test]
    fn a_captured_session_keeps_shapes_and_drops_content() {
        let secret = "alice@example.com sk-live-4f9a8b7c6d5e4f3a2b1c";
        let t = Some("t1");
        let entries = vec![
            message(
                t,
                TranscriptRole::User,
                vec![text(&format!("email {secret}"))],
            ),
            message(
                t,
                TranscriptRole::Assistant,
                vec![TranscriptBlock::ToolCall {
                    call_id: lash_core::ToolCallId::fixture("c1"),
                    tool_name: "crm_lookup".to_owned(),
                    arguments: format!("{{\"q\":\"{secret}\"}}"),
                }],
            ),
            message(t, TranscriptRole::Tool, vec![result("c1", 4096)]),
            message(
                t,
                TranscriptRole::Assistant,
                vec![text(&format!("found {secret}"))],
            ),
        ];
        let workload = script("captured", Protocol::Standard, &entries).expect("a script");
        let text = serde_json::to_string(&workload).expect("encodes");
        assert!(
            !text.contains("alice") && !text.contains("sk-live"),
            "{text}"
        );
        assert_eq!(workload.turns.len(), 1);
        let turn = &workload.turns[0];
        assert_eq!(turn.user.len(), format!("email {secret}").len());
        assert_eq!(
            turn.replies[0],
            Reply::ToolCalls(vec![ToolCallSpec {
                tool: format!("tools.{LARGE}"),
                args: serde_json::json!({ "key": "crm_lookup", "bytes": 4096 }),
            }])
        );
        assert!(matches!(&turn.replies[1], Reply::Text(body) if body.len() == 6 + secret.len()));
        assert_eq!(
            redact_code(
                Protocol::Typescript,
                "const r = await tools.crmLookup({ q: \"bob@corp.io\", n: 5551234567 });"
            ),
            "const r = await tools.fixtureLookup({ q: \"xxxxxxxxxxx\", n: xxxxxxxxxx });"
        );
    }

    /// REPLAY-RECORD call identity: a tool result sizes the call its id
    /// names, wherever that call sits: results answering out of order or a
    /// later turn, or under a repeated tool name, size their own calls; a
    /// result naming no open call (unknown, or answered already) sizes
    /// nothing; and a call no result answers refuses the capture.
    #[test]
    fn a_tool_result_sizes_the_call_its_id_names() {
        let (one, two) = (Some("t1"), Some("t2"));
        let mut entries = vec![
            message(one, TranscriptRole::User, vec![text("go")]),
            message(
                one,
                TranscriptRole::Assistant,
                vec![call("a", "read"), call("b", "read"), call("c", "grep")],
            ),
            message(
                one,
                TranscriptRole::Tool,
                vec![result("b", 4096), result("zz", 7), result("a", 16)],
            ),
            message(one, TranscriptRole::Assistant, vec![text("ok")]),
            message(two, TranscriptRole::User, vec![text("again")]),
            // A result outside every turn still answers its call.
            message(
                None,
                TranscriptRole::Tool,
                vec![result("c", 300), result("b", 9)],
            ),
            message(two, TranscriptRole::Assistant, vec![text("done")]),
        ];
        let workload = script("captured", Protocol::Standard, &entries).expect("a script");
        assert_eq!(workload.turns.len(), 2);
        assert_eq!(
            sizes(&workload.turns[0].replies[0]),
            [
                ("read".to_owned(), 16),
                ("read".to_owned(), 4096),
                ("grep".to_owned(), 300)
            ]
        );
        entries.insert(
            3,
            message(one, TranscriptRole::Assistant, vec![call("d", "read")]),
        );
        let refusal = script("captured", Protocol::Standard, &entries)
            .expect_err("an unanswered call refuses the capture");
        let unanswered = lash_core::ToolCallId::fixture("d").to_string();
        assert!(refusal.to_string().contains(&unanswered), "{refusal}");
    }

    fn cell(id: &str, code: &str, result: CellOutcome) -> TranscriptItem {
        TranscriptItem::Cell(std::sync::Arc::new(CellRecord {
            read_facts: Vec::new(),
            consumption: Vec::new(),
            id: id.to_owned(),
            protocol_iteration: 0,
            language: "typescript".to_owned(),
            code: code.to_owned(),
            prints: Vec::new(),
            prints_retained: None,
            images: Vec::new(),
            calls: Vec::new(),
            bindings: Default::default(),
            result,
        }))
    }

    /// REPLAY-RECORD typed capture: a code-mode turn's replies are its cells,
    /// once each whatever companion context their channel committed, and the
    /// model's text answer; the reply a cell's `finish` sealed is no model
    /// reply. Inputs one turn admitted together script one user input as
    /// long as their joined text.
    #[test]
    fn a_code_mode_turn_scripts_its_cells_once() {
        let (one, two) = (Some("t1"), Some("t2"));
        let context = |turn, cell_id: &str, blocks| {
            let mut entry = message(turn, TranscriptRole::Assistant, blocks);
            entry.provenance.cell_id = Some(cell_id.to_owned());
            entry
        };
        let reasoning = TranscriptBlock::Reasoning {
            text: "think".to_owned(),
        };
        let finish = CellOutcome::Controlled {
            tool_name: "finish".to_owned(),
            call_id: lash_core::ToolCallId::fixture("f"),
            control: CellControl::Finish {
                value: serde_json::json!("v").into(),
            },
        };
        let mut sealed = message(two, TranscriptRole::Assistant, vec![text("v")]);
        sealed.provenance.is_turn_reply = true;
        let entries = vec![
            message(one, TranscriptRole::User, vec![text("first")]),
            message(one, TranscriptRole::User, vec![text("second")]),
            message(one, TranscriptRole::Assistant, vec![reasoning]),
            entry(one, cell("c1", "console.log(1);", CellOutcome::Completed)),
            context(one, "c1", vec![text("console.log(1);")]),
            // The native channel's context carries the cell's call.
            entry(one, cell("c2", "console.log(2);", CellOutcome::Completed)),
            context(one, "c2", vec![call("x", "execute_code")]),
            message(one, TranscriptRole::Assistant, vec![text("done")]),
            message(two, TranscriptRole::User, vec![text("end")]),
            entry(two, cell("c3", "finish(\"v\");", finish)),
            sealed,
        ];
        let workload = script("captured", Protocol::Typescript, &entries).expect("a script");
        Workload::parse(&serde_json::to_string(&workload).unwrap()).expect("a valid script");
        let source = |code: &str| Reply::Cell(Source(code.to_owned()));
        assert_eq!(workload.turns.len(), 2);
        assert_eq!(workload.turns[0].user.len(), "first\nsecond".len());
        assert_eq!(
            workload.turns[0].replies,
            [
                source("console.log(1);"),
                source("console.log(2);"),
                Reply::Text(filler("reply", 4)),
            ]
        );
        assert_eq!(workload.turns[1].replies, [source("finish(\"v\");")]);
    }
}
