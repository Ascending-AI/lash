//! One lashlang run's command ordinals (FIG-3586).
//!
//! A lashlang run — a code cell, or a process body — runs its program. Every
//! command that leaves the VM through `ExecutionHost::perform` and reaches
//! the effect host — a resource operation, a whole aggregate, a sleep, an
//! await of a handle, a signal wait — takes the next **issue ordinal** of the
//! run, and every journal row the command writes lives under that ordinal's
//! key. Nothing a compiler produces reaches a key.
//!
//! A run starts fresh: the recorded-frontier read a replayed run made went
//! with Restate's journal (FIG-5190); L6 and L7b rebuild resume on
//! snapshots.

/// version_surface = "coexist"
/// version_guard(items(LASH_LASHLANG_CELL_GENERATION_DOMAIN_VERSION, lashlang_cell_generation))
const LASH_LASHLANG_CELL_GENERATION_DOMAIN_VERSION: &str = "lash-lashlang-cell-generation/v1";

/// version_surface = "coexist"
/// version_guard(items(LASHLANG_PREFIX_VERSION, process))
const LASHLANG_PREFIX_VERSION: &str = "lashlang:v2:";

/// version_surface = "coexist"
/// version_guard(items(LASHLANG_DISPATCHED_ORDINALS_DOMAIN_VERSION, hash))
const LASHLANG_DISPATCHED_ORDINALS_DOMAIN_VERSION: &str = "lashlang-dispatched-ordinals/v1";

use std::sync::Mutex;

use lash_core::{
    CommandReplayKey, RecordedKeyRange, RuntimeEffectControllerError, RuntimeErrorCode,
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
///
/// version_guard(
///     roots(path = "crates/lash-core-execution/src/runtime/causal.rs", CommandReplayKey),
/// )
/// version_surface = "drain"
/// format_outside_manifest = "a key grammar, not a payload: the grammar a journal was written under rides the execution-environment sync outcome and the ProcessStarted record, and a run under any other grammar is refused before it issues a command"
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
/// accounting would meet its peeks at other positions. v5 moves with
/// instruction accounting v2 (FIG-3707): a captured, assigned binding compiles
/// to binding-cell intrinsics, so a v4 journal's checkpoints sit at other
/// positions than the v5 re-execution's. v6 moves with instruction
/// accounting 3, which charges every intrinsic whose work grows with its
/// input or output (FIG-3672 P2b): a v5 journal replayed under it would meet
/// its peeks at other positions. A cell's iteration sync
/// stamps this version, and a cell whose sync names another is refused before
/// it runs. Process bodies journal no binding set and stay on the key grammar.
///
/// version_guard(
///     roots(path = "crates/lash-core-execution/src/session.rs", ToolDispatchSurface),
///     items(
///         path = "crates/lash-lashlang-runtime/src/cell_bindings.rs", CELL_TOOL_BINDINGS_SUFFIX,
///         CELL_TOOL_BINDINGS_OPERATION, resolve_ambient_bindings, compare,
///     ),
///     items(path = "crates/lash-core-execution/src/session.rs", tool_dispatch_surface),
/// )
/// version_surface = "drain"
/// format_outside_manifest = "a journal grammar, not a payload: the grammar a cell's journal was written under rides the execution-environment sync outcome, and a cell under any other grammar is refused before it runs"
pub const LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION: u32 = 6;

/// The executable generation code cells run under (FIG-3571): what a turn's
/// admission records, and what a redrive must match before its first effect.
///
/// Its preimage is every contract that decides how a cell compiles and what
/// its journal holds: the semantic-hash and bytecode generations the cell
/// compiles under, the instruction accounting its cancel checkpoints are
/// placed by, and the cell journal grammar its nested effects and binding set
/// are keyed by. A change to any of them moves the generation, so a turn a
/// previous build admitted parks rather than replaying a journal this build
/// would read differently.
#[expect(
    clippy::expect_used,
    reason = "the preimage is a tuple of string and integer constants serialized straight to in-memory bytes"
)]
pub fn lashlang_cell_generation() -> lash_core::ExecutableGeneration {
    let preimage = serde_json::to_vec(&(
        lashlang::LASHLANG_SEMANTIC_HASH_VERSION,
        lashlang::BYTECODE_FORMAT_VERSION,
        lashlang::INSTRUCTION_ACCOUNTING_VERSION,
        LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION,
    ))
    .expect("the cell generation preimage should serialize");
    lash_core::ExecutableGeneration::new(format!(
        "blake3:{}",
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_LASHLANG_CELL_GENERATION_DOMAIN_VERSION,
            preimage,
        )
    ))
}

