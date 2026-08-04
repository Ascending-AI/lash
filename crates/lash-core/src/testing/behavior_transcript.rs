//! The normalized behavior-transcript vocabulary every scenario harness
//! renders into.
//!
//! A **behavior transcript** is the review artifact named by
//! `docs/agents/way-of-working.md` ("Expect tests versus conformance
//! assertions"): a short, deterministic rendering of what one scenario actually
//! did, judged as *one* diff. This module owns the vocabulary every harness
//! renders into, so the `Transcript:` PR rule in `docs/agents/pr-style.md`
//! governs a shared grammar instead of one bespoke renderer. ADR 0050 records
//! the decision and its rationale.
//!
//! It is deliberately **not** a lash type. Nothing in this module imports
//! anything else from `lash_core`; it consumes only strings, integers, booleans
//! and `serde_json::Value`. A harness extracts real facts from real product
//! state and hands them over already reduced to those primitives, which is what
//! keeps a transcript from becoming "a snapshot of facts the harness itself
//! constructed" (ADR 0044). The renderer cannot re-derive a fact, because it
//! cannot see one.
//!
//! # Grammar
//!
//! One line per recorded [`Entry`], four columns, plus indented component lines
//! under a durable commit:
//!
//! ```text
//! <actor>      <kind>    <event>                 [<key>=<value> ...]
//! ```
//!
//! ```text
//! root         ingress   turn.start              input="text"
//! root         provider  model.request           iteration=1
//! root         spawn     process.start           process=process-001 label="child"
//! process-001  outcome   process.success         value="left"
//! root         commit    checkpoint.commit       rev=0->1
//! root                     usage                 entries=0 input=0 output=0 cache_read=0 cache_write=0 reasoning=0 total=0
//! root                     turn_state            stored logical=412B
//! root                     tool_state            ref (unchanged)
//! root         outcome   turn.final_value        value={"joined":["left","right"]}
//! ```
//!
//! # The four families
//!
//! Every [`Kind`] belongs to exactly one [`Family`], and the four families are
//! the whole vocabulary:
//!
//! - [`Family::Boundary`] — execution crossing a durable seam or changing hands:
//!   [`Kind::Ingress`], [`Kind::Park`], [`Kind::Resume`], [`Kind::Cancel`],
//!   [`Kind::Worker`], [`Kind::Fault`].
//! - [`Family::Step`] — causal / ordering events inside a boundary:
//!   [`Kind::Provider`], [`Kind::Tool`], [`Kind::Exec`], [`Kind::Spawn`],
//!   [`Kind::Await`], [`Kind::Wake`], [`Kind::Lease`], [`Kind::Observe`].
//! - [`Family::DurableWrite`] — what reached durable storage:
//!   [`Kind::Commit`], [`Kind::Effect`].
//! - [`Family::Terminal`] — how an actor ended: [`Kind::Outcome`].
//!
//! Adding a fifth family is a vocabulary change and belongs in the ADR, not in
//! a harness. Adding an *event name* inside a family does not.
//!
//! # Why there are no sequence numbers and no clock
//!
//! Order is carried by line position. A global sequence column renumbers the
//! whole tail whenever one event is inserted, which turns a one-line behavior
//! change into a whole-artifact diff — the opposite of "stable under irrelevant
//! change". For the same reason nothing here can render a wall-clock time, an
//! elapsed duration, or a raw identifier: durations are not offered at all, and
//! identifiers only enter through [`Attr::id`], which replaces them with a
//! first-mention alias (`process-001`). Free text goes through a scrubber that
//! collapses whitespace, masks UUID- and hash-shaped substrings, and truncates.
//!
//! # Review budget
//!
//! [`Transcript::render`] panics past [`DEFAULT_REVIEW_BUDGET_LINES`]. A
//! transcript too long to read in one sitting is decoration, and the doctrine's
//! "no decoration" rule is worth enforcing where it is cheapest. A harness that
//! genuinely needs more says so visibly with
//! [`Transcript::with_review_budget`].

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Rendered-line budget for one reviewable transcript.
pub const DEFAULT_REVIEW_BUDGET_LINES: usize = 80;

