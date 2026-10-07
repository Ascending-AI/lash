//! The session-command seam (FIG-5230): a session behind the facade, on the
//! production turn services, holds two host appends and a config transaction
//! on its command lane. Its session actor applies each on its own fenced
//! transaction under `session.command`: the first append adds its node; the
//! second requires an ancestor that was never on the session's path, so the
//! store refuses its append and it settles `StaleBranch` in the commit after;
//! the transaction settles with its resolution.
//!
//! Laws: each command settled once, with its typed outcome; the head moved
//! once per command and by nothing else; exactly one `session.command`
//! commit landed per command; and a zombie owner's `session.command`, held
//! until the session moved to another owner, is refused with
//! `OwnershipLost`: a stale-epoch command commits nothing.

use std::sync::{Arc, Mutex};

use lash_core::facade_support::SessionCommand;
use lash_core::runtime::{DeliveryPolicy, QueuedWorkBatchDraft, SessionCommandOutcome};
use lash_core::sync::MutexExt as _;
use lash_core::{AppendSessionNodesOutcome, AppendSessionNodesRequest, SessionAppendNode};
use lash_durable::{ActorState, CommitLabel, DurableError};
use lash_durable_test::{Cut, Fault, SimNodes, Stored};
use lash_sansio::{BatchId, NodeId, SessionId};

use super::{session_actor, state};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::TurnScript;
use crate::crash_matrix::world::World;

/// How many commands the session holds, each settled by one head commit.
const COMMANDS: u64 = 3;

/// The ancestor the stale append requires: never on any session's path.
const NEVER_ON_THE_PATH: &str = "command-never-on-the-path";

#[derive(Clone, Default)]
struct Seeded {
    session: Option<SessionId>,
    append: Option<BatchId>,
    stale: Option<BatchId>,
    config: Option<BatchId>,
    /// The head revision once the session was created.
    created_head: u64,
}

#[derive(Default)]
pub struct CommandCase {
    tag: String,
    seeded: Mutex<Seeded>,
}

impl CommandCase {
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

/// An append of one plugin node under `operation`, requiring `ancestor` on
/// the session's path when given.
fn append(operation: &str, ancestor: Option<&str>) -> Result<SessionCommand, String> {
    let requires_ancestor_node_id = ancestor
        .map(NodeId::parse)
        .transpose()
        .map_err(|error| error.to_string())?;
    Ok(SessionCommand::AppendSessionNodes {
        request: Box::new(AppendSessionNodesRequest {
            operation_id: operation.to_owned(),
            nodes: vec![SessionAppendNode::plugin(
                "lash-sim.command",
                serde_json::json!({ "operation": operation }),
            )],
            requires_ancestor_node_id,
        }),
    })
}

/// Submit `command` to `session`'s command lane as a host's submission
/// records it; its commit wakes the session's actor.
pub(super) async fn submit(
    world: &World,
    session: &SessionId,
    command: SessionCommand,
    key: &str,
) -> Result<BatchId, String> {
    world
        .backend()?
        .session_store_factory()
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                session.clone(),
                DeliveryPolicy::AfterCurrentTurnCommit,
                command.clone(),
            )
            .with_source_key(command.source_key(key)),
        )
        .await
        .map(|batch| batch.batch_id)
        .map_err(|error| format!("submit the command {key}: {error}"))
}

/// The outcome `batch` of `session` settled with, once it settled.
pub(super) async fn settled(
    world: &World,
    session: &SessionId,
    batch: &BatchId,
) -> Option<SessionCommandOutcome> {
    world
        .backend()
        .ok()?
        .session_store_factory()
        .queued_work_batch_completion(session, batch.as_str())
        .await
        .ok()??
        .command_outcomes
        .get(batch)
        .cloned()
}

async fn head(world: &World, session: &SessionId) -> Result<u64, String> {
    world
        .backend()?
        .session_store_factory()
        .load_session_head_meta(session)
        .await
        .map_err(|error| error.to_string())?
        .map(|head| head.head_revision)
        .ok_or_else(|| format!("session {session} has no head"))
}

