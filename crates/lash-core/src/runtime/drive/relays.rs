//! Every obligation kind's relay a core runs (ADR 0109 §1.4, FIG-3888).
//!
//! A store set arms every [`ObligationKind`], and a kind whose relay no
//! deployment runs leaves its rows owed forever: a scope close never closed,
//! a child never cancelled, an intent held open until its ceiling. The relays
//! are therefore assembled here, one per kind, from the parts a core resolves
//! — never chosen per host — and a core that cannot supply some kind's
//! delivery refuses to build ([`RelaySupply::check`]).

use std::sync::Arc;

use super::relay::ObligationRelay;
use super::{ControlIntentRelay, IngressRelay, ParentEndRelay, ScopeCloseRelay};
use crate::engine::ScopeCloseSink;
use crate::runtime::process_start::ProcessStartRelay;
use crate::runtime::process_terminal::ProcessTerminalRelay;
use crate::runtime::session_delete::SessionDeleteRelay;
use crate::store::ObligationKind;
use crate::{
    Backend, Clock, ProcessWorkWiring, SessionAdministration, SessionStoreFactory,
    SessionWorkEngine,
};

/// What a kind's delivery needs beyond the store set, the session engine and
/// the scope owner every core has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayNeed {
    /// The process registry and the process-work port its processes run on:
    /// a parent-end plan cancels children through it, and a terminal is
    /// published through it.
    ProcessWork,
    /// The session administration a physical delete runs through.
    SessionAdministration,
}

impl RelayNeed {
    /// What `kind`'s relay needs, if anything.
    #[must_use]
    pub const fn of(kind: ObligationKind) -> Option<Self> {
        match kind {
            ObligationKind::Ingress
            | ObligationKind::ControlIntent
            | ObligationKind::ScopeClose => None,
            ObligationKind::ParentEnd
            | ObligationKind::ProcessStart
            | ObligationKind::ProcessTerminal => Some(Self::ProcessWork),
            ObligationKind::SessionDelete => Some(Self::SessionAdministration),
        }
    }

    const fn describe(self) -> &'static str {
        match self {
            Self::ProcessWork => "process-work port",
            Self::SessionAdministration => "session administration",
        }
    }
}

/// A core that cannot run one obligation kind's relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the core cannot run the `{}` obligation relay: it has no {}",
    kind.label(),
    need.describe()
)]
pub struct ObligationRelayUnavailable {
    pub kind: ObligationKind,
    pub need: RelayNeed,
}

/// What a core can deliver obligations through, known when it is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelaySupply {
    pub process_work: bool,
    pub session_administration: bool,
}

impl RelaySupply {
    const fn has(self, need: RelayNeed) -> bool {
        match need {
            RelayNeed::ProcessWork => self.process_work,
            RelayNeed::SessionAdministration => self.session_administration,
        }
    }

    /// Whether every kind's relay can run on this supply.
    ///
    /// # Errors
    ///
    /// The first kind, in [`ObligationKind::ALL`] order, whose delivery this
    /// supply lacks.
    pub fn check(self) -> Result<(), ObligationRelayUnavailable> {
        for kind in ObligationKind::ALL {
            if let Some(need) = RelayNeed::of(kind)
                && !self.has(need)
            {
                return Err(ObligationRelayUnavailable { kind, need });
            }
        }
        Ok(())
    }
}

/// The parts every kind's relay is assembled from: what a core resolved for
/// one reconcile tick.
pub struct RelayParts {
    /// The backend whose store set holds every kind's ledger.
    pub backend: Backend,
    pub sessions: Arc<dyn SessionStoreFactory>,
    /// The session engine drives and control verbs are asked of.
    pub work: Arc<dyn SessionWorkEngine>,
    /// The owner of lifetime scopes a root's close reaches.
    pub scopes: Arc<dyn ScopeCloseSink>,
    /// The process registry and the port its processes run on.
    pub processes: Option<ProcessWorkWiring>,
    /// What a physical delete runs through.
    pub administration: Option<SessionAdministration>,
    pub clock: Arc<dyn Clock>,
}

impl RelayParts {
    /// What these parts can deliver through.
    #[must_use]
    pub fn supply(&self) -> RelaySupply {
        RelaySupply {
            process_work: self.processes.is_some(),
            session_administration: self.administration.is_some(),
        }
    }
}