/// Character budget for one scrubbed free-text attribute value.
pub const TEXT_BUDGET_CHARS: usize = 72;

/// Character budget for one canonical-JSON attribute value.
pub const JSON_BUDGET_CHARS: usize = 96;

/// Durable-write event names the vocabulary can emit.
///
/// `scripts/check-transcript-diff.py` keys the `Transcript:` justification rule
/// on these tokens (plus the `rev=a->b`, `stored logical=` and
/// `ref (unchanged)` renderings below). Keep the two in sync: a durable event
/// name the script does not know about is a durable-semantics change that can
/// land unremarked.
pub const DURABLE_WRITE_EVENTS: &[&str] = &[
    CHECKPOINT_COMMIT_EVENT,
    DURABLE_EFFECT_EVENT,
    super::sansio_transcript::CHECKPOINT_REQUEST_EVENT,
];

/// Event name for a successful runtime-checkpoint commit.
pub const CHECKPOINT_COMMIT_EVENT: &str = "checkpoint.commit";

/// Event name for a journaled durable effect.
pub const DURABLE_EFFECT_EVENT: &str = "durable.effect";

const ACTOR_WIDTH: usize = 11;
const KIND_WIDTH: usize = 8;
const EVENT_WIDTH: usize = 22;

/// The four line families of the vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// Execution crossing a durable seam.
    Boundary,
    /// A causal / ordering event inside a boundary.
    Step,
    /// Something that reached durable storage.
    DurableWrite,
    /// How an actor ended.
    Terminal,
}

impl Family {
    /// Stable lowercase name, used in diagnostics and docs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::Step => "step",
            Self::DurableWrite => "durable-write",
            Self::Terminal => "terminal",
        }
    }
}

/// The closed set of transcript line kinds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// Work entered an actor (turn input, queued work, trigger delivery).
    Ingress,
    /// The actor released its live runtime at a boundary.
    Park,
    /// The actor re-entered after a park.
    Resume,
    /// Work was withdrawn before it settled.
    Cancel,
    /// A worker / lease owner started, crashed, or took over.
    Worker,
    /// An injected world fault reached the actor (backend error, provider
    /// mutation, torn connection).
    Fault,
    /// A model request.
    Provider,
    /// A tool attempt.
    Tool,
    /// A code / cell execution block.
    Exec,
    /// Child work was created (process, subagent, child session).
    Spawn,
    /// A wait was registered on a handle or promise.
    Await,
    /// A wait was satisfied, or its delivery discarded.
    Wake,
    /// A lease was claimed, renewed, superseded, or lost.
    Lease,
    /// Observer-visible activity crossed the observation boundary: a message the
    /// protocol made visible to the session, a replay gap, a visibility change.
    Observe,
    /// A runtime-checkpoint commit.
    Commit,
    /// A durable effect reached the journal.
    Effect,
    /// An actor reached a terminal state.
    Outcome,
}

impl Kind {
    /// The family this kind belongs to.
    pub const fn family(self) -> Family {
        match self {
            Self::Ingress
            | Self::Park
            | Self::Resume
            | Self::Cancel
            | Self::Worker
            | Self::Fault => Family::Boundary,
            Self::Provider
            | Self::Tool
            | Self::Exec
            | Self::Spawn
            | Self::Await
            | Self::Wake
            | Self::Lease
            | Self::Observe => Family::Step,
            Self::Commit | Self::Effect => Family::DurableWrite,
            Self::Outcome => Family::Terminal,
        }
    }

    /// The rendered column token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingress => "ingress",
            Self::Park => "park",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
            Self::Worker => "worker",
            Self::Fault => "fault",
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Exec => "exec",
            Self::Spawn => "spawn",
            Self::Await => "await",
            Self::Wake => "wake",
            Self::Lease => "lease",
            Self::Observe => "observe",
            Self::Commit => "commit",
            Self::Effect => "effect",
            Self::Outcome => "outcome",
        }
    }
}

