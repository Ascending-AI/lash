//! One re-executed lashlang run's command ordinals and its recorded frontier
//! (FIG-3586).
//!
//! A lashlang run — a code cell, or a process body — replays by running its
//! program again (ADR 0103). Every command that leaves the VM through
//! `ExecutionHost::perform` and reaches the effect host — a resource
//! operation, a whole aggregate, a sleep, an await of a handle, a signal wait
//! — takes the next **issue ordinal** of the run, and every journal row the
//! command writes lives under that ordinal's key. Nothing a compiler produces
//! reaches a key: a redrive on a build whose lowering differs finds each
//! command where it left it, as long as it issues the same commands in the
//! same order.
//!
//! The ordinal alone would let a changed program walk *past* its recorded
//! prefix and dispatch live. The **recorded frontier** closes that: before
//! the run's first command reaches the host, the run reads its whole key
//! namespace once, and from then on no command is dispatched live while the
//! journal still holds an entry at or beyond it. A command whose recorded
//! entry has another shape (a scalar call replayed as an aggregate) refuses at
//! its ordinal, and a completed run's seal refuses a run that ends early. Every
//! refusal is [`RuntimeErrorCode::LashlangCellReplayDivergence`] with zero
//! dispatch; the run stops, and its turn parks.
//!
//! Engines that replay their journal by position and name-check each entry
//! (Restate) answer the frontier read as positional: their own check is the
//! fence, and this run only mints the keys and journals the seal.

use std::collections::BTreeMap;
use std::sync::Mutex;

use lash_core::{
    CommandReplayKey, RecordedJournal, RecordedKeyRange, RecordedKeys,
    RuntimeEffectControllerError, RuntimeErrorCode,
};
use lash_sansio::sync::MutexExt;

/// The replay-key grammar every lashlang run mints (FIG-3586).
///
/// v2 keys a nested effect by its issue ordinal within the run's namespace
/// (`{namespace}:lk2:{ordinal:010}`); v1 keyed it by the AST node id and
/// occurrence of its call site. A journal written under v1 cannot be read by
/// a v2 run: a cell refuses it through the missing grammar stamp on its
/// iteration's execution-environment sync, a process body through its start
/// record's stamp. Changing any spelling in this module — the namespace
/// marker, the ordinal width, a sub-key, the seal — is a grammar change.
pub const LASHLANG_REPLAY_KEY_GRAMMAR_VERSION: u32 = 2;

/// The journal grammar a code cell writes (FIG-3587): the replay-key grammar
/// of [`LASHLANG_REPLAY_KEY_GRAMMAR_VERSION`] plus the cell's ambient binding
/// set, journaled before its first effect and linked against on redrive.
///
/// v3 journals the binding set; v2 did not, so a redrive of a v2 cell would
/// link against the live registry. v4 journals a turn-cancellation gate peek
/// at each cancel checkpoint the cell's VM reaches (FIG-3672 P9), placed by
/// lashlang's instruction accounting ([`lashlang::INSTRUCTION_ACCOUNTING_VERSION`]):
/// v3 journals hold no such peeks, and a v4 journal replayed under other
/// accounting would meet its peeks at other positions. A cell's iteration sync
/// stamps this version, and a cell whose sync names another is refused before
/// it runs. Process bodies journal no binding set and stay on the key grammar.
pub const LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION: u32 = 4;

// The instruction accounting is part of the cell journal grammar: moving it
// moves this grammar with it.
const _: () = assert!(
    lashlang::INSTRUCTION_ACCOUNTING_VERSION == 1,
    "an instruction-accounting change is a cell journal grammar change: bump \
     LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION and update this pin"
);

/// The namespace marker that follows a run's base key.
const NAMESPACE_MARKER: &str = "lk2";

/// The seal's key within a namespace: `~` sorts after every ordinal digit in
/// byte order, so the seal closes the namespace's key range.
const SEAL_SUFFIX: &str = "~seal";

/// The width every ordinal is zero-padded to, so byte order is ordinal order.
const ORDINAL_WIDTH: usize = 10;

/// The largest ordinal the fixed width can spell.
const MAX_ORDINAL: u64 = 9_999_999_999;

/// One run's key namespace: every key the run's commands write starts with
/// `{base}:lk2:`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangReplayNamespace {
    prefix: String,
}

