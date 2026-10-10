//! The format sets of this build's actors (ADR 0106 §1; L11, FIG-5187).
//!
//! A session's state is its turn checkpoint, its session-state generation,
//! its run records, its wait rows and its tool outcomes' materials; a started
//! process's is its engine's state and the same run records, waits and
//! materials. The assembler adds
//! the formats of the crates above this one that actor state also holds
//! (the VM's continuation and snapshot formats, with `rlm`), and
//! [`Backend`](crate::Backend) spells each kind's [`FormatSet`] from them.

use std::collections::BTreeMap;

use lash_durable::{ActorKind, FormatSet, FormatSurface};

use crate::EngineStateFormat;

pub use crate::runtime::actor::round::RUN_RECORD_FORMAT_VERSION;

/// The turn checkpoint's format id.
pub const TURN_CHECKPOINT_FORMAT_ID: &str = "turn-checkpoint";
/// The run-record bodies' format id.
pub const RUN_RECORD_FORMAT_ID: &str = "run-record";
/// The wait rows' format id.
pub const WAIT_ROW_FORMAT_ID: &str = "wait-row";
/// The tool outcome materials' format id.
pub const OUTCOME_MATERIAL_FORMAT_ID: &str = "outcome-material";
/// The session-state generation's id: a session's mutable continuation,
/// admitted under the build's session admission window.
pub const SESSION_STATE_FORMAT_ID: &str = "session-state";

/// The formats every actor that runs tools and waits holds.
fn tool_state() -> [FormatSurface; 3] {
    [
        FormatSurface::new(RUN_RECORD_FORMAT_ID, RUN_RECORD_FORMAT_VERSION),
        FormatSurface::new(
            WAIT_ROW_FORMAT_ID,
            lash_durable::domain::WAIT_ROW_FORMAT_VERSION,
        ),
        FormatSurface::new(
            OUTCOME_MATERIAL_FORMAT_ID,
            u32::from(lash_core_store::tool_run::material::OUTCOME_MATERIAL_FORMAT_VERSION),
        ),
    ]
}

/// The format sets one build writes and decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildFormats {
    session: FormatSet,
    processes: BTreeMap<String, FormatSet>,
    /// The state format of each engine this build has.
    written: BTreeMap<String, EngineStateFormat>,
    /// The sets of earlier builds this build's engines carry forward, by
    /// the engine state format each holds.
    carried: BTreeMap<EngineStateFormat, FormatSet>,
    /// The session sets of those earlier builds: a session one of them
    /// left is claimed, and what it holds in that build's formats is
    /// carried forward where it is restored.
    carried_sessions: Vec<FormatSet>,
    /// The sets of earlier builds this build no longer decodes nor carries
    /// forward (ADR 0115 §3.5): an actor still in one would be stranded, so
    /// a node of this build does not start while there is one.
    retired: Vec<FormatSet>,
    /// The operator command that has a node of the build before this one
    /// carry the actors in a retired set forward.
    sweep: Option<String>,
}

/// The set of a process whose engine state is in `format`, by a build whose
/// actors also hold `extra`.
fn process_set(format: &EngineStateFormat, extra: &[FormatSurface]) -> FormatSet {
    FormatSet::of(
        ActorKind::Process,
        tool_state()
            .into_iter()
            .chain([FormatSurface::engine(&format.kind, format.version)])
            .chain(extra.iter().cloned()),
    )
}

/// The set a session's state is written in by a build whose actors also
/// hold `extra`.
fn session_set(extra: &[FormatSurface]) -> FormatSet {
    FormatSet::of(
        ActorKind::Session,
        tool_state()
            .into_iter()
            .chain([
                FormatSurface::new(
                    TURN_CHECKPOINT_FORMAT_ID,
                    lash_sansio::TURN_CHECKPOINT_SCHEMA_VERSION,
                ),
                FormatSurface::new(
                    SESSION_STATE_FORMAT_ID,
                    lash_core_store::store::CURRENT_SESSION_STATE_VERSION,
                ),
            ])
            .chain(extra.iter().cloned()),
    )
}