/// Identifier namespaces the renderer knows how to normalize.
///
/// Every namespace gets its own first-mention counter, so a transcript reads
/// `process-001` / `process-002` regardless of what the runtime actually
/// minted.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum IdKind {
    /// A session.
    Session,
    /// A runtime process.
    Process,
    /// A session-history node.
    Node,
    /// A tool call.
    Call,
    /// An await-event key.
    Key,
    /// A durable effect.
    Effect,
    /// An attachment.
    Attachment,
    /// A worker / lease owner.
    Worker,
}

impl IdKind {
    /// Alias prefix for this namespace.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Process => "process",
            Self::Node => "node",
            Self::Call => "call",
            Self::Key => "key",
            Self::Effect => "effect",
            Self::Attachment => "attachment",
            Self::Worker => "worker",
        }
    }
}

/// Who a transcript line is attributed to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Actor {
    kind: IdKind,
    raw: String,
}

impl Actor {
    /// A session actor.
    pub fn session(raw: impl Into<String>) -> Self {
        Self {
            kind: IdKind::Session,
            raw: raw.into(),
        }
    }

    /// A runtime-process actor.
    pub fn process(raw: impl Into<String>) -> Self {
        Self {
            kind: IdKind::Process,
            raw: raw.into(),
        }
    }

    /// A worker / lease-owner actor.
    pub fn worker(raw: impl Into<String>) -> Self {
        Self {
            kind: IdKind::Worker,
            raw: raw.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AttrValue {
    Text(String),
    Token(String),
    Int(u64),
    Flag(bool),
    Bytes(usize),
    Id(IdKind, String),
    Json(String),
    Revision(u64, u64),
}

/// One `key=value` attribute on a transcript line.
///
/// The constructors are the only way in, which is what keeps unnormalized data
/// out: there is no `Attr::raw`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attr {
    key: &'static str,
    value: AttrValue,
}

impl Attr {
    /// Free text: whitespace-collapsed, identifier-masked, truncated, quoted.
    pub fn text(key: &'static str, value: impl AsRef<str>) -> Self {
        Self {
            key,
            value: AttrValue::Text(scrub_text(value.as_ref())),
        }
    }

    /// A closed-vocabulary label such as a status, outcome or checkpoint kind.
    ///
    /// Rendered unquoted, which is what makes `outcome=success` read as a
    /// classification rather than as data. Whitespace is replaced so the
    /// `key=value` grammar cannot be broken from a call site.
    pub fn token(key: &'static str, value: impl AsRef<str>) -> Self {
        Self {
            key,
            value: AttrValue::Token(
                mask_identifiers(value.as_ref())
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("_"),
            ),
        }
    }

    /// A closed-vocabulary label taken from a unit-variant `Debug` name.
    ///
    /// `AfterWork` renders as `after_work`. Enums whose variants are the
    /// classification a reviewer reads are common in lash and mostly lack an
    /// `as_str`; converting here keeps every token in one normal form instead of
    /// letting `Debug` casing leak into the reviewed text.
    pub fn debug_token(key: &'static str, value: &impl std::fmt::Debug) -> Self {
        Self::token(key, snake_case(&format!("{value:?}")))
    }

    /// A durable revision transition, rendered `rev=0->1`.
    ///
    /// The rendering is fixed here on purpose: it is one of the tokens
    /// `scripts/check-transcript-diff.py` treats as durable semantics.
    pub fn revision(before: u64, after: u64) -> Self {
        Self {
            key: "rev",
            value: AttrValue::Revision(before, after),
        }
    }

    /// A count or index. Never a timestamp or a duration.
    pub fn int(key: &'static str, value: u64) -> Self {
        Self {
            key,
            value: AttrValue::Int(value),
        }
    }

    /// A boolean fact.
    pub fn flag(key: &'static str, value: bool) -> Self {
        Self {
            key,
            value: AttrValue::Flag(value),
        }
    }

    /// A logical size, rendered `412B` / `1.2KB`.
    pub fn bytes(key: &'static str, value: usize) -> Self {
        Self {
            key,
            value: AttrValue::Bytes(value),
        }
    }

    /// A runtime identifier, replaced at render time by a first-mention alias.
    pub fn id(key: &'static str, kind: IdKind, raw: impl Into<String>) -> Self {
        Self {
            key,
            value: AttrValue::Id(kind, raw.into()),
        }
    }

    /// A JSON value, rendered as canonical compact JSON, masked and truncated.
    pub fn json(key: &'static str, value: &serde_json::Value) -> Self {
        let encoded = serde_json::to_string(value).unwrap_or_else(|_| "<unencodable>".to_string());
        Self {
            key,
            value: AttrValue::Json(truncate(&mask_identifiers(&encoded), JSON_BUDGET_CHARS)),
        }
    }
}

/// Whether a checkpoint component carried a body at the commit seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentBody {
    /// The body was present. `logical_bytes` is an encoding-independent size
    /// used only for human comparison.
    Stored { logical_bytes: Option<usize> },
    /// Only the prior reference was carried; no new body was written.
    UnchangedRef,
}

/// One checkpoint component observed under a [`Kind::Commit`] line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Component {
    name: String,
    body: ComponentBody,
}