impl LashlangReplayNamespace {
    /// The namespace of one code cell: under its own replay key inside the
    /// turn, which already separates two cells of one turn.
    pub fn cell(execution_replay_key: &str) -> Self {
        Self {
            prefix: format!("{execution_replay_key}:{NAMESPACE_MARKER}"),
        }
    }

    /// The namespace of one process body: under the incarnation's canonical
    /// opener encoding, for the whole life of the incarnation.
    pub fn process(opener_scope: &str) -> Self {
        Self {
            prefix: format!("lashlang:v2:{opener_scope}:{NAMESPACE_MARKER}"),
        }
    }

    /// The command key of `ordinal`.
    pub fn command(&self, ordinal: u64) -> CommandReplayKey {
        CommandReplayKey::new(format!(
            "{}:{ordinal:0width$}",
            self.prefix,
            width = ORDINAL_WIDTH
        ))
    }

    /// The run's seal key: the namespace's last key in byte order.
    pub fn seal(&self) -> String {
        format!("{}:{SEAL_SUFFIX}", self.prefix)
    }

    /// The closed byte range holding every key of the namespace, seal
    /// included.
    pub fn range(&self) -> RecordedKeyRange {
        RecordedKeyRange {
            lower: format!("{}:", self.prefix),
            upper: self.seal(),
            group_key_prefix: String::new(),
        }
    }

    /// The namespace's own prefix, for diagnostics.
    pub fn as_str(&self) -> &str {
        &self.prefix
    }

    /// Splits a recorded key of this namespace into its ordinal and the
    /// sub-key after it (`""` for the command's own row). `None` for a key
    /// outside the namespace or one that does not parse as an ordinal key —
    /// the seal among them.
    fn split<'k>(&self, key: &'k str) -> Option<(u64, &'k str)> {
        let rest = key.strip_prefix(&self.prefix)?.strip_prefix(':')?;
        let digits = rest.get(..ORDINAL_WIDTH)?;
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let ordinal = digits.parse().ok()?;
        match &rest[ORDINAL_WIDTH..] {
            "" => Some((ordinal, "")),
            tail => tail.strip_prefix(':').map(|sub| (ordinal, sub)),
        }
    }
}

/// What one command writes to its journal, as a frontier read tells it
/// apart: the shape of the rows at its ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandShape {
    /// A tool call: `{command}:attempt:{n}` and its sub-rows, or an
    /// orchestrating tool's nested rows — a process it started
    /// (`{command}:process:start:{id}`) and awaited
    /// (`{command}:process:await:{id}`).
    ToolCall,
    /// A journaled value at the command's own key: a runtime value
    /// (`Date.now()`, `Math.random()`) or a trigger operation.
    Value,
    /// A sleep: `{command}:sleep`.
    Sleep,
    /// An aggregate: a group at the command's key, its children under it.
    Aggregate,
    /// An await of a process handle: `{command}:process:await:{id}`.
    AwaitHandle,
    /// A process signal wait: `{command}:signal`.
    SignalWait,
    /// A command that journals nothing in the effect journal: a process event
    /// append, or an ability this host refuses before dispatch. It holds its
    /// ordinal so every later command keeps its key.
    Silent,
}

impl CommandShape {
    fn of_sub_key(sub: &str) -> Option<Self> {
        match sub {
            "" => Some(Self::Value),
            "sleep" => Some(Self::Sleep),
            "signal" => Some(Self::SignalWait),
            "await" => Some(Self::ToolCall),
            "timers-admitted" => Some(Self::Aggregate),
            _ if sub.starts_with("attempt:") => Some(Self::ToolCall),
            _ if sub.starts_with("process:attach-terminal:") => Some(Self::ToolCall),
            _ if sub.starts_with("process:start:") => Some(Self::ToolCall),
            _ if sub.starts_with("child:") => Some(Self::Aggregate),
            _ if sub.starts_with("process:await:") => Some(Self::AwaitHandle),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ToolCall => "tool call",
            Self::Value => "journaled value",
            Self::Sleep => "sleep",
            Self::Aggregate => "aggregate",
            Self::AwaitHandle => "handle await",
            Self::SignalWait => "signal wait",
            Self::Silent => "unjournaled command",
        }
    }
}

