//! One owner transaction's lock plan, read off its whole write (FIG-7700),
//! in the class order [`LockPlan`] states. It covers:
//! - every scope and consumer hold a registration's admission locks;
//! - every execution a terminal ends;
//! - every referrer a package registration, adoption or seed locks;
//! - every frame a head leaves;
//! - every frame a session delete ends;
//! - every environment a registration acquires;
//! - every carry.
//!
//! It does not depend on the order the write names these in, or on whether
//! its head runs. The plan is taken before the transaction's first scope,
//! referrer or artifact lock ([`crate::artifact_store::lock_plan_tx`]), and
//! each admission, end, carry and acquisition after it takes locks it
//! already holds.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use lash_store_sql::Dialect;

use lash_core_store::artifact_referrer::{ArtifactReferrer, FrameEnvironmentId};
use lash_core_store::scope_identity::ScopeId;
use lash_core_store::session_identity::FrameNodeId;
use lash_core_store::store::FrameTransition;
use lash_durable::ActorKey;
use lash_durable::DomainWrite;
use lash_durable::domain::{ProcessWrite, SessionCloseStep, SessionCloseWrite};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::EffectJournalIdentity;

use crate::artifact_store::{LockPlan, ScopeLock};

lash_store_sql::statements! {
    /// The lock plan's statement, which reads stored execution referrers.
    struct LockPlanStatements @ "durable_lock_plan_postgres" {
        /// The plan of `lash_artifact_lock_plan`'s fourteen arguments, with
        /// the stored referrer of each execution of `?15` (a seed's) added
        /// to the held referrers `?7`/`?8`. An execution's referrer never
        /// changes once registered, so it is read before its lock.
        plan = "SELECT lash_artifact_lock_plan(?1::TEXT[], ?2::TEXT[], ?3::TEXT[], ?4::TEXT[], ?5::TEXT[], ?6::TEXT[],
             ?7::TEXT[] || ARRAY(SELECT e.referrer_kind FROM package_executions e
                 WHERE e.execution = ANY(?15::TEXT[]) ORDER BY e.execution),
             ?8::TEXT[] || ARRAY(SELECT e.referrer_id FROM package_executions e
                 WHERE e.execution = ANY(?15::TEXT[]) ORDER BY e.execution),
             ?9::TEXT[], ?10::TEXT[], ?11::TEXT[], ?12::TEXT[], ?13::TEXT[], ?14::TEXT[])";
    }
}

static SQL: LazyLock<LockPlanStatements> =
    LazyLock::new(|| LockPlanStatements::render(Dialect::postgres()));

/// The plan statement.
pub(super) fn plan_statement() -> &'static lash_store_sql::Rendered {
    &SQL.plan
}

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
    // Admission's order: closing scopes by ledger key, then holds.
    let mut scopes: BTreeMap<(String, String), ScopeLock> = BTreeMap::new();
    let mut holds: BTreeSet<String> = BTreeSet::new();
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
                // The head step takes its session's write authority, a
                // class before the plan.
                plan.writers.push(session.clone());
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
                for scope in staged.registration.closing_scopes() {
                    scopes.insert(
                        (scope.storage_kind().to_owned(), scope.storage_id()),
                        crate::process_registry::parent_end::scope_lock(&scope),
                    );
                }
                if let Some(hold) = &staged.registration.consumer_hold {
                    holds.insert(crate::process_registry::parent_end::hold_lock_key(
                        &hold.key,
                    ));
                }
            }
            DomainWrite::Process(ProcessWrite::Terminal { process, .. }) => {
                let scope = ScopeId::process(process.clone());
                scopes.insert(
                    (scope.storage_kind().to_owned(), scope.storage_id()),
                    crate::process_registry::parent_end::scope_lock(&scope),
                );
            }
            DomainWrite::Process(ProcessWrite::ScopeClosed { scope }) => {
                if let Some(scope) = lash_execution_contracts::scope::scope_id(scope) {
                    scopes.insert(
                        (scope.storage_kind().to_owned(), scope.storage_id()),
                        crate::process_registry::parent_end::scope_lock(&scope),
                    );
                }
            }
            DomainWrite::RegisterExecution(record) => plan.held.push(record.referrer.clone()),
            DomainWrite::AdoptExecution(successor) => plan.held.push(successor.referrer.clone()),
            DomainWrite::SeedResolution(record) => {
                plan.executions.push(record.key.execution.0.clone());
            }
            _ => {}
        }
    }
    plan.scopes = scopes
        .into_values()
        .chain(holds.into_iter().map(ScopeLock::Key))
        .collect();
    (plan, transitions)
}

/// The sessions whose write authority the transaction takes: those `plan`
/// reads a leaf frame under and those a later step writes, in the order
/// every session writer takes them: by actor key. The caller takes each
/// before the plan, the class before it.
pub(super) fn session_writes(
    plan: &LockPlan,
) -> Result<Vec<(ActorKey, SessionId)>, lash_core_store::store::StoreError> {
    let mut sessions = plan
        .sessions
        .iter()
        .chain(&plan.writers)
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
