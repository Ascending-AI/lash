use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCleanupFacts {
    pub replay_keys: Vec<String>,
    pub dispatcher: EffectGroupDispatchState,
    #[serde(with = "btree_map_as_pairs")]
    pub dispatched: BTreeMap<usize, String>,
}

impl EffectGroupCleanupFacts {
    /// The declared arity, derived from the retained replay keys — the same
    /// answer [`EffectGroupShape::children`] gives on the live shape.
    pub(crate) fn children(&self) -> usize {
        self.replay_keys.len()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCleanup {
    Pending {
        facts: EffectGroupCleanupFacts,
        /// The live record retirement is still writing into: `retirement_cancel`
        /// seats the canceller-side terminals of undecided children here before
        /// the tombstone. `Complete` is what says retirement holds none — the
        /// variant keeps it, not an `Option` on the index record. Boxed so the
        /// enum stays narrow once retirement is complete.
        live: Box<EffectGroupStateLiveRecord>,
    },
    Complete {
        /// Retirement can pause after cleanup but before its reply. Keep
        /// its owner so that invocation still parks and resumes normally.
        #[serde(with = "lash_core::admitted_scope_wire")]
        opener: lash_core::AdmittedScope,
    },
}

/// The group's phase, shaped enum-per-phase so a phase that has live state
/// always carries it — the model the SQL tiers' `lifecycle` column mirrors.
///
/// `live` folds into `Preparing`/`Ready`/`Closed` rather than sitting beside
/// the lifecycle on the index record: a `Preparing` index with no live state
/// was representable and corrupt, and `Retired` is the only phase whose own
/// fields carry none — the pending cleanup keeps the record it is reducing
/// to a tombstone inside [`EffectGroupCleanup`], out of the variant, so the
/// variant is still what says a finished group has no live state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupLifecycle {
    Preparing {
        dispatch: EffectGroupDispatchState,
        live: EffectGroupStateLiveRecord,
    },
    Ready {
        #[serde(with = "btree_map_as_pairs")]
        addresses: BTreeMap<usize, String>,
        live: EffectGroupStateLiveRecord,
    },
    Closed {
        effective: EffectGroupCloseOutcome,
        /// The durable twin of the SQL entries' cleared `closed` flag
        /// (FIG-3481): a reopen is a new caller interest, so a reopened
        /// closed entry serves the ranks a still-running loser has yet to
        /// seat instead of answering `Closed`. The disposition itself stays
        /// cumulative in `effective`. `#[serde(default)]` because index
        /// records journaled before the flag existed decode as not reopened.
        #[serde(default)]
        reopened: bool,
        #[serde(with = "btree_map_as_pairs")]
        addresses: BTreeMap<usize, String>,
        live: EffectGroupStateLiveRecord,
    },
    Retired {
        cleanup: EffectGroupCleanup,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCloseOutcome {
    RunToCompletion,
    Cancel,
    Refused { reason: EffectGroupRefusal },
}

impl From<LoserPolicy> for EffectGroupCloseOutcome {
    fn from(value: LoserPolicy) -> Self {
        match value {
            LoserPolicy::RunToCompletion => Self::RunToCompletion,
            LoserPolicy::Cancel => Self::Cancel,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRefusal {
    NoExecutor { position: usize },
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupSettlementTerminal {
    StoredPayload,
    Failed { error: RuntimeEffectControllerError },
    Cancelled,
}

/// One seated rank as a read serves it. The rank is the reader's own
/// question, so the record does not repeat it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupSettlementRecord {
    pub position: usize,
    pub terminal: EffectGroupSettlementTerminal,
}

/// What one child holds at the §4 arbitration point, and whether its rank is
/// seated.
///
/// `record_settlement` is the child's final record reaching the point and
/// `close`/`retirement_cancel` are the cancel disposition reaching it, and
/// whichever wrote first holds it. `CancelDecided` is what turns a late
/// `record_settlement` into the typed `CancelDecided` refusal rather than an
/// indistinguishable `Duplicate`.
///
/// - `Committed`: the child's own final won the point and reserved its rank
///   (FIG-4308); the seat is owed until its drain and projection are done.
/// - `Seated`: the committed child published its terminal at that rank.
/// - `CancelDecided`: the cancel disposition won the point, which seats the
///   rank cancelled in the same step.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupSeat {
    Committed,
    Seated {
        terminal: EffectGroupSettlementTerminal,
    },
    CancelDecided,
}

impl EffectGroupSeat {
    /// The terminal a seated rank serves; `None` while the seat is owed.
    fn terminal(&self) -> Option<EffectGroupSettlementTerminal> {
        match self {
            Self::Committed => None,
            Self::Seated { terminal } => Some(terminal.clone()),
            Self::CancelDecided => Some(EffectGroupSettlementTerminal::Cancelled),
        }
    }

    pub(crate) fn is_seated(&self) -> bool {
        !matches!(self, Self::Committed)
    }
}

/// One §4 decision: the child it decided and what that child holds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupDecision {
    pub position: usize,
    pub seat: EffectGroupSeat,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupStateLiveRecord {
    pub(crate) shape: EffectGroupShape,
    /// Every child whose §4 point is taken, by either side, in rank order:
    /// the decision at index `i` holds rank `i + 1`. Rank order is therefore
    /// the order of §4 decisions, and commit order is that order restricted
    /// to committed children. A child absent from it is `pending`.
    pub(crate) decisions: Vec<EffectGroupDecision>,
}

impl EffectGroupStateLiveRecord {
    /// A live record no child has decided.
    pub(crate) fn undecided(shape: EffectGroupShape) -> Self {
        Self {
            shape,
            decisions: Vec::new(),
        }
    }

    /// Every decision with the rank it holds, in rank order.
    pub(crate) fn ranked(&self) -> impl Iterator<Item = (u64, &EffectGroupDecision)> {
        (1..).zip(&self.decisions)
    }

    /// The rank `position` holds and its seat, or `None` while it is pending.
    pub(crate) fn decision(&self, position: usize) -> Option<(u64, &EffectGroupSeat)> {
        self.ranked()
            .find(|(_, decision)| decision.position == position)
            .map(|(rank, decision)| (rank, &decision.seat))
    }

    /// Takes the §4 point for a pending `position` and answers the rank the
    /// decision reserves: the next one.
    pub(crate) fn decide(&mut self, position: usize, seat: EffectGroupSeat) -> u64 {
        self.decisions.push(EffectGroupDecision { position, seat });
        self.decisions.len() as u64
    }

    /// Publishes the seat of a decision at the rank it reserved.
    pub(crate) fn seat(&mut self, rank: u64, seat: EffectGroupSeat) {
        if let Some(decision) = usize::try_from(rank)
            .ok()
            .and_then(|rank| rank.checked_sub(1))
            .and_then(|index| self.decisions.get_mut(index))
        {
            decision.seat = seat;
        }
    }

    /// Whether `position`'s rank is seated, by its own settlement or by a
    /// cancel decision.
    pub(crate) fn is_seated(&self, position: usize) -> bool {
        self.decision(position)
            .is_some_and(|(_, seat)| seat.is_seated())
    }

    /// How many positions are seated.
    pub(crate) fn seated(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.seat.is_seated())
            .count()
    }

    /// The committed children whose seat is still owed, as `(rank, position)`
    /// in rank order.
    pub(crate) fn owed(&self) -> impl Iterator<Item = (u64, usize)> {
        self.ranked()
            .filter(|(_, decision)| !decision.seat.is_seated())
            .map(|(rank, decision)| (rank, decision.position))
    }

    /// The contiguous-seated watermark: the highest rank `r` such that every
    /// rank `1..=r` is seated, or 0. A read is served only at or below it
    /// (FIG-4308).
    pub(crate) fn seated_prefix(&self) -> u64 {
        self.decisions
            .iter()
            .take_while(|decision| decision.seat.is_seated())
            .count() as u64
    }

    /// The settlement `rank` serves once it is seated.
    pub(crate) fn settlement(&self, rank: u64) -> Option<EffectGroupSettlementRecord> {
        let index = usize::try_from(rank).ok()?.checked_sub(1)?;
        let decision = self.decisions.get(index)?;
        Some(EffectGroupSettlementRecord {
            position: decision.position,
            terminal: decision.seat.terminal()?,
        })
    }

    /// Seats every still-pending position cancelled (the cancel disposition
    /// reaching the §4 point) and answers the positions it decided. A
    /// committed child keeps the decision it holds, and an already-cancelled
    /// one its first seat.
    pub(crate) fn decide_pending_cancelled(&mut self) -> Vec<usize> {
        let pending = (0..self.shape.children())
            .filter(|position| self.decision(*position).is_none())
            .collect::<Vec<_>>();
        for position in &pending {
            self.decide(*position, EffectGroupSeat::CancelDecided);
        }
        pending
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct EffectGroupStateRecord {
    pub(crate) shape_digest: String,
    /// The route — the full Restate service name — the group's dispatch was
    /// sent under (FIG-3795 S10), fixed at `open`. Dispatcher self-calls and
    /// host-side group calls address this recorded route; it is never
    /// recomputed from the running build.
    pub(crate) dispatch_route: String,
    pub(crate) lifecycle: EffectGroupLifecycle,
}

impl EffectGroupStateRecord {
    /// The live state a non-`Retired` phase always carries. `Retired` is the
    /// only variant that does not hold it in the phase fields — the pending
    /// cleanup's copy is reached through [`EffectGroupCleanup`], never this
    /// accessor — so it is also the only error.
    pub(crate) fn live(&self) -> Result<&EffectGroupStateLiveRecord, TerminalError> {
        match &self.lifecycle {
            EffectGroupLifecycle::Preparing { live, .. }
            | EffectGroupLifecycle::Ready { live, .. }
            | EffectGroupLifecycle::Closed { live, .. } => Ok(live),
            EffectGroupLifecycle::Retired { .. } => Err(TerminalError::new(format!(
                "effect-group index {} has no live state after retirement",
                self.shape_digest
            ))),
        }
    }

    pub(crate) fn live_mut(&mut self) -> Result<&mut EffectGroupStateLiveRecord, TerminalError> {
        match &mut self.lifecycle {
            EffectGroupLifecycle::Preparing { live, .. }
            | EffectGroupLifecycle::Ready { live, .. }
            | EffectGroupLifecycle::Closed { live, .. } => Ok(live),
            EffectGroupLifecycle::Retired { .. } => Err(TerminalError::new(format!(
                "effect-group index {} has no live state after retirement",
                self.shape_digest
            ))),
        }
    }
}

/// What a group still needs of one paused dispatcher invocation, pure so its
/// arms are exercisable without an object context (FIG-4630).
///
/// - The dispatch (`run`) of a group not retired, and a retirement in any
///   phase, are needed: they park the group's opener.
/// - A child is needed until its position is seated, in every phase. A
///   closed group's opener reads no more ranks, yet a `RunToCompletion` loser
///   it left running still has to settle before the opener's scope is
///   quiescent, and a committed child still owes its drain. A seat a cancel
///   decision took, or the child's own settlement, leaves the invocation
///   nothing to do.
/// - An invocation the index retains no position for is a successor minted
///   to seat a position (§8): needed while any position is unseated.
/// - A retired group needs nothing but its retirement.
pub(crate) fn paused_work_need(
    lifecycle: EffectGroupLifecycle,
    request: &EffectGroupOpenerRequest,
) -> EffectGroupOpenerResponse {
    let (addresses, live) = match lifecycle {
        EffectGroupLifecycle::Retired { cleanup } => {
            return if request.handler == "retire" {
                EffectGroupOpenerResponse::Needed {
                    opener: match cleanup {
                        EffectGroupCleanup::Pending { live, .. } => live.shape.opener,
                        EffectGroupCleanup::Complete { opener } => opener,
                    },
                }
            } else {
                EffectGroupOpenerResponse::Seated
            };
        }
        // A preparing group records no child id yet, and seats none.
        EffectGroupLifecycle::Preparing { live, .. } => (BTreeMap::new(), live),
        EffectGroupLifecycle::Ready { addresses, live }
        | EffectGroupLifecycle::Closed {
            addresses, live, ..
        } => (addresses, live),
    };
    let seated = request.handler == "child"
        && match addresses
            .iter()
            .find(|(_, id)| **id == request.invocation_id)
        {
            Some((position, _)) => live.is_seated(*position),
            None => live.shape.children() > 0 && live.seated() >= live.shape.children(),
        };
    if seated {
        EffectGroupOpenerResponse::Seated
    } else {
        EffectGroupOpenerResponse::Needed {
            opener: live.shape.opener,
        }
    }
}

/// The §8 admission decision, pure so its arms are exercisable without an
/// `ObjectContext`: the index's retained invocation id for a position, and
/// the position's §4 point, are the authority over the id a child invocation
/// presents.
///
/// - A dispatched child presenting its retained id is admitted.
/// - A `Some(_)` mismatch is [`EffectGroupAdmissionResponse::AttachExpired`],
///   not `Refused`: for the idempotency key to mint a second invocation id,
///   the retained one's retention expired.
/// - A child whose final is already committed is `AttachExpired` whatever id
///   it presents (FIG-4454). The invocation that committed it journaled its
///   own admission before its commit and replays that answer, so a live
///   admission of a committed child comes from an invocation whose journal
///   is gone: a successor the idempotency-keyed re-send minted, which Restate
///   may mint under the very id it retains. It drains the committed final and
///   never executes the child again.
/// - `Refused` stays for a position that was never dispatched and for the
///   `Cancel`/`Refused` close dispositions, where a late child is simply
///   disallowed — except a child committed before a `Cancel` close, which
///   the close protects and which drains as under `RunToCompletion`.
pub(crate) fn decide_group_child_admission(
    lifecycle: &EffectGroupLifecycle,
    position: usize,
    invocation_id: &str,
) -> EffectGroupAdmissionResponse {
    match lifecycle {
        // A preparing group records no child id yet: the registration that
        // records them makes the group ready in the same step (FIG-4308).
        EffectGroupLifecycle::Preparing { .. } => EffectGroupAdmissionResponse::NotYetRecorded,
        EffectGroupLifecycle::Ready { addresses, live } => {
            admit_dispatched(addresses, live, position, invocation_id)
        }
        EffectGroupLifecycle::Closed {
            effective,
            addresses,
            live,
            ..
        } => match effective {
            EffectGroupCloseOutcome::RunToCompletion => {
                admit_dispatched(addresses, live, position, invocation_id)
            }
            EffectGroupCloseOutcome::Cancel => match live.decision(position) {
                Some((_, EffectGroupSeat::CancelDecided)) => {
                    EffectGroupAdmissionResponse::CancelDecided
                }
                Some((_, EffectGroupSeat::Committed | EffectGroupSeat::Seated { .. })) => {
                    admit_dispatched(addresses, live, position, invocation_id)
                }
                None => EffectGroupAdmissionResponse::Refused,
            },
            EffectGroupCloseOutcome::Refused { .. } => EffectGroupAdmissionResponse::Refused,
        },
        EffectGroupLifecycle::Retired { .. } => EffectGroupAdmissionResponse::Retired,
    }
}

/// The admission of a dispatched position of a group whose children may run.
fn admit_dispatched(
    addresses: &BTreeMap<usize, String>,
    live: &EffectGroupStateLiveRecord,
    position: usize,
    invocation_id: &str,
) -> EffectGroupAdmissionResponse {
    match addresses.get(&position) {
        None => EffectGroupAdmissionResponse::Refused,
        Some(_)
            if matches!(
                live.decision(position),
                Some((
                    _,
                    EffectGroupSeat::Committed | EffectGroupSeat::Seated { .. }
                ))
            ) =>
        {
            EffectGroupAdmissionResponse::AttachExpired
        }
        Some(id) if id == invocation_id => EffectGroupAdmissionResponse::Admitted,
        Some(_) => EffectGroupAdmissionResponse::AttachExpired,
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn live_record() -> EffectGroupStateLiveRecord {
        EffectGroupStateLiveRecord::undecided(EffectGroupShape {
            wake: lash_core::GroupWakePolicy::All,
            loser_disposition: LoserPolicy::RunToCompletion,
            replay_keys: vec!["child-0".to_owned()],
            opener: lash_core::AdmittedScope::turn("session", "turn"),
        })
    }

    fn adopted_dispatch() -> EffectGroupDispatchState {
        EffectGroupDispatchState::Adopted {
            id: "dispatcher-1".to_owned(),
        }
    }

    #[test]
    fn a_retained_child_id_admits_its_own_invocation() {
        let lifecycle = EffectGroupLifecycle::Ready {
            addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
            live: live_record(),
        };
        assert_eq!(
            decide_group_child_admission(&lifecycle, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::Admitted
        );
    }

    /// The §8 surface: a successor minted under the idempotency key after the
    /// retained invocation's retention expired presents a *different* id, and
    /// the index answers `AttachExpired` — the typed failure the child
    /// records — rather than the silent `Refused` a cancel disallows.
    #[test]
    fn a_successor_with_an_expired_attachment_is_named_attach_expired() {
        for lifecycle in [
            EffectGroupLifecycle::Ready {
                addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
                live: live_record(),
            },
            EffectGroupLifecycle::Closed {
                effective: EffectGroupCloseOutcome::RunToCompletion,
                reopened: false,
                addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
                live: live_record(),
            },
        ] {
            assert_eq!(
                decide_group_child_admission(&lifecycle, 0, "a-fresh-invocation-id"),
                EffectGroupAdmissionResponse::AttachExpired,
                "{lifecycle:?}"
            );
        }
    }

    /// FIG-4454 R3: a `Cancel` close protects a committed child (ADR 0099
    /// §4), so a successor minted for it after its invocation expired is
    /// admitted to drain the committed final — named `AttachExpired`, as
    /// under `RunToCompletion` — and never refused. A committed child's
    /// live admission is always a successor's, whichever id it presents:
    /// the idempotency-keyed re-send may mint the very id the index retains.
    #[test]
    fn a_cancel_closed_groups_committed_child_admits_its_successor_to_drain() {
        let mut committed = live_record();
        committed.decide(0, EffectGroupSeat::Committed);
        let addresses: BTreeMap<usize, String> =
            [(0, "child-invocation-0".to_owned())].into_iter().collect();
        for lifecycle in [
            EffectGroupLifecycle::Closed {
                effective: EffectGroupCloseOutcome::Cancel,
                reopened: false,
                addresses: addresses.clone(),
                live: committed.clone(),
            },
            EffectGroupLifecycle::Closed {
                effective: EffectGroupCloseOutcome::RunToCompletion,
                reopened: false,
                addresses: addresses.clone(),
                live: committed.clone(),
            },
            EffectGroupLifecycle::Ready {
                addresses: addresses.clone(),
                live: committed.clone(),
            },
        ] {
            for presented in ["a-fresh-invocation-id", "child-invocation-0"] {
                assert_eq!(
                    decide_group_child_admission(&lifecycle, 0, presented),
                    EffectGroupAdmissionResponse::AttachExpired,
                    "{presented} under {lifecycle:?}"
                );
            }
        }
    }

    #[test]
    fn an_unrecorded_position_and_a_cancelled_group_still_refuse() {
        let preparing = EffectGroupLifecycle::Preparing {
            dispatch: adopted_dispatch(),
            live: live_record(),
        };
        assert_eq!(
            decide_group_child_admission(&preparing, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::NotYetRecorded
        );
        let cancelled = EffectGroupLifecycle::Closed {
            effective: EffectGroupCloseOutcome::Cancel,
            reopened: false,
            addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
            live: live_record(),
        };
        assert_eq!(
            decide_group_child_admission(&cancelled, 0, "a-fresh-invocation-id"),
            EffectGroupAdmissionResponse::Refused
        );
        // A child the close decided is told so: the close already released
        // its wait, so the child has nothing left to do (FIG-3630).
        let mut decided = live_record();
        decided.decide(0, EffectGroupSeat::CancelDecided);
        let decided = EffectGroupLifecycle::Closed {
            effective: EffectGroupCloseOutcome::Cancel,
            reopened: false,
            addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
            live: decided,
        };
        assert_eq!(
            decide_group_child_admission(&decided, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::CancelDecided
        );
        let retired = EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Complete {
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
        };
        assert_eq!(
            decide_group_child_admission(&retired, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::Retired
        );
    }
}
