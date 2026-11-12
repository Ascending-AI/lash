//! One owner transaction's artifact lock plan, read off its whole write
//! (FIG-7700): every execution a terminal ends, every frame a head leaves,
//! every frame a session delete ends, every environment a registration
//! acquires and every carry, whatever order the write names them in and
//! whether or not its head runs. The plan is taken before the
//! transaction's first artifact lock
//! ([`crate::artifact_store::lock_plan_tx`]); each end, carry and
//! acquisition after it takes locks it already holds.

use std::collections::BTreeMap;

use lash_core_store::artifact_referrer::{ArtifactReferrer, FrameEnvironmentId};
use lash_core_store::session_identity::FrameNodeId;
use lash_core_store::store::FrameTransition;
use lash_durable::ActorKey;
use lash_durable::DomainWrite;
use lash_durable::domain::{ProcessWrite, SessionCloseStep, SessionCloseWrite};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::EffectJournalIdentity;

use crate::artifact_store::LockPlan;

/// The plan of `writes`, committed by `actor`, which end the executions
/// `ending`, with the frame transitions whose carries the caller adds.
/// A head or registration that does not decode adds nothing: its write
/// refuses before it locks anything.
pub(super) fn of_writes(
    actor: &ActorKey,
    writes: &[DomainWrite],
    ending: &[EffectJournalIdentity],
) -> (LockPlan, Vec<FrameTransition>) {
    let mut plan = LockPlan {
        ended: ending
            .iter()
            .cloned()
            .map(ArtifactReferrer::Execution)
            .collect(),
        gates: ending.to_vec(),
        ..LockPlan::default()
    };
    let mut transitions = Vec::new();
    // Each session's leaf frame as the write leaves it so far: `None`
    // while it is the stored head's, which the plan reads.
    let mut leaves: BTreeMap<SessionId, Option<FrameNodeId>> = BTreeMap::new();
    let leaf_ends = |plan: &mut LockPlan,
                     leaves: &mut BTreeMap<SessionId, Option<FrameNodeId>>,
                     session: &SessionId| {
        match leaves.get(session) {
            Some(Some(frame)) => plan.ended.push(frame_referrer(session, frame)),
            Some(None) => {}
            None => {
                plan.sessions.push(session.clone());
                leaves.insert(session.clone(), None);
            }
        }
    };
    for write in writes {
        match write {
            DomainWrite::SessionCommit(head) => {
                let Ok(commit) = lash_core_store::store::decode_session_commit(&head.commit_json)
                else {
                    continue;
                };
                if commit.session_id != head.session {
                    continue;
                }
                let session = &commit.session_id;
                let opened: Vec<FrameNodeId> = commit
                    .graph
                    .nodes()
                    .iter()
                    .filter_map(|node| {
                        node.frame_open()
                            .and_then(|_| FrameNodeId::new(node.node_id.as_str().to_owned()).ok())
                    })
                    .collect();
                // A head that opens a frame leaves the leaf's and each it
                // opens but the last, which the new head holds.
                if let Some((last, before)) = opened.split_last() {
                    leaf_ends(&mut plan, &mut leaves, session);
                    plan.ended
                        .extend(before.iter().map(|frame| frame_referrer(session, frame)));
                    leaves.insert(session.clone(), Some(last.clone()));
                }
                plan.held.push(ArtifactReferrer::Session(session.clone()));
                if let Some(transition) = commit.frame_transition {
                    plan.held.push(ArtifactReferrer::FrameEnvironment(
                        transition.successor.clone(),
                    ));
                    transitions.push(transition);
                }
            }
            DomainWrite::SessionClose(SessionCloseWrite::Step {
                session,
                step: SessionCloseStep::Artifacts,
            }) if ActorKey::session(session.as_str()).is_ok_and(|own| &own == actor) => {
                leaf_ends(&mut plan, &mut leaves, session);
                plan.held.push(ArtifactReferrer::Session(session.clone()));
            }
            DomainWrite::Process(ProcessWrite::Register(rows)) => {
                let Ok(staged) =
                    lash_execution_contracts::process::StagedRegistration::decode(rows)
                else {
                    continue;
                };
                plan.held
                    .push(ArtifactReferrer::ProcessRecord(staged.process_id.clone()));
                if let Some(launch) = staged.registration.input.launch() {
                    plan.envs.push(launch.env.as_str().to_owned());
                }
            }
            _ => {}
        }
    }
    (plan, transitions)
}

/// The sessions whose write authority `plan` reads a leaf frame under, in
/// the order every session writer takes them: by actor key. The caller
/// takes each before the plan.
pub(super) fn session_writes(
    plan: &LockPlan,
) -> Result<Vec<(ActorKey, SessionId)>, lash_core_store::store::StoreError> {
    let mut sessions = plan
        .sessions
        .iter()
        .map(|session| {
            ActorKey::session(session.as_str())
                .map(|actor| (actor, session.clone()))
                .map_err(|error| {
                    lash_core_store::store::StoreError::Backend(format!(
                        "session actor key: {error}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    sessions.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    sessions.dedup_by(|left, right| left.0 == right.0);
    Ok(sessions)
}

fn frame_referrer(session: &SessionId, frame: &FrameNodeId) -> ArtifactReferrer {
    ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(session.clone(), frame.clone()))
}