/// Typed usage facts carried by every [`Kind::Commit`] entry.
///
/// This is separate from [`Component`]: usage is accounting data, not an
/// opaque checkpoint body whose byte size says whether it changed. The
/// renderer requires this value for every commit and always emits it as one
/// line, so a harness cannot silently omit accounting from its golden.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Usage {
    entries: usize,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_write_input_tokens: i64,
    reasoning_output_tokens: i64,
}

impl Usage {
    /// Usage rows and canonical token buckets submitted by one commit.
    pub const fn new(
        entries: usize,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_input_tokens: i64,
        cache_write_input_tokens: i64,
        reasoning_output_tokens: i64,
    ) -> Self {
        Self {
            entries,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
        }
    }

    /// A commit seam that submitted no usage rows.
    pub const fn none() -> Self {
        Self::new(0, 0, 0, 0, 0, 0)
    }

    fn rendered_body(&self) -> String {
        let total = i128::from(self.input_tokens)
            + i128::from(self.output_tokens)
            + i128::from(self.cache_read_input_tokens)
            + i128::from(self.cache_write_input_tokens);
        format!(
            "entries={} input={} output={} cache_read={} cache_write={} reasoning={} total={total}",
            self.entries,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_input_tokens,
            self.cache_write_input_tokens,
            self.reasoning_output_tokens,
        )
    }
}

impl Component {
    /// A component whose body reached the store.
    pub fn stored(name: impl Into<String>, logical_bytes: Option<usize>) -> Self {
        Self {
            name: name.into(),
            body: ComponentBody::Stored { logical_bytes },
        }
    }

    /// A component carried forward by reference only.
    pub fn unchanged_ref(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            body: ComponentBody::UnchangedRef,
        }
    }

    fn rendered_body(&self) -> String {
        match self.body {
            ComponentBody::Stored {
                logical_bytes: Some(bytes),
            } => format!("stored logical={}", format_bytes(bytes)),
            ComponentBody::Stored {
                logical_bytes: None,
            } => "stored logical=unknown".to_string(),
            ComponentBody::UnchangedRef => "ref (unchanged)".to_string(),
        }
    }
}

/// One transcript line.
///
/// Renders as a single line, plus the mandatory typed [`Usage`] line and one
/// indented line per [`Component`] for a [`Kind::Commit`] entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    actor: Actor,
    kind: Kind,
    event: String,
    attrs: Vec<Attr>,
    usage: Option<Usage>,
    components: Vec<Component>,
}

impl Entry {
    /// A line of any kind. `event` is a stable dotted name such as
    /// `process.start`; it is part of the reviewed text, so it should read the
    /// same way across harnesses.
    pub fn new(kind: Kind, actor: Actor, event: impl Into<String>) -> Self {
        Self {
            actor,
            kind,
            event: event.into(),
            attrs: Vec::new(),
            usage: None,
            components: Vec::new(),
        }
    }

    /// A durable checkpoint commit and its revision transition.
    ///
    /// Harnesses that observe a commit without a revision (the sans-io protocol
    /// harnesses see a `CheckpointKind` request, not a store commit) build the
    /// line with [`Entry::new`] instead and describe what they actually saw.
    pub fn commit(actor: Actor, revision_before: u64, revision_after: u64, usage: Usage) -> Self {
        Self::new(Kind::Commit, actor, CHECKPOINT_COMMIT_EVENT)
            .attr(Attr::revision(revision_before, revision_after))
            .usage(usage)
    }