/// The instruction accounting the cell journal grammar was written against.
/// The accounting is part of the grammar: moving it moves this grammar with
/// it, so the pin is a constant of its own that the grammar's owner moves by
/// hand, and that the release reset sets with every other counter.
///
/// version_surface = "drain"
/// version_guard(unshaped = "a pin on lashlang::INSTRUCTION_ACCOUNTING_VERSION, which guards the accounting's own shapes")
const CELL_GRAMMAR_INSTRUCTION_ACCOUNTING_VERSION: u32 = 3;

const _: () = assert!(
    lashlang::INSTRUCTION_ACCOUNTING_VERSION == CELL_GRAMMAR_INSTRUCTION_ACCOUNTING_VERSION,
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
            prefix: format!("{LASHLANG_PREFIX_VERSION}{opener_scope}:{NAMESPACE_MARKER}"),
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
}

/// What one command writes to its journal: the shape of the rows at its
/// ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandShape {
    /// A tool call: `{command}:{call_id}:attempt:{n}` and its sub-rows, its deferred
    /// `{command}:{call_id}:await`, and a declared start's rows — the start
    /// (`{command}:process:start:{key}`) and its armed terminal
    /// (`{command}:process:subscribe-terminal:{id}:{key}`).
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

/// A running hash of the ordinals a run dispatched, in dispatch order.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DispatchedOrdinalsDigest(String);

impl DispatchedOrdinalsDigest {
    fn hash(preimage: &[u8]) -> Self {
        Self(lash_sansio::core_support::blake3_domain_hash_hex(
            LASHLANG_DISPATCHED_ORDINALS_DOMAIN_VERSION,
            preimage,
        ))
    }

    /// The digest of a run that has dispatched nothing yet.
    pub fn empty() -> Self {
        Self::hash(&[])
    }

    fn extend(&self, ordinal: u64) -> Self {
        let mut preimage = Vec::with_capacity(self.0.len() + 8);
        preimage.extend_from_slice(self.0.as_bytes());
        preimage.extend_from_slice(&ordinal.to_be_bytes());
        Self::hash(&preimage)
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

/// One run's ordinal mint and dispatch record.
#[derive(Debug)]
pub struct LashlangReplayRun {
    namespace: LashlangReplayNamespace,
    state: Mutex<RunState>,
}

#[derive(Debug)]
struct RunState {
    ordinals: LashlangRunOrdinals,
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
            state: Mutex::new(RunState { ordinals }),
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

    /// Closes `command`: `wrote` says whether it wrote the journal. A written
    /// command joins the run's dispatched digest.
    pub fn finish(&self, command: &IssuedCommand, wrote: bool) {
        if wrote {
            let mut state = self.state.lock_recover();
            state.ordinals.dispatched = state.ordinals.dispatched.extend(command.ordinal);
        }
    }

    /// Returns `command` to the mint: its host left it open for the segment
    /// that resumes the run, which issues it again under the same ordinal and
    /// key. Only the run's last issued command can be handed over, and it
    /// joins no dispatched digest here: the segment that completes it records
    /// it.
    pub fn hand_over(&self, command: &IssuedCommand) -> Result<(), ReplayDivergence> {
        let mut state = self.state.lock_recover();
        if state.ordinals.next != command.ordinal + 1 {
            return Err(ReplayDivergence::at(
                &self.namespace,
                Some(command.ordinal),
                format!(
                    "this command was handed over after the run issued ordinal {}",
                    state.ordinals.next.saturating_sub(1)
                ),
            ));
        }
        state.ordinals.next = command.ordinal;
        Ok(())
    }

    /// The seal this run writes as its last nested effect.
    pub fn seal(&self) -> RunSeal {
        let state = self.state.lock_recover();
        RunSeal {
            key: self.namespace.seal(),
            issued_count: state.ordinals.next,
            dispatched_ordinals_digest: state.ordinals.dispatched.clone(),
        }
    }

    /// Who is running the journal's run, for a refusal's message.
    pub fn attribution(&self, current: impl Into<String>) -> SealAttribution {
        SealAttribution {
            recorded: None,
            current: current.into(),
        }
    }
}

/// A run's seal: the count of commands it issued and the digest of those it
/// dispatched, at the namespace's closing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSeal {
    pub key: String,
    pub issued_count: u64,
    pub dispatched_ordinals_digest: DispatchedOrdinalsDigest,
}

impl lash_core::store::DurableRecord for LashlangReplayNamespace {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION);
}

impl lash_core::store::DurableRecord for CommandShape {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION);
}

impl lash_core::store::DurableRecord for DispatchedOrdinalsDigest {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION);
}
