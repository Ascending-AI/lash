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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupSettlementRecord {
    pub position: usize,
    pub sequence: u64,
    pub terminal: EffectGroupSettlementTerminal,
}

/// Which side of the §4 arbitration point committed for one child.
///
/// `record_settlement` is the child's final record reaching the point and
/// `close`/`retirement_cancel` are the cancel disposition reaching it, and
/// whichever wrote first holds it. `CancelDecided` is what turns a late
/// `record_settlement` into the typed `CancelDecided` refusal rather than an
/// indistinguishable `Duplicate`. A child absent from the map is `pending`.
///
/// Either side reserves the child's settlement rank at the point (FIG-4308):
/// `Committed` holds the rank its seat will publish once the child's drain
/// and projection are done, and a cancel decision seats its rank in the same
/// step. Rank order is therefore the order of §4 decisions; commit order is
/// that order restricted to committed children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupChildCommitState {
    Committed { rank: u64 },
    CancelDecided,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupStateLiveRecord {
    pub(crate) shape: EffectGroupShape,
    /// The next rank a §4 decision reserves.
    pub(crate) next_rank: u64,
    /// Every child whose §4 point is taken, by either side.
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) commit_states: BTreeMap<usize, EffectGroupChildCommitState>,
    /// The published settlements by rank: a reserved rank appears here only
    /// once its child has seated.
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) settlements: BTreeMap<u64, EffectGroupSettlementRecord>,
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) settled_positions: BTreeMap<usize, u64>,
}