    /// Attach a `key=value` attribute. Attribute order is the order recorded.
    pub fn attr(mut self, attr: Attr) -> Self {
        self.attrs.push(attr);
        self
    }

    /// Attach the mandatory typed usage facts for a commit entry.
    pub fn usage(mut self, usage: Usage) -> Self {
        debug_assert_eq!(
            self.kind,
            Kind::Commit,
            "usage belongs to a commit line, not to {}",
            self.kind.as_str()
        );
        self.usage = Some(usage);
        self
    }

    /// Attach a checkpoint component. Only meaningful under [`Kind::Commit`].
    pub fn component(mut self, component: Component) -> Self {
        debug_assert_eq!(
            self.kind,
            Kind::Commit,
            "checkpoint components belong to a commit line, not to {}",
            self.kind.as_str()
        );
        self.components.push(component);
        self
    }

    /// The family this line belongs to.
    pub fn family(&self) -> Family {
        self.kind.family()
    }
}

/// A behavior transcript under construction.
///
/// Harnesses record real facts in the order they were observed and call
/// [`Transcript::render`] once, inside an inline `insta` snapshot.
#[derive(Clone, Debug)]
pub struct Transcript {
    entries: Vec<Entry>,
    pinned: BTreeMap<String, String>,
    review_budget_lines: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            pinned: BTreeMap::new(),
            review_budget_lines: DEFAULT_REVIEW_BUDGET_LINES,
        }
    }
}

impl Transcript {
    /// An empty transcript with the default review budget.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raise (or lower) the rendered-line budget for this transcript.
    pub fn with_review_budget(mut self, lines: usize) -> Self {
        self.review_budget_lines = lines;
        self
    }

    /// Give one raw identifier a semantic alias, so the reviewed text says
    /// `root` or `child/left` instead of `session-002`.
    ///
    /// Prefer this wherever the harness knows the role: a semantic name keeps a
    /// diff local when an unrelated actor appears earlier in the run.
    pub fn pin(&mut self, raw: impl Into<String>, name: impl Into<String>) -> &mut Self {
        self.pinned.insert(raw.into(), name.into());
        self
    }

    /// Record one line.
    pub fn record(&mut self, entry: Entry) -> &mut Self {
        self.entries.push(entry);
        self
    }

    /// Number of recorded entries (not rendered lines).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether anything has been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the reviewable text.
    ///
    /// # Panics
    ///
    /// When the rendering exceeds the review budget, or when nothing was
    /// recorded — an empty transcript is a harness bug that would otherwise
    /// snapshot as a passing empty string.
    pub fn render(&self) -> String {
        assert!(
            !self.entries.is_empty(),
            "behavior transcript recorded no entries; an empty transcript cannot fail"
        );
        let mut aliases = Aliases::new(&self.pinned);
        let mut output = String::new();
        for entry in &self.entries {
            let actor = aliases.alias(entry.actor.kind, &entry.actor.raw);
            let attrs = entry
                .attrs
                .iter()
                .map(|attr| format!("{}={}", attr.key, render_value(&attr.value, &mut aliases)))
                .collect::<Vec<_>>()
                .join(" ");
            let mut line = format!(
                "{actor:<ACTOR_WIDTH$}  {kind:<KIND_WIDTH$}  {event:<EVENT_WIDTH$}",
                kind = entry.kind.as_str(),
                event = entry.event,
            );
            if !attrs.is_empty() {
                line.push_str("  ");
                line.push_str(&attrs);
            }
            writeln!(output, "{}", line.trim_end()).expect("write transcript line");
            if entry.kind == Kind::Commit {
                let usage = entry.usage.as_ref().unwrap_or_else(|| {
                    panic!(
                        "commit entry `{}` omitted mandatory typed usage facts",
                        entry.event
                    )
                });
                writeln!(
                    output,
                    "{}",
                    format!(
                        "{actor:<ACTOR_WIDTH$}  {blank:<KIND_WIDTH$}  {name:<EVENT_WIDTH$}  {body}",
                        blank = "",
                        name = "  usage",
                        body = usage.rendered_body(),
                    )
                    .trim_end()
                )
                .expect("write transcript usage line");
            }
            for component in &entry.components {
                let indented = format!("  {}", component.name);
                writeln!(
                    output,
                    "{}",
                    format!(
                        "{actor:<ACTOR_WIDTH$}  {blank:<KIND_WIDTH$}  {indented:<EVENT_WIDTH$}  {body}",
                        blank = "",
                        body = component.rendered_body(),
                    )
                    .trim_end()
                )
                .expect("write transcript component line");
            }
        }
        let rendered = output.trim_end().to_string();
        let lines = rendered.lines().count();
        assert!(
            lines <= self.review_budget_lines,
            "behavior transcript rendered {lines} lines, over the {budget}-line review budget; \
             narrow the scenario or raise the budget explicitly with `with_review_budget`:\n{rendered}",
            budget = self.review_budget_lines,
        );
        rendered
    }
}

