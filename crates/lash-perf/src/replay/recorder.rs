//! Capture a real session's committed transcript as a replay script, so the
//! corpus grows from real usage.
//!
//! Shapes survive, content does not: every user and assistant text becomes
//! deterministic filler of the same byte length, a native tool call becomes
//! a `fixture_large` call answering as many bytes as the real result did,
//! and a cell keeps its source (it must still run) with secret- and
//! PII-shaped literals replaced and its tool calls pointed at the fixtures.

use anyhow::{Context, Result, ensure};
use lash_core::transcript::{
    ToolResultBlock, TranscriptBlock, TranscriptItem, TranscriptMessage, TranscriptRole,
};

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
        .build()
        .map_err(|error| anyhow::anyhow!("build the reading backend: {error}"))?;
    let core = lash::LashCore::standard_builder(backend)
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lash-perf-replay-recorder"),
            lash::persistence::LeaseIncarnationId::new(format!("{}", std::process::id())),
        ))
        .map_err(|error| anyhow::anyhow!("build the reading core: {error}"))?;
    let id = lash::SessionId::try_from(args.session.clone())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let transcript = core
        .session(id)
        .durable()
        .await
        .map_err(|error| anyhow::anyhow!("open {}: {error}", args.session))?
        .transcript()
        .await
        .map_err(|error| anyhow::anyhow!("read the transcript: {error}"))?;
    let messages: Vec<TranscriptMessage> = transcript
        .visible()
        .filter_map(|entry| match &entry.item {
            TranscriptItem::Message(message) => Some(message.clone()),
            _ => None,
        })
        .collect();
    let workload = script(&args.name, args.protocol.into(), &messages)?;
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

/// The script `messages` (a committed transcript, in order) replay as.
pub fn script(name: &str, protocol: Protocol, messages: &[TranscriptMessage]) -> Result<Workload> {
    let mut turns: Vec<TurnGroup> = Vec::new();
    let mut pending_calls: Vec<(String, usize)> = Vec::new();
    for message in messages {
        match message.role {
            TranscriptRole::User => {
                let text = texts(&message.blocks);
                turns.push(TurnGroup {
                    repeat: 1,
                    setup: false,
                    user: filler("user", text.len()),
                    replies: Vec::new(),
                });
            }
            TranscriptRole::Assistant => {
                let Some(turn) = turns.last_mut() else {
                    continue;
                };
                let calls: Vec<(String, usize)> = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        TranscriptBlock::ToolCall { tool_name, .. } => {
                            Some((tool_name.clone(), turn.replies.len()))
                        }
                        _ => None,
                    })
                    .collect();
                let code = message.blocks.iter().find_map(|block| match block {
                    TranscriptBlock::Code { code } => Some(code.clone()),
                    _ => None,
                });
                let reply = if let Some(code) = code.filter(|_| protocol != Protocol::Standard) {
                    Reply::Cell(Source(redact_code(&code)))
                } else if !calls.is_empty() && protocol == Protocol::Standard {
                    pending_calls.extend(calls.iter().cloned());
                    Reply::ToolCalls(
                        calls
                            .iter()
                            .map(|(tool, _)| ToolCallSpec {
                                tool: LARGE.to_owned(),
                                args: serde_json::json!({ "key": tool, "bytes": 0 }),
                            })
                            .collect(),
                    )
                } else {
                    Reply::Text(filler("reply", texts(&message.blocks).len()))
                };
                turn.replies.push(reply);
            }
            TranscriptRole::Tool => {
                let Some(turn) = turns.last_mut() else {
                    continue;
                };
                for block in &message.blocks {
                    let TranscriptBlock::ToolResult { content, .. } = block else {
                        continue;
                    };
                    let bytes: usize = content
                        .iter()
                        .map(|block| match block {
                            ToolResultBlock::Text { text } => text.len(),
                            _ => 0,
                        })
                        .sum();
                    if pending_calls.is_empty() {
                        continue;
                    }
                    let (_, reply) = pending_calls.remove(0);
                    size_next_call(&mut turn.replies[reply], bytes);
                }
            }
            _ => {}
        }
    }
    turns.retain(|turn| !turn.replies.is_empty());
    ensure!(!turns.is_empty(), "the transcript has no answered turn");
    Ok(Workload {
        format: FORMAT.to_owned(),
        name: name.to_owned(),
        description: format!("Captured by `lash-perf replay-record`: {name}."),
        protocol,
        sessions: 1,
        compact_every: None,
        turns,
    })
}

/// Give the first unsized fixture call of `reply` the real result's size.
fn size_next_call(reply: &mut Reply, bytes: usize) {
    if let Reply::ToolCalls(calls) = reply
        && let Some(call) = calls
            .iter_mut()
            .find(|call| call.args.get("bytes") == Some(&serde_json::json!(0)))
    {
        call.args["bytes"] = serde_json::json!(bytes.max(1));
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
/// calls at the fixture that accepts any arguments.
pub fn redact_code(code: &str) -> String {
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
    rewrite_tools(&out)
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

/// `tools.<name>(` becomes `tools.fixture_lookup(`.
fn rewrite_tools(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(at) = rest.find("tools.") {
        out.push_str(&rest[..at + "tools.".len()]);
        rest = &rest[at + "tools.".len()..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if rest[name_len..].starts_with('(') {
            out.push_str(LOOKUP);
            rest = &rest[name_len..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: TranscriptRole, blocks: Vec<TranscriptBlock>) -> TranscriptMessage {
        TranscriptMessage { role, blocks }
    }

    /// REPLAY-RECORD: a captured standard session keeps its turn and reply
    /// shapes and its tool results' sizes, and none of its content: no user
    /// or assistant word and no tool argument survives verbatim.
    #[test]
    fn a_captured_session_keeps_shapes_and_drops_content() {
        let secret = "alice@example.com sk-live-4f9a8b7c6d5e4f3a2b1c";
        let messages = vec![
            message(
                TranscriptRole::User,
                vec![TranscriptBlock::Text {
                    text: format!("email {secret}"),
                }],
            ),
            message(
                TranscriptRole::Assistant,
                vec![TranscriptBlock::ToolCall {
                    call_id: lash_core::ToolCallId::fixture("c1"),
                    tool_name: "crm_lookup".to_owned(),
                    arguments: format!("{{\"q\":\"{secret}\"}}"),
                }],
            ),
            message(
                TranscriptRole::Tool,
                vec![TranscriptBlock::ToolResult {
                    call_id: lash_core::ToolCallId::fixture("c1"),
                    tool_name: "crm_lookup".to_owned(),
                    content: vec![ToolResultBlock::Text {
                        text: "r".repeat(4096),
                    }],
                }],
            ),
            message(
                TranscriptRole::Assistant,
                vec![TranscriptBlock::Text {
                    text: format!("found {secret}"),
                }],
            ),
        ];
        let workload = script("captured", Protocol::Standard, &messages).expect("a script");
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
                tool: LARGE.to_owned(),
                args: serde_json::json!({ "key": "crm_lookup", "bytes": 4096 }),
            }])
        );
        assert!(matches!(&turn.replies[1], Reply::Text(body) if body.len() == 6 + secret.len()));
        assert_eq!(
            redact_code("const r = await tools.crm_lookup({ q: \"bob@corp.io\", n: 5551234567 });"),
            "const r = await tools.fixture_lookup({ q: \"xxxxxxxxxxx\", n: xxxxxxxxxx });"
        );
    }
}