#[async_trait::async_trait]
impl Workload for CommandCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        // A cell session runs on the production turn services, whose
        // command lane is the subject.
        let session = TurnScript::Cell.session(&format!("command{}", self.tag));
        let core = crate::crash_matrix::cells::cell_core(world)?;
        crate::crash_matrix::cells::create(&core, &session).await?;
        let created_head = head(world, &session).await?;
        let append_batch =
            submit(world, &session, append("command-append", None)?, "append").await?;
        let stale = submit(
            world,
            &session,
            append("command-stale", Some(NEVER_ON_THE_PATH))?,
            "stale",
        )
        .await?;
        let config = submit(
            world,
            &session,
            SessionCommand::ApplyConfigTransaction {
                transaction: Box::new(lash_core::ConfigTransactionRecord {
                    id: "command-config".to_owned(),
                    expected_revision: 0,
                    entries: Vec::new(),
                }),
            },
            "config",
        )
        .await?;
        world.track(session_actor(&session)?);
        *self.seeded.lock_recover() = Seeded {
            session: Some(session),
            append: Some(append_batch),
            stale: Some(stale),
            config: Some(config),
            created_head,
        };
        Ok(())
    }

    async fn done(&self, world: &World, nodes: &SimNodes) -> bool {
        let seeded = self.seeded();
        let (Some(session), Some(append), Some(stale), Some(config)) =
            (seeded.session, seeded.append, seeded.stale, seeded.config)
        else {
            return false;
        };
        let Ok(actor) = session_actor(&session) else {
            return false;
        };
        settled(world, &session, &append).await.is_some()
            && settled(world, &session, &stale).await.is_some()
            && settled(world, &session, &config).await.is_some()
            && state(nodes, &actor).await == Some(ActorState::Idle)
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let seeded = self.seeded();
        let (Some(session), Some(append), Some(stale), Some(config)) =
            (seeded.session, seeded.append, seeded.stale, seeded.config)
        else {
            return vec!["the commands were never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        match settled(world, &session, &append).await {
            Some(SessionCommandOutcome::AppendSessionNodes {
                outcome: AppendSessionNodesOutcome::Appended { node_ids, .. },
            }) if node_ids.len() == 1 => {}
            other => violations.push(format!("the append settled {other:?}")),
        }
        match settled(world, &session, &stale).await {
            Some(SessionCommandOutcome::AppendSessionNodes {
                outcome: AppendSessionNodesOutcome::StaleBranch { required_node_id },
            }) if required_node_id.as_str() == NEVER_ON_THE_PATH => {}
            other => violations.push(format!("the stale append settled {other:?}")),
        }
        match settled(world, &session, &config).await {
            Some(SessionCommandOutcome::ConfigTransaction { .. }) => {}
            other => violations.push(format!("the config transaction settled {other:?}")),
        }
        match head(world, &session).await {
            Ok(revision) if revision == seeded.created_head + COMMANDS => {}
            other => violations.push(format!(
                "the head is at {other:?}, not {COMMANDS} commits past {}",
                seeded.created_head
            )),
        }
        let trace = nodes.script().trace();
        let commits = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::SESSION_COMMAND && write.committed())
            .count();
        if commits != usize::try_from(COMMANDS).unwrap_or(usize::MAX) {
            violations.push(format!(
                "{commits} session.command commits landed, not one per command"
            ));
        }
        if let Some(cut) = cut
            && cut.fault == Fault::Zombie
            && cut.point.label == CommitLabel::SESSION_COMMAND
        {
            match trace
                .iter()
                .find(|write| write.node == cut.node && write.cut == Some(Fault::Zombie))
            {
                Some(write)
                    if matches!(
                        write.stored,
                        Stored::Refused(DurableError::OwnershipLost(_))
                    ) => {}
                other => violations.push(format!(
                    "the zombie's session.command was not refused as stale: {other:?}"
                )),
            }
        }
        violations
    }
}