struct Aliases<'pinned> {
    pinned: &'pinned BTreeMap<String, String>,
    assigned: BTreeMap<(IdKind, String), String>,
    next: BTreeMap<IdKind, usize>,
}

impl<'pinned> Aliases<'pinned> {
    fn new(pinned: &'pinned BTreeMap<String, String>) -> Self {
        Self {
            pinned,
            assigned: BTreeMap::new(),
            next: BTreeMap::new(),
        }
    }

    fn alias(&mut self, kind: IdKind, raw: &str) -> String {
        if let Some(name) = self.pinned.get(raw) {
            return name.clone();
        }
        let key = (kind, raw.to_string());
        if let Some(alias) = self.assigned.get(&key) {
            return alias.clone();
        }
        let counter = self.next.entry(kind).or_insert(0);
        *counter += 1;
        let alias = format!("{}-{:03}", kind.prefix(), counter);
        self.assigned.insert(key, alias.clone());
        alias
    }
}

fn render_value(value: &AttrValue, aliases: &mut Aliases<'_>) -> String {
    match value {
        AttrValue::Text(text) => format!("\"{text}\""),
        AttrValue::Token(token) => token.clone(),
        AttrValue::Int(value) => value.to_string(),
        AttrValue::Flag(value) => value.to_string(),
        AttrValue::Bytes(value) => format_bytes(*value),
        AttrValue::Id(kind, raw) => aliases.alias(*kind, raw),
        AttrValue::Json(encoded) => encoded.clone(),
        AttrValue::Revision(before, after) => format!("{before}->{after}"),
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    }
}

/// `AfterWork` -> `after_work`; already-snake input is returned unchanged.
fn snake_case(name: &str) -> String {
    let mut output = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 && !output.ends_with('_') {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push(ch);
        }
    }
    output
}

/// Collapse whitespace, mask identifier-shaped substrings, then truncate.
fn scrub_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&mask_identifiers(&collapsed), TEXT_BUDGET_CHARS)
}

/// Replace UUID- and long-hex-shaped runs so a churning identifier that slipped
/// into free text cannot churn the transcript.
fn mask_identifiers(text: &str) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if let Some(width) = uuid_width(&chars[index..]) {
            output.push_str("<uuid>");
            index += width;
            continue;
        }
        if let Some(width) = long_hex_width(&chars[index..]) {
            output.push_str("<hash>");
            index += width;
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];

fn uuid_width(chars: &[char]) -> Option<usize> {
    let mut offset = 0;
    for (group_index, group_len) in UUID_GROUPS.iter().enumerate() {
        if group_index > 0 {
            if chars.get(offset) != Some(&'-') {
                return None;
            }
            offset += 1;
        }
        for _ in 0..*group_len {
            if !chars.get(offset).is_some_and(|ch| ch.is_ascii_hexdigit()) {
                return None;
            }
            offset += 1;
        }
    }
    if chars.get(offset).is_some_and(char::is_ascii_hexdigit) {
        return None;
    }
    Some(offset)
}

fn long_hex_width(chars: &[char]) -> Option<usize> {
    let mut offset = 0;
    while chars.get(offset).is_some_and(char::is_ascii_hexdigit) {
        offset += 1;
    }
    // 32 hex digits is the shortest digest lash mints (`sha256` prefixes are
    // longer); shorter runs are ordinary data such as an exit code or a size.
    (offset >= 32).then_some(offset)
}