/// What the journal holds at one ordinal.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedCommand {
    Shape(CommandShape),
    /// Rows of more than one shape, or a sub-key no command writes: nothing
    /// may be admitted at this ordinal.
    Unreadable(String),
}

/// A run's journal as the frontier read found it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecordedRun {
    commands: BTreeMap<u64, RecordedCommand>,
    /// Keys under the namespace that name no ordinal: the run cannot tell
    /// where they sit, so it treats them as recorded beyond every command.
    foreign: Vec<String>,
    sealed: bool,
    /// The recorded seal's outcome, served for attribution only.
    seal_outcome: Option<String>,
    /// Every replay key the journal holds in the namespace: a replayed
    /// command's writes must land on these while entries lie beyond it.
    keys: std::sync::Arc<std::collections::BTreeSet<String>>,
}

impl RecordedRun {
    fn read(namespace: &LashlangReplayNamespace, keys: RecordedKeys) -> Self {
        // The seal row holds its journaled outcome; the producer is its value.
        let seal_outcome = keys.closing_outcome.map(|outcome| {
            serde_json::from_str::<serde_json::Value>(&outcome)
                .ok()
                .and_then(|outcome| outcome.get("value").cloned())
                .map_or(outcome, |producer| producer.to_string())
        });
        let mut run = Self {
            seal_outcome,
            ..Self::default()
        };
        run.keys = std::sync::Arc::new(keys.replay_keys.iter().cloned().collect());
        let seal = namespace.seal();
        let mut shapes: BTreeMap<u64, Vec<(String, Option<CommandShape>)>> = BTreeMap::new();
        for key in keys.replay_keys {
            if key == seal {
                run.sealed = true;
                continue;
            }
            match namespace.split(&key) {
                Some((ordinal, sub)) => {
                    let shape = CommandShape::of_sub_key(sub);
                    shapes.entry(ordinal).or_default().push((key, shape));
                }
                None => run.foreign.push(key),
            }
        }
        for key in keys.group_keys {
            match namespace.split(&key) {
                Some((ordinal, "")) => shapes
                    .entry(ordinal)
                    .or_default()
                    .push((key, Some(CommandShape::Aggregate))),
                _ => run.foreign.push(key),
            }
        }
        for (ordinal, rows) in shapes {
            let mut shape = None;
            let mut unreadable = None;
            for (key, row_shape) in rows {
                match (row_shape, shape) {
                    (None, _) => {
                        unreadable = Some(format!("`{key}` is not a key any command writes"));
                    }
                    (Some(row), None) => shape = Some(row),
                    (Some(row), Some(seen)) if row == seen => {}
                    // An orchestrating tool call awaits the process it
                    // started under its own ordinal: its await rows are the
                    // call's, not a separate handle await.
                    (Some(CommandShape::ToolCall), Some(CommandShape::AwaitHandle)) => {
                        shape = Some(CommandShape::ToolCall);
                    }
                    (Some(CommandShape::AwaitHandle), Some(CommandShape::ToolCall)) => {}
                    (Some(row), Some(seen)) => {
                        unreadable = Some(format!(
                            "rows of a {} and a {} share it (`{key}`)",
                            seen.label(),
                            row.label()
                        ));
                    }
                }
            }
            let recorded = match (unreadable, shape) {
                (Some(reason), _) => RecordedCommand::Unreadable(reason),
                (None, Some(shape)) => RecordedCommand::Shape(shape),
                (None, None) => continue,
            };
            run.commands.insert(ordinal, recorded);
        }
        run
    }

    /// The first recorded entry at or beyond `ordinal`, when there is one.
    fn first_at_or_beyond(&self, ordinal: u64) -> Option<String> {
        if let Some((recorded, _)) = self.commands.range(ordinal..).next() {
            return Some(format!("a recorded command at ordinal {recorded}"));
        }
        if let Some(key) = self.foreign.first() {
            return Some(format!("the unordered recorded key `{key}`"));
        }
        self.sealed.then(|| "the run's seal".to_string())
    }
}

/// How this run's host answered the frontier read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Frontier {
    /// Not read yet: the run has not dispatched a command.
    Unread,
    /// The journal's rows, by ordinal.
    Recorded(RecordedRun),
    /// The host checks its journal by position as it replays.
    Positional,
}

