//! The owned-call seam (ADR 0133 §8, FIG-5259): the host appends a
//! conversation to a session behind the facade, then its `CompactContext`
//! command summarizes it. The summary is a call the command's run owns: it composes
//! the compaction purpose, is lowered to its exact body, and is admitted
//! under `completion.start` before the model sees it
//! ([`crate::crash_matrix::compactions`]).
//!
//! Laws:
//! - Settlement: the append landed, and the compaction opened its frame.
//! - Admission: exactly one `completion.start` commit lands, however the
//!   run is cut: a redrive that finds the admission makes no other.
//! - WIRE: every send of the summary carries the one body its admission
//!   stored, so a redrive after the admission sends those bytes.
//! - Lowering: uncut, the summary lowers once; a cut wastes at most one
//!   lowering, made before its admission landed.

use std::sync::{Arc, Mutex};

use lash_core::facade_support::SessionCommand;
use lash_core::runtime::{CompactContextOutcome, SessionCommandOutcome};
use lash_core::sync::MutexExt as _;
use lash_core::{
    AppendSessionNodesOutcome, AppendSessionNodesRequest, MessageRole, Part, PluginMessage,
    SessionAppendNode,
};
use lash_durable::{ActorState, CommitLabel};
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::{BatchId, SessionId};

use super::command::{settled, submit};
use super::{session_actor, state};
use crate::crash_matrix::compactions::{LOWERED, SENT};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::TurnScript;
use crate::crash_matrix::world::World;

#[derive(Clone, Default)]
struct Seeded {
    session: Option<SessionId>,
    append: Option<BatchId>,
    compaction: Option<BatchId>,
}

/// The host's append of a short conversation for the compaction to
/// summarize.
fn conversation() -> SessionCommand {
    let message = |id: &str, role, text: &str| {
        SessionAppendNode::message(PluginMessage {
            id: Some(id.to_owned()),
            role,
            origin: None,
            parts: vec![Part::text(format!("{id}.p0"), text.to_owned(), None)],
        })
    };
    SessionCommand::AppendSessionNodes {
        request: Box::new(AppendSessionNodesRequest {
            operation_id: "compaction-conversation".to_owned(),
            nodes: vec![
                message("compaction-ask", MessageRole::User, "what was decided?"),
                message("compaction-answer", MessageRole::Assistant, "to ship it"),
                // The compaction keeps the latest user message and summarizes
                // what came before it.
                message("compaction-next", MessageRole::User, "and then?"),
            ],
            requires_ancestor_node_id: None,
        }),
    }
}

#[derive(Default)]
pub struct CompactionCase {
    tag: String,
    seeded: Mutex<Seeded>,
}

impl CompactionCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            seeded: Mutex::default(),
        }
    }

    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for CompactionCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = TurnScript::Compaction.session(&format!("compaction{}", self.tag));
        let core = crate::crash_matrix::compactions::compaction_core(world)?;
        crate::crash_matrix::compactions::create(&core, &session).await?;
        let append = submit(world, &session, conversation(), "conversation").await?;
        let compaction = submit(
            world,
            &session,
            SessionCommand::CompactContext { instructions: None },
            "compaction",
        )
        .await?;
        world.track(session_actor(&session)?);
        *self.seeded.lock_recover() = Seeded {
            session: Some(session),
            append: Some(append),
            compaction: Some(compaction),
        };
        Ok(())
    }

    async fn done(&self, world: &World, nodes: &SimNodes) -> bool {
        let Seeded {
            session: Some(session),
            append: Some(append),
            compaction: Some(compaction),
        } = self.seeded()
        else {
            return false;
        };
        let Ok(actor) = session_actor(&session) else {
            return false;
        };
        settled(world, &session, &append).await.is_some()
            && settled(world, &session, &compaction).await.is_some()
            && state(nodes, &actor).await == Some(ActorState::Idle)
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let Seeded {
            session: Some(session),
            append: Some(append),
            compaction: Some(compaction),
        } = self.seeded()
        else {
            return vec!["the compaction was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        match settled(world, &session, &append).await {
            Some(SessionCommandOutcome::AppendSessionNodes {
                outcome: AppendSessionNodesOutcome::Appended { .. },
            }) => {}
            other => violations.push(format!("the conversation's append settled {other:?}")),
        }
        match settled(world, &session, &compaction).await {
            Some(SessionCommandOutcome::CompactContext {
                outcome: CompactContextOutcome::Opened { .. },
            }) => {}
            other => violations.push(format!("the compaction settled {other:?}")),
        }
        let admissions = nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.point.label == CommitLabel::COMPLETION_START && write.committed())
            .count();
        if admissions != 1 {
            violations.push(format!(
                "admission: {admissions} completion.start commits landed, not one"
            ));
        }
        let notes = world.notes();
        let bodies = |prefix: &str| {
            let prefix = format!("{prefix} {session} :: ");
            notes
                .iter()
                .filter_map(|note| note.strip_prefix(&prefix).map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let (lowered, sent) = (bodies(LOWERED), bodies(SENT));
        let mut distinct = sent.clone();
        distinct.sort();
        distinct.dedup();
        if distinct.len() != 1 {
            violations.push(format!(
                "WIRE: the summary was sent with {} bodies: {sent:?}",
                distinct.len()
            ));
        }
        let bounds = if cut.is_none() { 1..=1 } else { 1..=2 };
        if !bounds.contains(&lowered.len()) {
            violations.push(format!(
                "lowering: the summary lowered {} times: {lowered:?}",
                lowered.len()
            ));
        }
        violations
    }
}
