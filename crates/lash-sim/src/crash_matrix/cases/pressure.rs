//! The context-pressure seam (FIG-4110, FIG-5355): the host sends a session
//! behind the facade two inputs. The first turn's model call overflows, so
//! the turn stops on it and records a pending recovery; preparing the
//! second turn summarizes the history in a call admitted under
//! `completion.start` and opens the recovery frame the summary seeds, as
//! the session's own head commit under `pressure.frame`, on the session
//! actor's fenced transaction. The second turn runs in that frame
//! ([`crate::crash_matrix::compactions`]).
//!
//! Laws:
//! - Settlement: the first turn ended, and the second answered.
//! - Once: exactly one `pressure.frame` and one `completion.start` commit
//!   land, however the run is cut: a redrive after the frame opened
//!   prepares over the head it moved and opens nothing.
//! - WIRE: every send of the summary carries the one body its admission
//!   stored.
//! - In the frame: every model call of the second turn carries the summary
//!   and not the first input, which the frame it left holds.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core_store::store::RunTerminalKind;
use lash_durable::{ActorState, CommitLabel};
use lash_durable_test::{Cut, SimNodes};
use lash_sansio::{SessionId, TurnId};

use super::{run_of, session_actor, state};
use crate::crash_matrix::compactions::{RECOVERED, SENT};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::services::{TurnScript, turn_id};
use crate::crash_matrix::world::World;

/// The run that takes `session`'s second input.
fn recovered_run(session: &SessionId) -> TurnId {
    turn_id(&format!("{session}-recovered"))
}

#[derive(Default)]
pub struct PressureCase {
    tag: String,
    session: Mutex<Option<SessionId>>,
}

impl PressureCase {
    /// The case with its session named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            session: Mutex::default(),
        }
    }

    fn session(&self) -> Option<SessionId> {
        self.session.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for PressureCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let session = TurnScript::Pressure.session(&format!("pressure{}", self.tag));
        let core = crate::crash_matrix::compactions::compaction_core(world)?;
        crate::crash_matrix::compactions::send_pressure(
            &core,
            &session,
            &run_of(&session),
            &recovered_run(&session),
        )
        .await?;
        world.track(session_actor(&session)?);
        *self.session.lock_recover() = Some(session);
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        let Some(session) = self.session() else {
            return false;
        };
        let Ok(actor) = session_actor(&session) else {
            return false;
        };
        matches!(
            nodes
                .database()
                .turn_end(&session, &recovered_run(&session))
                .await,
            Ok(Some(_))
        ) && state(nodes, &actor).await == Some(ActorState::Idle)
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Some(session) = self.session() else {
            return vec!["the pressure session was never seeded".to_owned()];
        };
        let mut violations = Vec::new();
        let database = nodes.database();
        match database.turn_end(&session, &run_of(&session)).await {
            Ok(Some(_)) => {}
            other => violations.push(format!("the overflowing turn ended {other:?}")),
        }
        match database.turn_end(&session, &recovered_run(&session)).await {
            Ok(Some(end)) if end.kind() == RunTerminalKind::Answered => {}
            other => violations.push(format!("the recovered turn ended {other:?}")),
        }
        let trace = nodes.script().trace();
        for label in [CommitLabel::PRESSURE_FRAME, CommitLabel::COMPLETION_START] {
            let landed = trace
                .iter()
                .filter(|write| write.point.label == label && write.committed())
                .count();
            if landed != 1 {
                violations.push(format!("once: {landed} {label} commits landed, not one"));
            }
        }
        let notes = world.notes();
        let of = |prefix: &str| {
            let prefix = format!("{prefix} {session} :: ");
            notes
                .iter()
                .filter_map(|note| note.strip_prefix(&prefix).map(str::to_owned))
                .collect::<Vec<_>>()
        };
        let mut bodies = of(SENT);
        bodies.sort();
        bodies.dedup();
        if bodies.len() != 1 {
            violations.push(format!(
                "WIRE: the summary was sent with {} bodies: {bodies:?}",
                bodies.len()
            ));
        }
        let recovered = of(RECOVERED);
        if recovered.is_empty() {
            violations.push("the model never saw the second input".to_owned());
        }
        if let Some(request) = recovered
            .iter()
            .find(|request| request.as_str() != "summary=true ask=false")
        {
            violations.push(format!(
                "in the frame: a call of the recovered turn carried {request}"
            ));
        }
        violations
    }
}