fn truncate(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut truncated = text.chars().take(budget).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_KINDS: &[Kind] = &[
        Kind::Ingress,
        Kind::Park,
        Kind::Resume,
        Kind::Cancel,
        Kind::Worker,
        Kind::Fault,
        Kind::Provider,
        Kind::Tool,
        Kind::Exec,
        Kind::Spawn,
        Kind::Await,
        Kind::Wake,
        Kind::Lease,
        Kind::Observe,
        Kind::Commit,
        Kind::Effect,
        Kind::Outcome,
    ];

    fn session(raw: &str) -> Actor {
        Actor::session(raw)
    }

    #[test]
    fn every_kind_has_a_distinct_token_and_one_family() {
        let mut tokens = std::collections::BTreeSet::new();
        for kind in ALL_KINDS {
            assert!(
                tokens.insert(kind.as_str()),
                "duplicate rendered token for {kind:?}"
            );
            assert!(
                kind.as_str().len() <= KIND_WIDTH,
                "{kind:?} does not fit the kind column"
            );
            let _family: Family = kind.family();
        }
        assert!(
            ALL_KINDS
                .iter()
                .any(|kind| kind.family() == Family::DurableWrite),
        );
    }

    #[test]
    fn identifiers_are_aliased_by_first_mention_within_their_namespace() {
        let mut transcript = Transcript::new();
        transcript
            .record(
                Entry::new(Kind::Spawn, session("s-b"), "process.start").attr(Attr::id(
                    "process",
                    IdKind::Process,
                    "process:engine:sha256:ffff",
                )),
            )
            .record(
                Entry::new(Kind::Spawn, session("s-a"), "process.start").attr(Attr::id(
                    "process",
                    IdKind::Process,
                    "process:engine:sha256:aaaa",
                )),
            )
            .record(
                Entry::new(Kind::Spawn, session("s-b"), "process.start").attr(Attr::id(
                    "process",
                    IdKind::Process,
                    "process:engine:sha256:ffff",
                )),
            );
        let rendered = transcript.render();
        let lines = rendered.lines().collect::<Vec<_>>();
        assert!(lines[0].starts_with("session-001"), "{rendered}");
        assert!(lines[1].starts_with("session-002"), "{rendered}");
        assert!(lines[2].starts_with("session-001"), "{rendered}");
        assert!(lines[0].ends_with("process=process-001"), "{rendered}");
        assert!(lines[1].ends_with("process=process-002"), "{rendered}");
        assert!(lines[2].ends_with("process=process-001"), "{rendered}");
    }

    #[test]
    fn pinned_names_replace_aliases_for_every_namespace() {
        let mut transcript = Transcript::new();
        transcript.pin("raw-session", "root");
        transcript.pin("raw-process", "child/left");
        transcript.record(
            Entry::new(Kind::Await, session("raw-session"), "process.await").attr(Attr::id(
                "process",
                IdKind::Process,
                "raw-process",
            )),
        );
        let rendered = transcript.render();
        assert!(rendered.starts_with("root "), "{rendered}");
        assert!(rendered.ends_with("process=child/left"), "{rendered}");
    }

    #[test]
    fn free_text_masks_churning_identifiers_and_collapses_whitespace() {
        let mut transcript = Transcript::new();
        transcript.record(
            Entry::new(Kind::Outcome, session("s"), "turn.failed").attr(Attr::text(
                "error",
                "failed for\n  6f6b8e0e-3d02-4d0e-b0a6-2d1d5a0f9c11 at \
             9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0",
            )),
        );
        let rendered = transcript.render();
        assert!(rendered.contains("<uuid>"), "{rendered}");
        assert!(rendered.contains("<hash>"), "{rendered}");
        assert!(!rendered.contains("6f6b8e0e"), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
    }

    #[test]
    fn debug_variant_names_become_one_token_normal_form() {
        #[derive(Debug)]
        #[allow(dead_code)]
        enum Sample {
            AfterWork,
            Failure,
        }
        let mut transcript = Transcript::new();
        transcript
            .record(
                Entry::new(Kind::Commit, session("s"), "checkpoint.request")
                    .attr(Attr::debug_token("checkpoint", &Sample::AfterWork))
                    .usage(Usage::none()),
            )
            .record(
                Entry::new(Kind::Tool, session("s"), "tool.result")
                    .attr(Attr::debug_token("outcome", &Sample::Failure)),
            );
        let rendered = transcript.render();
        assert!(rendered.contains("checkpoint=after_work"), "{rendered}");
        assert!(rendered.contains("outcome=failure"), "{rendered}");
    }

    #[test]
    fn short_hex_runs_are_left_alone() {
        assert_eq!(
            mask_identifiers("exit=0 code=deadbeef"),
            "exit=0 code=deadbeef"
        );
    }

    #[test]
    fn text_and_json_values_are_truncated_to_their_budgets() {
        let long = "x".repeat(TEXT_BUDGET_CHARS * 2);
        let mut transcript = Transcript::new();
        transcript
            .record(
                Entry::new(Kind::Exec, session("s"), "cell.output").attr(Attr::text("out", &long)),
            )
            .record(
                Entry::new(Kind::Outcome, session("s"), "turn.final_value")
                    .attr(Attr::json("value", &serde_json::json!({ "blob": long }))),
            );
        let rendered = transcript.render();
        for line in rendered.lines() {
            assert!(line.contains('…'), "value was not truncated: {line}");
            assert!(
                line.chars().count() < 160,
                "line too wide to review: {line}"
            );
        }
    }

    #[test]
    fn durable_commit_lines_carry_the_tokens_the_pr_rule_keys_on() {
        let mut transcript = Transcript::new();
        transcript.record(
            Entry::commit(session("s"), 1, 2, Usage::new(1, 11, 7, 3, 2, 4))
                .component(Component::stored("turn_state", Some(412)))
                .component(Component::unchanged_ref("tool_state")),
        );
        let rendered = transcript.render();
        insta::assert_snapshot!(rendered, @r###"
        session-001  commit    checkpoint.commit       rev=1->2
        session-001              usage                 entries=1 input=11 output=7 cache_read=3 cache_write=2 reasoning=4 total=23
        session-001              turn_state            stored logical=412B
        session-001              tool_state            ref (unchanged)
        "###);
        assert!(rendered.contains("rev=1->2"), "{rendered}");
        assert!(rendered.contains("stored logical=412B"), "{rendered}");
        assert!(rendered.contains("ref (unchanged)"), "{rendered}");
        assert!(
            rendered.contains(
                "usage                 entries=1 input=11 output=7 cache_read=3 cache_write=2 reasoning=4 total=23"
            ),
            "{rendered}"
        );
        assert!(
            DURABLE_WRITE_EVENTS.contains(&CHECKPOINT_COMMIT_EVENT),
            "the commit event must stay in the durable-marker list"
        );
    }

    #[test]
    fn rendering_is_reproducible_for_the_same_recorded_facts() {
        let build = || {
            let mut transcript = Transcript::new();
            transcript
                .record(Entry::new(Kind::Ingress, session("s"), "turn.start"))
                .record(Entry::commit(session("s"), 0, 1, Usage::none()));
            transcript.render()
        };
        assert_eq!(build(), build());
    }

    #[test]
    #[should_panic(expected = "omitted mandatory typed usage facts")]
    fn a_commit_without_usage_cannot_render() {
        let mut transcript = Transcript::new();
        transcript.record(Entry::new(Kind::Commit, session("s"), "checkpoint.request"));
        transcript.render();
    }

    #[test]
    #[should_panic(expected = "over the 2-line review budget")]
    fn rendering_past_the_review_budget_is_a_failure_not_a_long_snapshot() {
        let mut transcript = Transcript::new().with_review_budget(2);
        for _ in 0..3 {
            transcript.record(Entry::new(Kind::Ingress, session("s"), "turn.start"));
        }
        transcript.render();
    }

    #[test]
    #[should_panic(expected = "recorded no entries")]
    fn an_empty_transcript_cannot_be_rendered() {
        Transcript::new().render();
    }
}