/// Lowercase hex SHA-256, byte-identical to `lash_core_ids::stable_hash::sha256_hex`.
/// `lash_core::stable_hash` is exported only under `testing`, so a production
/// build cannot reach it.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// A running hash of the ordinals a run dispatched, in dispatch order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DispatchedOrdinalsDigest(String);

impl DispatchedOrdinalsDigest {
    /// The digest of a run that has dispatched nothing yet.
    pub fn empty() -> Self {
        Self(sha256_hex(b"lashlang-dispatched-ordinals/v1"))
    }

    fn extend(&self, ordinal: u64) -> Self {
        let mut preimage = Vec::with_capacity(self.0.len() + 8);
        preimage.extend_from_slice(self.0.as_bytes());
        preimage.extend_from_slice(&ordinal.to_be_bytes());
        Self(sha256_hex(&preimage))
    }

    /// The digest's text, as the seal envelope carries it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The part of a run's ordinal state that survives a process segment
/// handover. A cell starts every run from [`Self::start`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LashlangRunOrdinals {
    /// The ordinal the next command takes.
    pub next: u64,
    /// The running digest of the ordinals dispatched so far.
    pub dispatched: DispatchedOrdinalsDigest,
}

impl LashlangRunOrdinals {
    /// A run that has issued nothing.
    pub fn start() -> Self {
        Self {
            next: 0,
            dispatched: DispatchedOrdinalsDigest::empty(),
        }
    }
}

/// One run's ordinal mint, dispatch record and recorded frontier.
#[derive(Debug)]
pub struct LashlangReplayRun {
    namespace: LashlangReplayNamespace,
    state: Mutex<RunState>,
}

#[derive(Debug)]
struct RunState {
    ordinals: LashlangRunOrdinals,
    frontier: Frontier,
}

/// One command's issue: its ordinal and key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCommand {
    pub ordinal: u64,
    pub key: CommandReplayKey,
}

/// A run's refusal to replay, before anything was dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayDivergence {
    namespace: String,
    ordinal: Option<u64>,
    reason: String,
}

impl ReplayDivergence {
    fn at(namespace: &LashlangReplayNamespace, ordinal: Option<u64>, reason: String) -> Self {
        Self {
            namespace: namespace.as_str().to_string(),
            ordinal,
            reason,
        }
    }

    /// The typed refusal, attributed to the recorded seal's producer when
    /// the run has one.
    pub fn into_error(self, attribution: &SealAttribution) -> RuntimeEffectControllerError {
        let at = match self.ordinal {
            Some(ordinal) => format!("at issue ordinal {ordinal}"),
            None => "at its seal".to_string(),
        };
        RuntimeEffectControllerError::new(
            RuntimeErrorCode::LashlangCellReplayDivergence,
            format!(
                "lashlang run `{}` diverged from its journal {at}: {}; {}. Nothing was \
                 dispatched: redeploy the build that wrote the journal, cancel the turn, or \
                 fork it from before this command",
                self.namespace,
                self.reason,
                attribution.describe()
            ),
        )
    }
}

/// Who wrote the journal a run is replaying, as its recorded seal says, and
/// who is replaying it. Served for attribution only; never compared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SealAttribution {
    /// The recorded seal's outcome JSON, or `None` for an unsealed run.
    pub recorded: Option<String>,
    /// This run's producer.
    pub current: String,
}

impl SealAttribution {
    /// The attribution as a refusal's message states it.
    pub fn describe(&self) -> String {
        match &self.recorded {
            Some(recorded) => format!(
                "journal written by {recorded}, replayed by {}",
                self.current
            ),
            None => format!("journal unsealed, replayed by {}", self.current),
        }
    }
}

impl LashlangReplayRun {
    /// A run over `namespace`, continuing from `ordinals` (a fresh run passes
    /// [`LashlangRunOrdinals::start`]).
    pub fn new(namespace: LashlangReplayNamespace, ordinals: LashlangRunOrdinals) -> Self {
        Self {
            namespace,
            state: Mutex::new(RunState {
                ordinals,
                frontier: Frontier::Unread,
            }),
        }
    }

    /// The run's namespace.
    pub fn namespace(&self) -> &LashlangReplayNamespace {
        &self.namespace
    }

