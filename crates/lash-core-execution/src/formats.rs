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
}

impl BuildFormats {
    /// The sets of a build with `engines`, whose actors also hold `extra`.
    pub(crate) fn new(engines: &[EngineStateFormat], extra: &[FormatSurface]) -> Self {
        let session = FormatSet::of(
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
        );
        let processes = engines
            .iter()
            .map(|format| {
                let set = FormatSet::of(
                    ActorKind::Process,
                    tool_state()
                        .into_iter()
                        .chain([FormatSurface::engine(&format.kind, format.version)])
                        .chain(extra.iter().cloned()),
                );
                (format.kind.clone(), set)
            })
            .collect();
        Self { session, processes }
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

    /// Every set a node of this build decodes: the session set, each
    /// engine's unstarted and started sets, and the kernel process set.
    #[must_use]
    pub fn decodes(&self) -> Vec<FormatSet> {
        let mut decodes = vec![self.session.clone(), FormatSet::kernel_process()];
        for (kind, set) in &self.processes {
            decodes.push(FormatSet::unstarted_process(kind));
            decodes.push(set.clone());
        }
        decodes
    }
}
