//! The park step: a root whose runs fail until the engine stops retrying
//! it, the park the recovery pass records, and the host's redrive of it.

use std::num::NonZeroUsize;
use std::time::Duration;

use lash_core::SessionId;

use super::{Admission, Driver};
use crate::crash_matrix::deployment::{ArmEffect, HostSite};
use crate::invariants::{HostOp, HostRefusalCode, RedriveRefusal};

/// The retry timers and recovery ticks a park step waits, at most, for its
/// root's run to stop and the recovery pass to park it: the turn handler's
/// eight attempts, and the ticks between them.
const PARK_WAITS: usize = 48;

/// What a park step did: the root's admission, and the park its session
/// took with how the host heard its redrive answered.
#[derive(Clone, Debug)]
pub struct Redriven {
    pub admission: Admission,
    /// The root the session parked, and the redrive's outcome; `None` when
    /// nothing parked.
    pub park: Option<(lash_core::TurnId, Admission)>,
}

impl Driver {
    /// Send `root` on `session` while every run of a root of that session
    /// fails live, wait until the engine stops retrying one and the recovery
    /// pass parks it, let the deployment run roots again, and redrive the
    /// park. The fault enters at the session driver's trait seam
    /// ([`HostSite::RunRootBefore`]); the redrive is the host's own verb.
    pub(in crate::chaos_soak) async fn park_and_redrive(
        &mut self,
        session: &SessionId,
        root: &str,
    ) -> Result<Redriven, String> {
        let matching = format!("{session}/");
        self.world.faults().always_matching(
            HostSite::RunRootBefore,
            ArmEffect::FailRetryable,
            matching.clone(),
        );
        self.phase = format!("park `{root}`: sending");
        let parked = match self.send(session, vec![root.to_owned()]).await {
            Ok(admission) if matches!(admission, Admission::Refused { .. }) => {
                Ok((admission, None))
            }
            Ok(admission) => self.await_park(session).await.map(|park| (admission, park)),
            Err(error) => Err(error),
        };
        // The deployment is fixed before the redrive, as an operator fixes
        // one: the resumed root runs.
        self.world
            .faults()
            .disarm_matching(HostSite::RunRootBefore, &matching);
        let (admission, park) = parked?;
        let Some(park) = park else {
            return Ok(Redriven {
                admission,
                park: None,
            });
        };
        self.phase = format!("park `{root}`: redriving `{}`", park.turn_id);
        let redrive = self.redrive(session, &park).await?;
        Ok(Redriven {
            admission,
            park: Some((park.turn_id, redrive)),
        })
    }

    /// Wait until `session` holds a park no redrive names: move time to each
    /// retry of the failing run, and tick recovery once none is left to wait
    /// for, since the recovery pass is what records the park.
    async fn await_park(
        &mut self,
        session: &SessionId,
    ) -> Result<Option<lash_core::store::TurnPark>, String> {
        for wait in 0..PARK_WAITS {
            self.settle_crash().await?;
            let parks = self
                .world
                .backend()
                .session_store_factory()
                .list_turn_parks(&lash_core::store::TurnParkQuery {
                    reasons: None,
                    session: Some(session.clone()),
                    parked_at_or_before_ms: None,
                    after: None,
                    limit: NonZeroUsize::MIN,
                })
                .await
                .map_err(|error| format!("list the parks of `{session}`: {error}"))?;
            if let Some(park) = parks.into_iter().find(|park| park.resume_intent.is_none()) {
                return Ok(Some(park));
            }
            let retrying = self
                .world
                .double()?
                .server()
                .timers()
                .iter()
                .any(|timer| timer.kind == "retry");
            if retrying && wait % 4 != 3 {
                self.advance_retry()?;
                self.world.quiesce().await;
            } else {
                self.tick().await?;
            }
        }
        Ok(None)
    }

