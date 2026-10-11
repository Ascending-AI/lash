//! The replay workload script: one readable JSON file per standard session.
//!
//! A script names its protocol, its session population and its turns. A turn
//! group repeats one user input and the model replies that answer it; `{turn}`
//! and `{session}` in any string expand to the turn's 1-based ordinal and the
//! session's 0-based index, so a 600-turn session is one group, not 600 rows.
//! Every reply is data: text, native tool calls or a code-mode cell in the
//! script's dialect. Nothing in a script reaches the network or a clock.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

/// The script format this module reads and the recorder writes.
pub const FORMAT: &str = "lash-perf-replay/1";

/// One standard replayable session, as committed under `workloads/replay/`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub format: String,
    pub name: String,
    pub description: String,
    pub protocol: Protocol,
    /// Sessions that run the turns at once, in lockstep rounds on one store.
    #[serde(default = "one")]
    pub sessions: usize,
    /// Compact each session's context after every this many turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_every: Option<usize>,
    pub turns: Vec<TurnGroup>,
}

fn one() -> usize {
    1
}

/// The session protocol a workload runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// The standard protocol: text and native tool calls.
    Standard,
    /// Code mode with TypeScript cells.
    Typescript,
    /// Code mode with Python cells.
    Python,
}

impl Protocol {
    /// The cell fence a code-mode reply is wrapped in.
    pub fn fence(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Standard => None,
            Self::Typescript => Some(("<typescript>", "</typescript>")),
            Self::Python => Some(("<python>", "</python>")),
        }
    }
}

/// `repeat` consecutive turns sharing one user input and reply script.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnGroup {
    #[serde(default = "one")]
    pub repeat: usize,
    /// Setup turns (loading a code-mode session's state, say) run and are
    /// recorded, but stay out of the per-turn medians and depth slopes: they
    /// are not the steady-state turn the workload measures.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup: bool,
    pub user: String,
    /// The model's replies in order: reply `n` answers the turn's `n`th model
    /// call. A call past the last reply is answered with plain text.
    pub replies: Vec<Reply>,
}

/// One scripted model reply.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    /// A plain text answer.
    Text(String),
    /// Native tool calls, executed together.
    ToolCalls(Vec<ToolCallSpec>),
    /// A code-mode cell's source, fenced in the script's dialect.
    Cell(Source),
}

/// Cell source, written in a script as an array of lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source(pub String);

impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.split('\n'))
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Text {
            Lines(Vec<String>),
            One(String),
        }
        Ok(Self(match Text::deserialize(deserializer)? {
            Text::Lines(lines) => lines.join("\n"),
            Text::One(text) => text,
        }))
    }
}

/// One native tool call a reply makes.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallSpec {
    pub tool: String,
    pub args: serde_json::Value,
}

/// One expanded turn: its ordinal, its user input and its replies.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptedTurn {
    /// 1-based position in the session's history.
    pub ordinal: usize,
    /// From a setup group: recorded, not summarized.
    pub setup: bool,
    pub user: String,
    pub replies: Vec<Reply>,
}

