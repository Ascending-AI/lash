//! The owned-call seam (ADR 0133 §8, FIG-5259): the host appends a
//! conversation to a session behind the facade, then its `CompactContext`
//! command summarizes it. The summary is a call the command's run owns: it composes
//! the compaction purpose, is lowered to its exact body, and is admitted
//! under `completion.start` before the model sees it
//! ([`crate::crash_matrix::compactions`]).
//!
//! Laws:
//! - Settlement: the append landed, and the compaction opened its frame.
//! - Admission: exactly one logical admission lands. Later
//!   `completion.start` commits reserve increasing attempts of that same
//!   call, without recording its prompt or body again (FIG-6570).
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
use lash_durable::domain::{ModelCallId, ModelCallPinWrite, PromptCallKey, PromptWrite};
use lash_durable::{ActorState, CommitLabel};
use lash_durable_test::{Cut, SimNodes};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::llm::types::{LlmCallId, LlmRequestScope};
use lash_vocabulary::identity::BatchId;

use super::command::{settled, submit};
use super::{session_actor, state};
use crate::crash_matrix::compactions::{ATTEMPT, LOWERED, SENT};
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
    completion: CompletionAdmissions,
}

impl CompactionCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            seeded: Mutex::default(),
            completion: CompletionAdmissions::default(),
        }
    }

    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for CompactionCase {
    async fn seed(&self, world: &Arc<World>, nodes: &Arc<SimNodes>) -> Result<(), String> {
        self.completion.observe(nodes);
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
        if let Err(error) = self.completion.check(world, nodes, &session).await {
            violations.push(error);
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

/// Successful database writes, observed before a cut can hide their receipts.
/// Admission and dispatch reservation share a label, but only admission
/// records a prompt. Keep the evidence across owner death (FIG-6570).
#[derive(Default)]
pub(super) struct CompletionAdmissions {
    commits: Arc<Mutex<Vec<Vec<lash_durable::DomainWrite>>>>,
}

impl CompletionAdmissions {
    pub(super) fn observe(&self, nodes: &SimNodes) {
        let commits = Arc::clone(&self.commits);
        nodes.script().observe_actor_receipts(move |label, tx, _| {
            if label == CommitLabel::COMPLETION_START {
                commits.lock_recover().push(tx.domain().to_vec());
            }
        });
    }

    pub(super) async fn check(
        &self,
        world: &World,
        nodes: &SimNodes,
        session: &SessionId,
    ) -> Result<(), String> {
        use lash_durable::DomainWrite;

        let commits = self.commits.lock_recover().clone();
        let admissions = commits
            .iter()
            .flatten()
            .filter_map(|write| match write {
                DomainWrite::Prompt(PromptWrite::Record { call, request, .. }) => {
                    Some((call, request))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [(key, request)] = admissions.as_slice() else {
            return Err(format!(
                "admission: {} logical completions recorded, not one",
                admissions.len()
            ));
        };
        if key.session != *session || !matches!(key.call, ModelCallId::Owned { .. }) {
            return Err(format!(
                "admission: unexpected logical completion key {key:?}"
            ));
        }
        let reads = nodes.database();
        let retained = reads
            .prompt_snapshot(key)
            .await
            .map_err(|e| e.to_string())?;
        if retained.as_ref().and_then(|row| row.request.as_ref()) != Some(request) {
            return Err("admission: the retained request differs from its committed record".into());
        }
        let admitted = lash_core::plugin::prompt::load_admitted_call(reads.as_ref(), key)
            .await
            .map_err(|e| format!("admission: {e}"))?
            .ok_or("admission: the owned request was not retained")?;
        let call_id = LlmCallId(admitted.scope.request_id.clone());
        Self::check_reservations(&commits, key, &call_id)?;
        Self::check_dispatches(world, session, &admitted, commits.len())?;
        let calls = reads
            .model_calls(session, &key.call.owner_column())
            .await
            .map_err(|e| e.to_string())?;
        let [call] = calls.as_slice() else {
            return Err(format!(
                "settlement: {} logical call records, not one",
                calls.len()
            ));
        };
        if call.call_id != call_id {
            return Err("settlement: the record lost its logical identity".into());
        }
        if call
            .record
            .attempts
            .iter()
            .any(|attempt| attempt.usage.is_none())
            && call
                .usage()
                .map_err(|e| format!("usage: {e:?}"))?
                .unknown_calls
                != 1
        {
            return Err("usage: absent attempt usage was counted as free (FIG-6570)".into());
        }
        Ok(())
    }

    fn check_reservations(
        commits: &[Vec<lash_durable::DomainWrite>],
        key: &PromptCallKey,
        call_id: &LlmCallId,
    ) -> Result<(), String> {
        use lash_durable::DomainWrite;

        for (index, commit) in commits.iter().enumerate() {
            let reservations = commit
                .iter()
                .filter_map(|write| match write {
                    DomainWrite::ModelCallPin(ModelCallPinWrite::Reserve {
                        key,
                        call_id,
                        attempt,
                        ..
                    }) => Some((key, call_id, *attempt)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let ordinal = u32::try_from(index + 1).map_err(|e| e.to_string())?;
            if reservations != [(key, call_id, ordinal)] {
                return Err(format!(
                    "reservation: completion.start {} did not reserve attempt {ordinal} of {key:?}: {reservations:?}",
                    index + 1
                ));
            }
            let records = commit
                .iter()
                .filter(|write| matches!(write, DomainWrite::Prompt(PromptWrite::Record { .. })))
                .count();
            if records != usize::from(index == 0) {
                return Err("admission: only the first reservation may record the request".into());
            }
        }
        Ok(())
    }

    fn check_dispatches(
        world: &World,
        session: &SessionId,
        admitted: &lash_core::plugin::prompt::LoadedAdmittedCall,
        reservations: usize,
    ) -> Result<(), String> {
        let prefix = format!("{ATTEMPT} {session} :: ");
        // A stale owner can reach the provider after the next owner. The
        // committed reservations are ordered; network arrivals need not be.
        let mut sent = std::collections::BTreeSet::new();
        for note in world.notes() {
            let Some(json) = note.strip_prefix(&prefix) else {
                continue;
            };
            let (mut scope, body): (LlmRequestScope, String) = serde_json::from_str(json)
                .map_err(|e| format!("dispatch: unreadable scope and body: {e}"))?;
            let attempt = scope
                .attempt
                .take()
                .ok_or("dispatch: missing attempt ordinal")?;
            scope.llm_call_id = None;
            if scope != admitted.scope || body != admitted.template.redacted() {
                return Err(format!(
                    "WIRE: attempt {attempt} changed the admitted scope or body"
                ));
            }
            if attempt == 0 || attempt as usize > reservations || !sent.insert(attempt) {
                return Err(format!(
                    "dispatch: attempt {attempt} is unreserved or was sent twice"
                ));
            }
        }
        if sent.is_empty() {
            return Err("dispatch: no summary attempt reached the model".into());
        }
        Ok(())
    }
}