    /// Redrive `park` until the host hears an answer: a host that died
    /// inside the redrive asks again when it comes back, and a refusal that
    /// follows a lost answer leaves the redrive `Maybe`, since the lost
    /// attempt may be what the refusal names.
    async fn redrive(
        &mut self,
        session: &SessionId,
        park: &lash_core::store::TurnPark,
    ) -> Result<Admission, String> {
        let target = lash::ParkedWorkRef::Turn {
            session_id: session.clone(),
            turn_id: park.turn_id.clone(),
        };
        let roots = vec![park.turn_id.to_string()];
        let park_id = park.park_id;
        let mut unanswered = 0;
        for _ in 0..20 {
            let core = self.world.core()?;
            let target = target.clone();
            let answered = self
                .host(async move { core.parked_work().redrive(&target, park_id).await })
                .await?;
            let outcome = match answered {
                (Some(Ok(_)), _) => Admission::Known,
                (None, _) => {
                    unanswered += 1;
                    continue;
                }
                (Some(Err(lash::ParkVerbRefused::Store(error))), _) if error.is_transient() => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                (Some(Err(_)), _) if unanswered > 0 => Admission::Maybe,
                (Some(Err(error)), _) => Admission::Refused {
                    code: redrive_refusal(&error)?,
                },
            };
            self.record_host(HostOp::Redrive, session, roots, outcome.clone());
            return Ok(outcome);
        }
        self.record_host(HostOp::Redrive, session, roots, Admission::Maybe);
        Err(format!(
            "the redrive of `{}` never answered: the host died inside {unanswered} attempt(s)",
            park.turn_id
        ))
    }

    /// One real park and redrive gives the redrive checker facts: a root of
    /// a session of its own parks, is redriven, and commits.
    pub(in crate::chaos_soak) async fn park_witness(&mut self) -> Result<(), String> {
        const ROOT: &str = "park-witness";
        let id = SessionId::from(format!("soak-{:016x}-park-witness", self.world.seed()));
        self.world
            .core()?
            .session(id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                crate::crash_matrix::cases::process::MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await
            .map_err(|error| error.to_string())?;
        let redriven = self.park_and_redrive(&id, ROOT).await?;
        if !matches!(
            &redriven,
            Redriven {
                admission: Admission::Known,
                park: Some((_, Admission::Known)),
            }
        ) {
            return Err(format!(
                "the park witness was not parked and redriven: {redriven:?}"
            ));
        }
        let expected = crate::crash_matrix::invariants::Expected {
            inputs: vec![crate::crash_matrix::invariants::AcceptedInput {
                session: id,
                root: lash_core::TurnId::from(ROOT),
            }],
            ..Default::default()
        };
        for _ in 0..6 {
            self.world.quiesce().await;
            if crate::crash_matrix::invariants::check(&self.world, &expected)
                .await
                .is_empty()
            {
                return Ok(());
            }
            self.tick().await?;
        }
        Err("the redriven park witness did not finish".to_owned())
    }
}

fn redrive_refusal(error: &lash::ParkVerbRefused) -> Result<HostRefusalCode, String> {
    Ok(match error {
        lash::ParkVerbRefused::NotParked => HostRefusalCode::Redrive(RedriveRefusal::NotParked),
        lash::ParkVerbRefused::ParkSuperseded { .. } => {
            HostRefusalCode::Redrive(RedriveRefusal::ParkSuperseded)
        }
        lash::ParkVerbRefused::Redriving { .. } => {
            HostRefusalCode::Redrive(RedriveRefusal::Redriving)
        }
        lash::ParkVerbRefused::IntentOpen { .. } => {
            HostRefusalCode::Redrive(RedriveRefusal::IntentOpen)
        }
        lash::ParkVerbRefused::SessionDeleted => {
            HostRefusalCode::Redrive(RedriveRefusal::SessionDeleted)
        }
        lash::ParkVerbRefused::SessionClosing => {
            HostRefusalCode::Redrive(RedriveRefusal::SessionClosing)
        }
        lash::ParkVerbRefused::Store(error) => HostRefusalCode::Runtime(error.runtime_code()),
        lash::ParkVerbRefused::SubstrateRefused { code, .. } => {
            HostRefusalCode::Runtime(code.clone())
        }
        _ => return Err(format!("unclassified redrive refusal: {error:?}")),
    })
}