impl Workload {
    /// Parse and validate a script.
    pub fn parse(text: &str) -> Result<Self> {
        let workload: Self = serde_json::from_str(text).context("parse the replay script")?;
        workload.validate()?;
        Ok(workload)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.format == FORMAT,
            "replay script `{}` is format `{}`, not `{FORMAT}`",
            self.name,
            self.format
        );
        ensure!(
            !self.name.is_empty()
                && self
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "replay script names are lowercase kebab-case: `{}`",
            self.name
        );
        ensure!(self.sessions >= 1, "`{}` runs no session", self.name);
        ensure!(
            self.compact_every != Some(0),
            "`{}` compacts every zero turns",
            self.name
        );
        ensure!(!self.turns.is_empty(), "`{}` has no turns", self.name);
        for group in &self.turns {
            ensure!(
                group.repeat >= 1,
                "`{}` repeats a group zero times",
                self.name
            );
            ensure!(
                !group.replies.is_empty(),
                "`{}` has a turn without replies",
                self.name
            );
            for reply in &group.replies {
                match reply {
                    Reply::Cell(_) => ensure!(
                        self.protocol != Protocol::Standard,
                        "`{}` replies with a cell on the standard protocol",
                        self.name
                    ),
                    Reply::ToolCalls(calls) => {
                        ensure!(
                            self.protocol == Protocol::Standard,
                            "`{}` makes native tool calls in code mode; call tools from a cell",
                            self.name
                        );
                        ensure!(
                            !calls.is_empty(),
                            "`{}` makes an empty tool batch",
                            self.name
                        );
                    }
                    Reply::Text(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Turns in the script, before any `--turns` limit.
    pub fn turn_count(&self) -> usize {
        self.turns.iter().map(|group| group.repeat).sum()
    }

    /// The expanded turns of session `session`, the first `limit` of them when
    /// set.
    pub fn expand(&self, session: usize, limit: Option<usize>) -> Vec<ScriptedTurn> {
        let total = limit.map_or(self.turn_count(), |limit| limit.min(self.turn_count()));
        let mut turns = Vec::with_capacity(total);
        'groups: for group in &self.turns {
            for _ in 0..group.repeat {
                if turns.len() == total {
                    break 'groups;
                }
                let ordinal = turns.len() + 1;
                let vars = [
                    ("{turn}", ordinal.to_string()),
                    ("{session}", session.to_string()),
                ];
                turns.push(ScriptedTurn {
                    ordinal,
                    setup: group.setup,
                    user: expand(&group.user, &vars),
                    replies: group
                        .replies
                        .iter()
                        .map(|reply| expand_reply(reply, &vars))
                        .collect(),
                });
            }
        }
        turns
    }

    /// The SHA-256 of the script's canonical JSON: a receipt's workload
    /// identity, so edits to a script never compare against older receipts.
    pub fn digest(&self) -> String {
        use sha2::Digest as _;
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        format!("{:x}", sha2::Sha256::digest(canonical))
    }
}

fn expand(text: &str, vars: &[(&str, String)]) -> String {
    vars.iter().fold(text.to_owned(), |text, (key, value)| {
        text.replace(key, value)
    })
}

fn expand_value(value: &serde_json::Value, vars: &[(&str, String)]) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(expand(text, vars)),
        serde_json::Value::Array(items) => {
            items.iter().map(|item| expand_value(item, vars)).collect()
        }
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), expand_value(value, vars)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
        other => other.clone(),
    }
}

fn expand_reply(reply: &Reply, vars: &[(&str, String)]) -> Reply {
    match reply {
        Reply::Text(text) => Reply::Text(expand(text, vars)),
        Reply::Cell(Source(source)) => Reply::Cell(Source(expand(source, vars))),
        Reply::ToolCalls(calls) => Reply::ToolCalls(
            calls
                .iter()
                .map(|call| ToolCallSpec {
                    tool: call.tool.clone(),
                    args: expand_value(&call.args, vars),
                })
                .collect(),
        ),
    }
}

/// The committed corpus, compiled in so a test target needs no runfiles.
pub const CORPUS: [(&str, &str); 7] = [
    (
        "chat-short",
        include_str!("../../workloads/replay/chat-short.json"),
    ),
    (
        "chat-long-one-frame",
        include_str!("../../workloads/replay/chat-long-one-frame.json"),
    ),
    (
        "chat-long-compacting",
        include_str!("../../workloads/replay/chat-long-compacting.json"),
    ),
    (
        "ts-codemode",
        include_str!("../../workloads/replay/ts-codemode.json"),
    ),
    (
        "py-codemode",
        include_str!("../../workloads/replay/py-codemode.json"),
    ),
    (
        "concurrent-16",
        include_str!("../../workloads/replay/concurrent-16.json"),
    ),
    (
        "crash-resume",
        include_str!("../../workloads/replay/crash-resume.json"),
    ),
];

/// Every committed workload, by name.
pub fn corpus() -> Result<BTreeMap<String, Workload>> {
    CORPUS
        .iter()
        .map(|(name, text)| {
            let workload = Workload::parse(text).with_context(|| format!("workload `{name}`"))?;
            ensure!(
                workload.name == *name,
                "workloads/replay/{name}.json names itself `{}`",
                workload.name
            );
            Ok((workload.name.clone(), workload))
        })
        .collect()
}

/// The committed workload `name`, or a script file at that path.
pub fn resolve(name_or_path: &str) -> Result<Workload> {
    if let Some((_, text)) = CORPUS.iter().find(|(name, _)| *name == name_or_path) {
        return Workload::parse(text);
    }
    let text = std::fs::read_to_string(name_or_path).with_context(|| {
        format!(
            "`{name_or_path}` is neither a corpus workload ({}) nor a readable script",
            CORPUS.map(|(name, _)| name).join(", ")
        )
    })?;
    Workload::parse(&text)
}