/// Every kind's relay, one per [`ObligationKind`] in [`ObligationKind::ALL`]
/// order — ingress first: an admitted input's drive is the work every other
/// kind's session waits behind.
///
/// # Errors
///
/// A kind whose delivery `parts` cannot supply.
pub fn obligation_relays(
    parts: RelayParts,
) -> Result<Vec<Arc<dyn ObligationRelay>>, ObligationRelayUnavailable> {
    parts.supply().check()?;
    let RelayParts {
        backend,
        sessions,
        work,
        scopes,
        processes,
        administration,
        clock,
    } = parts;
    let scope_close: Arc<dyn ObligationRelay> = Arc::new(ScopeCloseRelay::over_backend(
        &backend,
        Arc::clone(&sessions),
        Arc::clone(&scopes),
    ));
    let unavailable = |kind: ObligationKind| ObligationRelayUnavailable {
        kind,
        need: RelayNeed::of(kind).unwrap_or(RelayNeed::ProcessWork),
    };
    let mut relays = Vec::with_capacity(ObligationKind::ALL.len());
    for kind in ObligationKind::ALL {
        let relay: Arc<dyn ObligationRelay> = match kind {
            ObligationKind::Ingress => Arc::new(IngressRelay::over_backend(
                &backend,
                Arc::clone(&work),
                Arc::clone(&clock),
            )),
            ObligationKind::ControlIntent => Arc::new(ControlIntentRelay::new(
                backend.obligation_ledger(kind),
                Arc::clone(&sessions),
                Arc::clone(&work),
                Arc::clone(&scopes),
                Arc::clone(&scope_close),
                Arc::clone(&clock),
            )),
            ObligationKind::ScopeClose => Arc::clone(&scope_close),
            ObligationKind::ParentEnd => {
                let wiring = processes.as_ref().ok_or_else(|| unavailable(kind))?;
                Arc::new(ParentEndRelay::new(
                    backend.obligation_ledger(kind),
                    Arc::clone(wiring.registry()),
                    Arc::clone(wiring.port()),
                    Arc::clone(&clock),
                ))
            }
            ObligationKind::SessionDelete => Arc::new(SessionDeleteRelay::new(
                administration.clone().ok_or_else(|| unavailable(kind))?,
            )),
            ObligationKind::ProcessStart => {
                let wiring = processes.as_ref().ok_or_else(|| unavailable(kind))?;
                Arc::new(ProcessStartRelay::new(
                    backend.obligation_ledger(kind),
                    Arc::clone(wiring.registry()),
                    Arc::clone(wiring.port()),
                    Arc::clone(&clock),
                ))
            }
            ObligationKind::ProcessTerminal => {
                let wiring = processes.as_ref().ok_or_else(|| unavailable(kind))?;
                Arc::new(ProcessTerminalRelay::new(
                    backend.obligation_ledger(kind),
                    Arc::clone(wiring.registry()),
                    Arc::clone(wiring.port()),
                ))
            }
        };
        relays.push(relay);
    }
    Ok(relays)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A supply without a process-work port cannot run the parent-end or
    /// process-terminal relay; one without session administration cannot run
    /// the session-delete relay; a full supply runs every kind.
    #[test]
    fn a_supply_missing_a_kinds_delivery_names_that_kind() {
        let full = RelaySupply {
            process_work: true,
            session_administration: true,
        };
        assert_eq!(full.check(), Ok(()));
        assert_eq!(
            RelaySupply {
                process_work: false,
                ..full
            }
            .check(),
            Err(ObligationRelayUnavailable {
                kind: ObligationKind::ParentEnd,
                need: RelayNeed::ProcessWork,
            })
        );
        assert_eq!(
            RelaySupply {
                session_administration: false,
                ..full
            }
            .check(),
            Err(ObligationRelayUnavailable {
                kind: ObligationKind::SessionDelete,
                need: RelayNeed::SessionAdministration,
            })
        );
        for kind in ObligationKind::ALL {
            assert!(
                RelayNeed::of(kind).is_none_or(|need| full.has(need)),
                "{kind:?} runs on a full supply"
            );
        }
    }
}
