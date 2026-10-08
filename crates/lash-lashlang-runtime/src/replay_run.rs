//! One lashlang run's command keys (FIG-3586).
//!
//! A code cell runs its program. Every command that leaves the VM through
//! `ExecutionHost::perform` and reaches the effect host — a resource
//! operation, a whole aggregate, a sleep, an await of a handle — is named
//! by the **issue ordinal** its broker admitted it under, and every journal
//! row the command writes lives under that ordinal's key. The broker's
//! admission is the only ordinal authority (ADR 0132 §8): the run keeps no
//! counter of its own, so a cell restored onto an operation that settled,
//! was interrupted or runs again names its next command by the next
//! admission. Nothing a compiler produces reaches a key.

/// version_surface = "coexist"
/// version_guard(items(LASH_LASHLANG_CELL_GENERATION_DOMAIN_VERSION, lashlang_cell_generation))
const LASH_LASHLANG_CELL_GENERATION_DOMAIN_VERSION: &str = "lash-lashlang-cell-generation/v1";

use lash_core::{
    CommandReplayKey, RecordedKeyRange, RuntimeEffectControllerError, RuntimeErrorCode,
};

/// The replay-key grammar every lashlang run mints (FIG-3586).
///
/// v2 keys a nested effect by its issue ordinal within the run's namespace
/// (`{namespace}:lk2:{ordinal:010}`); v1 keyed it by the AST node id and
/// occurrence of its call site. A journal written under v1 cannot be read by
/// a v2 run: a cell refuses it through the missing grammar stamp on its
/// iteration's execution-environment sync. Changing any spelling in this module — the namespace
/// marker, the ordinal width, a sub-key, the seal — is a grammar change.
///
/// version_guard(
///     roots(path = "crates/lash-core-execution/src/runtime/causal.rs", CommandReplayKey),
/// )
/// version_surface = "drain"
/// format_outside_manifest = "a key grammar, not a payload: the grammar a journal was written under rides the execution-environment sync outcome, and a run under any other grammar is refused before it issues a command"
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
/// it runs.
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
    /// (`Date.now()`, `Math.random()`) or another host effect.
    Value,
    /// A sleep: `{command}:sleep`.
    Sleep,
    /// An aggregate: a group at the command's key, its children under it.
    Aggregate,
    /// An await of a process handle: `{command}:process:await:{id}`.
    AwaitHandle,
}

/// One run's command keys, under its namespace.
#[derive(Debug)]
pub struct LashlangReplayRun {
    namespace: LashlangReplayNamespace,
}

/// One command's issue: its ordinal and key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCommand {
    pub ordinal: u64,
    pub key: CommandReplayKey,
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
    /// A run over `namespace`.
    pub fn new(namespace: LashlangReplayNamespace) -> Self {
        Self { namespace }
    }

    /// The run's namespace.
    pub fn namespace(&self) -> &LashlangReplayNamespace {
        &self.namespace
    }

    /// The command its broker admitted at `ordinal`, with its key. Called
    /// once for every command that leaves the VM, before anything else
    /// happens to it, so a refusal or a pre-dispatch failure still holds its
    /// ordinal.
    pub fn issue(&self, ordinal: u64) -> Result<IssuedCommand, RuntimeEffectControllerError> {
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
        Ok(IssuedCommand {
            ordinal,
            key: self.namespace.command(ordinal),
        })
    }

    /// Who is running the journal's run, for a refusal's message.
    pub fn attribution(&self, current: impl Into<String>) -> SealAttribution {
        SealAttribution {
            recorded: None,
            current: current.into(),
        }
    }
}

impl lash_core::store::DurableRecord for LashlangReplayNamespace {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION);
}

impl lash_core::store::DurableRecord for CommandShape {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::replay_run::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION);
}