impl EffectGroupStateLiveRecord {
    /// Reserves the next rank for a §4 decision.
    pub(crate) fn reserve_rank(&mut self, group_key: &str) -> Result<u64, TerminalError> {
        let rank = self.next_rank;
        self.next_rank = self.next_rank.checked_add(1).ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {group_key} exhausted settlement ranks"
            ))
        })?;
        Ok(rank)
    }

    /// The contiguous-seated watermark: the highest rank `r` such that every
    /// rank `1..=r` is seated, or 0. It is derived from the published
    /// settlements, so it can never disagree with them. A read is served only
    /// at or below it (FIG-4308).
    pub(crate) fn seated_prefix(&self) -> u64 {
        (1..)
            .take_while(|rank| self.settlements.contains_key(rank))
            .last()
            .unwrap_or(0)
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

#[cfg(test)]
#[test]
fn completed_retirement_index_serializes_as_tombstone_only() {
    let record = EffectGroupStateRecord {
        shape_digest: "shape-digest".to_owned(),
        dispatch_route: "EffectGroupDispatch".to_owned(),
        lifecycle: EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Complete {
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
        },
    };

    assert_eq!(
        serde_json::to_value(record).expect("serialize completed retirement tombstone"),
        serde_json::json!({
            "shape_digest": "shape-digest",
            "dispatch_route": "EffectGroupDispatch",
            "lifecycle": {
                "type": "retired",
                "cleanup": { "type": "complete", "opener": { "scope": { "type": "turn", "session_id": "session", "turn_id": "turn" } } }
            }
        })
    );
}

#[cfg(test)]
#[test]
fn retired_index_live_read_is_a_typed_terminal_error() {
    let record = EffectGroupStateRecord {
        shape_digest: "shape-digest".to_owned(),
        dispatch_route: "EffectGroupDispatch".to_owned(),
        lifecycle: EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Complete {
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
        },
    };

    let error = record
        .live()
        .expect_err("a retired group has no live state");
    assert!(
        error
            .to_string()
            .contains("has no live state after retirement")
    );
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
            Some((position, _)) => live.settled_positions.contains_key(position),
            None => {
                live.shape.children() > 0 && live.settled_positions.len() >= live.shape.children()
            }
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
///   never drives the child again.
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
            EffectGroupCloseOutcome::Cancel => match live.commit_states.get(&position) {
                Some(EffectGroupChildCommitState::CancelDecided) => {
                    EffectGroupAdmissionResponse::CancelDecided
                }
                Some(EffectGroupChildCommitState::Committed { .. }) => {
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
                live.commit_states.get(&position),
                Some(EffectGroupChildCommitState::Committed { .. })
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
        EffectGroupStateLiveRecord {
            shape: EffectGroupShape {
                wake: lash_core::GroupWakePolicy::All,
                loser_disposition: LoserPolicy::RunToCompletion,
                replay_keys: vec!["child-0".to_owned()],
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
            next_rank: 1,
            commit_states: BTreeMap::new(),
            settlements: BTreeMap::new(),
            settled_positions: BTreeMap::new(),
        }
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
        committed
            .commit_states
            .insert(0, EffectGroupChildCommitState::Committed { rank: 1 });
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
        decided
            .commit_states
            .insert(0, EffectGroupChildCommitState::CancelDecided);
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

#[cfg(test)]
mod paused_work_tests {
    use super::*;

    fn opener() -> lash_core::AdmittedScope {
        lash_core::AdmittedScope::turn("session", "turn")
    }

    fn live(children: usize) -> EffectGroupStateLiveRecord {
        EffectGroupStateLiveRecord {
            shape: EffectGroupShape {
                wake: lash_core::GroupWakePolicy::All,
                loser_disposition: LoserPolicy::RunToCompletion,
                replay_keys: (0..children)
                    .map(|child| format!("child-{child}"))
                    .collect(),
                opener: opener(),
            },
            next_rank: 1,
            commit_states: BTreeMap::new(),
            settlements: BTreeMap::new(),
            settled_positions: BTreeMap::new(),
        }
    }

    fn addresses(children: usize) -> BTreeMap<usize, String> {
        (0..children)
            .map(|child| (child, format!("invocation-{child}")))
            .collect()
    }

    fn ask(handler: &str, invocation: &str) -> EffectGroupOpenerRequest {
        EffectGroupOpenerRequest {
            handler: handler.to_owned(),
            invocation_id: invocation.to_owned(),
        }
    }

    fn needed() -> EffectGroupOpenerResponse {
        EffectGroupOpenerResponse::Needed { opener: opener() }
    }

    /// A closed group no longer hands ranks to its opener, and still needs
    /// the loser it left running: only a seated position is released.
    #[test]
    fn a_closed_groups_unseated_loser_is_needed_and_a_seated_position_is_not() {
        let mut winner_seated = live(2);
        winner_seated.settled_positions.insert(0, 1);
        for effective in [
            EffectGroupCloseOutcome::RunToCompletion,
            EffectGroupCloseOutcome::Cancel,
        ] {
            let closed = EffectGroupLifecycle::Closed {
                effective,
                reopened: false,
                addresses: addresses(2),
                live: winner_seated.clone(),
            };
            assert_eq!(
                paused_work_need(closed.clone(), &ask("child", "invocation-1")),
                needed(),
                "{closed:?}"
            );
            assert_eq!(
                paused_work_need(closed.clone(), &ask("child", "invocation-0")),
                EffectGroupOpenerResponse::Seated,
                "{closed:?}"
            );
            assert_eq!(
                paused_work_need(closed, &ask("run", "dispatcher")),
                needed()
            );
        }
        let mut decided = winner_seated;
        decided
            .commit_states
            .insert(1, EffectGroupChildCommitState::CancelDecided);
        decided.settled_positions.insert(1, 2);
        let cancelled = EffectGroupLifecycle::Closed {
            effective: EffectGroupCloseOutcome::Cancel,
            reopened: false,
            addresses: addresses(2),
            live: decided,
        };
        assert_eq!(
            paused_work_need(cancelled.clone(), &ask("child", "invocation-1")),
            EffectGroupOpenerResponse::Seated,
            "a cancel decision seated the position"
        );
        assert_eq!(
            paused_work_need(cancelled, &ask("child", "a-successor")),
            EffectGroupOpenerResponse::Seated,
            "no position is left for a successor to seat"
        );
    }

    #[test]
    fn open_groups_need_their_children_and_retired_ones_only_their_retirement() {
        let preparing = EffectGroupLifecycle::Preparing {
            dispatch: EffectGroupDispatchState::Unadopted,
            live: live(1),
        };
        assert_eq!(
            paused_work_need(preparing, &ask("child", "invocation-0")),
            needed()
        );
        let ready = EffectGroupLifecycle::Ready {
            addresses: addresses(2),
            live: live(2),
        };
        assert_eq!(
            paused_work_need(ready.clone(), &ask("child", "invocation-1")),
            needed()
        );
        assert_eq!(
            paused_work_need(ready, &ask("child", "a-successor")),
            needed(),
            "a successor seats a position still unseated"
        );
        for cleanup in [
            EffectGroupCleanup::Pending {
                facts: EffectGroupCleanupFacts {
                    replay_keys: vec!["child-0".to_owned()],
                    dispatcher: EffectGroupDispatchState::Unadopted,
                    dispatched: BTreeMap::new(),
                },
                live: Box::new(live(1)),
            },
            EffectGroupCleanup::Complete { opener: opener() },
        ] {
            let retired = EffectGroupLifecycle::Retired { cleanup };
            for handler in ["child", "run"] {
                assert_eq!(
                    paused_work_need(retired.clone(), &ask(handler, "invocation-0")),
                    EffectGroupOpenerResponse::Seated,
                    "{handler} of {retired:?}"
                );
            }
            assert_eq!(
                paused_work_need(retired, &ask("retire", "retirement")),
                needed()
            );
        }
    }
}