impl BuildFormats {
    /// The sets of a build with `engines`, whose actors also hold `extra`.
    #[must_use]
    pub fn new(engines: &[EngineStateFormat], extra: &[FormatSurface]) -> Self {
        let session = session_set(extra);
        let processes = engines
            .iter()
            .map(|format| (format.kind.clone(), process_set(format, extra)))
            .collect();
        Self {
            session,
            processes,
            written: engines
                .iter()
                .map(|format| (format.kind.clone(), format.clone()))
                .collect(),
            carried: BTreeMap::new(),
            carried_sessions: Vec::new(),
            retired: Vec::new(),
            sweep: None,
        }
    }

    /// These sets, and the set of a process whose engine state is in
    /// `format`, an earlier build's: `format` with the tool state and
    /// `extra`, the other formats that build's actors held.
    pub(crate) fn carrying(mut self, format: &EngineStateFormat, extra: &[FormatSurface]) -> Self {
        let set = process_set(format, extra);
        if self.retired.contains(&set) {
            return self;
        }
        self.carried.insert(format.clone(), set);
        let session = session_set(extra);
        if session != self.session && !self.carried_sessions.contains(&session) {
            self.carried_sessions.push(session);
        }
        self
    }

    /// These sets, retiring the sets of an earlier build whose process
    /// engine state is in `format` and whose actors also held `extra`: this
    /// build neither decodes nor carries them forward, and `sweep` is the
    /// operator command that has a node of the build before it carry an
    /// actor still in one forward.
    pub(crate) fn retiring(
        mut self,
        format: &EngineStateFormat,
        extra: &[FormatSurface],
        sweep: &str,
    ) -> Self {
        let process = process_set(format, extra);
        let session = session_set(extra);
        self.carried.retain(|_, set| *set != process);
        self.carried_sessions.retain(|set| *set != session);
        for set in [process, session] {
            if set != self.session
                && !self.processes.values().any(|own| *own == set)
                && !self.retired.contains(&set)
            {
                self.retired.push(set);
            }
        }
        self.sweep = Some(sweep.to_owned());
        self
    }

    /// These sets, still carrying and retiring what `earlier` did.
    pub(crate) fn keeping(mut self, earlier: &Self) -> Self {
        self.carried = earlier.carried.clone();
        self.carried_sessions = earlier.carried_sessions.clone();
        self.retired = earlier.retired.clone();
        self.sweep = earlier.sweep.clone();
        self
    }

    /// The sets of earlier builds this build no longer decodes nor carries
    /// forward: a node of this build does not start while an actor is in
    /// one (ADR 0115 §3.5).
    #[must_use]
    pub fn retired(&self) -> &[FormatSet] {
        &self.retired
    }

    /// The operator command that carries an actor in a
    /// [`retired`](Self::retired) set forward, when this build retires any.
    #[must_use]
    pub fn sweep(&self) -> Option<&str> {
        self.sweep.as_deref()
    }

    /// The set of a process whose engine state is in `format`: this
    /// build's own, or one it carries forward.
    #[must_use]
    pub fn process_in(&self, format: &EngineStateFormat) -> Option<&FormatSet> {
        if self.written.get(&format.kind) == Some(format) {
            return self.processes.get(&format.kind);
        }
        self.carried.get(format)
    }

    /// The set a session's state is written in.
    #[must_use]
    pub fn session(&self) -> &FormatSet {
        &self.session
    }

    /// The set a started process of engine `kind` is written in, when this
    /// build has the engine.
    #[must_use]
    pub fn process(&self, kind: &str) -> Option<&FormatSet> {
        self.processes.get(kind)
    }

    /// The session sets of the earlier builds this build carries forward.
    #[must_use]
    pub fn carried_sessions(&self) -> &[FormatSet] {
        &self.carried_sessions
    }

    /// The sets a node of this build that serves sessions decodes: the
    /// session set, the unstarted one a producer's wake creates, and the
    /// session sets it carries forward.
    #[must_use]
    pub fn session_decodes(&self) -> Vec<FormatSet> {
        let mut decodes = vec![self.session.clone(), FormatSet::unstarted_session()];
        decodes.extend(self.carried_sessions.iter().cloned());
        decodes
    }

    /// Every set a node of this build decodes: the session sets, each
    /// engine's unstarted and started sets, and the kernel process set.
    #[must_use]
    pub fn decodes(&self) -> Vec<FormatSet> {
        let mut decodes = self.session_decodes();
        decodes.push(FormatSet::kernel_process());
        for (kind, set) in &self.processes {
            decodes.push(FormatSet::unstarted_process(kind));
            decodes.push(set.clone());
        }
        decodes.extend(self.carried.values().cloned());
        decodes
    }
}