    /// The ordinal state to hand over at a segment boundary.
    pub fn ordinals(&self) -> LashlangRunOrdinals {
        self.state.lock_recover().ordinals.clone()
    }

    /// Mints the next command's ordinal and key. Called once for every
    /// command that leaves the VM, before anything else happens to it, so a
    /// refusal or a pre-dispatch failure still holds its ordinal.
    pub fn issue(&self) -> Result<IssuedCommand, RuntimeEffectControllerError> {
        let mut state = self.state.lock_recover();
        let ordinal = state.ordinals.next;
        if ordinal > MAX_ORDINAL {
            return Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::LashlangCellReplayDivergence,
                format!(
                    "lashlang run `{}` issued more than {MAX_ORDINAL} commands; its ordinal no \
                     longer fits the key grammar's fixed width",
                    self.namespace.as_str()
                ),
            ));
        }
        state.ordinals.next = ordinal + 1;
        Ok(IssuedCommand {
            ordinal,
            key: self.namespace.command(ordinal),
        })
    }

    /// Whether the frontier has been read.
    fn frontier_read(&self) -> bool {
        !matches!(self.state.lock_recover().frontier, Frontier::Unread)
    }

    /// Reads the recorded frontier if this run has not yet: the one range
    /// read of the run's namespace.
    pub async fn ensure_frontier(
        &self,
        ctx: &lash_core::RuntimeExecutionContext<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        if self.frontier_read() {
            return Ok(());
        }
        let frontier = match ctx.read_recorded_journal(&self.namespace.range()).await? {
            RecordedJournal::Keys(keys) => {
                Frontier::Recorded(RecordedRun::read(&self.namespace, keys))
            }
            RecordedJournal::Positional => Frontier::Positional,
        };
        let mut state = self.state.lock_recover();
        if matches!(state.frontier, Frontier::Unread) {
            state.frontier = frontier;
        }
        Ok(())
    }

    /// Decides how `command`, about to reach the host as a `shape`, may write
    /// the journal. The frontier must have been read.
    ///
    /// * The journal holds `command`'s ordinal as another shape, or as rows no
    ///   one command writes: refused here, before anything reaches the host.
    /// * The journal holds it as this shape: [`CommandAdmission::Replay`] —
    ///   its writes pass, and it must make one.
    /// * The journal holds nothing at it but still holds entries beyond it:
    ///   [`CommandAdmission::RefuseWrites`] — the command may run, but its
    ///   first journal write is refused, because the recorded run did not
    ///   write here and nothing may be dispatched live inside it.
    /// * The journal holds nothing at or beyond it, or checks itself by
    ///   position: [`CommandAdmission::Live`].
    pub fn enter(
        &self,
        command: &IssuedCommand,
        shape: CommandShape,
    ) -> Result<CommandAdmission, ReplayDivergence> {
        let state = self.state.lock_recover();
        let Frontier::Recorded(recorded) = &state.frontier else {
            return Ok(CommandAdmission::Live);
        };
        match recorded.commands.get(&command.ordinal) {
            Some(RecordedCommand::Shape(recorded_shape)) if *recorded_shape == shape => {
                Ok(match recorded.first_at_or_beyond(command.ordinal + 1) {
                    // Entries lie beyond this command: its writes must land
                    // on the keys the journal holds, or something that did
                    // not happen here would be dispatched live inside the
                    // recorded run.
                    Some(beyond) => CommandAdmission::ReplayRecordedKeys {
                        keys: std::sync::Arc::clone(&recorded.keys),
                        divergence: ReplayDivergence::at(
                            &self.namespace,
                            Some(command.ordinal),
                            format!(
                                "this {} wrote an entry the journal does not hold here, and \
                                 the journal still holds {beyond}",
                                shape.label()
                            ),
                        ),
                    },
                    None => CommandAdmission::Replay,
                })
            }
            Some(RecordedCommand::Shape(recorded_shape)) => Err(ReplayDivergence::at(
                &self.namespace,
                Some(command.ordinal),
                format!(
                    "the journal recorded a {} here and this run issued a {}",
                    recorded_shape.label(),
                    shape.label()
                ),
            )),
            Some(RecordedCommand::Unreadable(reason)) => Err(ReplayDivergence::at(
                &self.namespace,
                Some(command.ordinal),
                format!("the journal's rows here cannot be read as one command: {reason}"),
            )),
            None => Ok(match recorded.first_at_or_beyond(command.ordinal) {
                Some(beyond) => CommandAdmission::RefuseWrites(ReplayDivergence::at(
                    &self.namespace,
                    Some(command.ordinal),
                    format!(
                        "the journal recorded nothing here but still holds {beyond}, so this {} \
                         would be dispatched live inside the recorded run",
                        shape.label()
                    ),
                )),
                None => CommandAdmission::Live,
            }),
        }
    }

    /// Whether the run's host replays its journal by position, answering no
    /// frontier read: which commands it holds is known only as the replay
    /// reaches them (Restate).
    pub fn is_positional(&self) -> bool {
        matches!(self.state.lock_recover().frontier, Frontier::Positional)
    }

    /// Closes `command`: `wrote` says whether it wrote the journal. A written
    /// command joins the run's dispatched digest. A command the journal holds
    /// rows for that wrote nothing — it now fails before reaching the host, or
    /// settles without dispatching — refuses here, at its ordinal: the
    /// recorded run dispatched it, and this one answered it some other way.
    pub fn finish(&self, command: &IssuedCommand, wrote: bool) -> Result<(), ReplayDivergence> {
        let mut state = self.state.lock_recover();
        if wrote {
            state.ordinals.dispatched = state.ordinals.dispatched.extend(command.ordinal);
            return Ok(());
        }
        if let Frontier::Recorded(recorded) = &state.frontier
            && let Some(RecordedCommand::Shape(shape)) = recorded.commands.get(&command.ordinal)
        {
            return Err(ReplayDivergence::at(
                &self.namespace,
                Some(command.ordinal),
                format!(
                    "the journal recorded a {} here that this run did not dispatch",
                    shape.label()
                ),
            ));
        }
        Ok(())
    }

    /// The seal this run writes as its last nested effect, after checking
    /// that no recorded command lies beyond the commands it issued.
    pub fn seal(&self) -> Result<RunSeal, ReplayDivergence> {
        let state = self.state.lock_recover();
        let issued_count = state.ordinals.next;
        if let Frontier::Recorded(recorded) = &state.frontier
            && let Some((ordinal, _)) = recorded.commands.range(issued_count..).next()
        {
            return Err(ReplayDivergence::at(
                &self.namespace,
                None,
                format!(
                    "the run ended after {issued_count} commands but the journal recorded a \
                     command at ordinal {ordinal}"
                ),
            ));
        }
        Ok(RunSeal {
            key: self.namespace.seal(),
            issued_count,
            dispatched_ordinals_digest: state.ordinals.dispatched.clone(),
        })
    }

    /// Who wrote the journal, for a refusal's message.
    pub fn attribution(&self, current: impl Into<String>) -> SealAttribution {
        let state = self.state.lock_recover();
        SealAttribution {
            recorded: match &state.frontier {
                Frontier::Recorded(recorded) => recorded.seal_outcome.clone(),
                Frontier::Unread | Frontier::Positional => None,
            },
            current: current.into(),
        }
    }
}

/// How one command may write the journal: see [`LashlangReplayRun::enter`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandAdmission {
    /// The journal holds this command and nothing beyond it; its writes
    /// replay it, and any it did not record are live.
    Replay,
    /// The journal holds this command and entries beyond it: its writes
    /// replay it, and a write to a key the journal does not hold refuses
    /// with `divergence`.
    ReplayRecordedKeys {
        keys: std::sync::Arc<std::collections::BTreeSet<String>>,
        divergence: ReplayDivergence,
    },
    /// Nothing is recorded at or beyond this command; its writes are live.
    Live,
    /// Nothing is recorded here but entries are recorded beyond: the first
    /// write is refused with this divergence.
    RefuseWrites(ReplayDivergence),
}

/// A run's seal: the count of commands it issued and the digest of those it
/// dispatched, at the namespace's closing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSeal {
    pub key: String,
    pub issued_count: u64,
    pub dispatched_ordinals_digest: DispatchedOrdinalsDigest,
}

#[cfg(test)]
#[path = "replay_run_tests.rs"]
mod tests;
