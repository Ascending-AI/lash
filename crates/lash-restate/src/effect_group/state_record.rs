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
    Complete,
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
            cleanup: EffectGroupCleanup::Complete,
        },
    };

    assert_eq!(
        serde_json::to_value(record).expect("serialize completed retirement tombstone"),
        serde_json::json!({
            "shape_digest": "shape-digest",
            "dispatch_route": "EffectGroupDispatch",
            "lifecycle": {
                "type": "retired",
                "cleanup": { "type": "complete" }
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
            cleanup: EffectGroupCleanup::Complete,
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

/// The §8 admission decision, pure so its arms are exercisable without an
/// `ObjectContext`: the index's retained invocation id for a position is the
/// authority over the id a child invocation presents.
///
/// A `Some(_)` mismatch is [`EffectGroupAdmissionResponse::AttachExpired`],
/// not `Refused`: for the idempotency key to mint a second invocation id,
/// the retained one's retention expired. `Refused` stays for a position that
/// was never dispatched and for the `Cancel`/`Refused` close dispositions,
/// where a late child is simply disallowed.
pub(crate) fn decide_group_child_admission(
    lifecycle: &EffectGroupLifecycle,
    position: usize,
    invocation_id: &str,
) -> EffectGroupAdmissionResponse {
    match lifecycle {
        // A preparing group records no child id yet: the registration that
        // records them makes the group ready in the same step (FIG-4308).
        EffectGroupLifecycle::Preparing { .. } => EffectGroupAdmissionResponse::NotYetRecorded,
        EffectGroupLifecycle::Ready { addresses, .. } => match addresses.get(&position) {
            Some(id) if id == invocation_id => EffectGroupAdmissionResponse::Admitted,
            Some(_) => EffectGroupAdmissionResponse::AttachExpired,
            None => EffectGroupAdmissionResponse::Refused,
        },
        EffectGroupLifecycle::Closed {
            effective,
            addresses,
            live,
            ..
        } => match effective {
            EffectGroupCloseOutcome::RunToCompletion => match addresses.get(&position) {
                Some(id) if id == invocation_id => EffectGroupAdmissionResponse::Admitted,
                Some(_) => EffectGroupAdmissionResponse::AttachExpired,
                None => EffectGroupAdmissionResponse::Refused,
            },
            EffectGroupCloseOutcome::Cancel
                if matches!(
                    live.commit_states.get(&position),
                    Some(EffectGroupChildCommitState::CancelDecided)
                ) =>
            {
                EffectGroupAdmissionResponse::CancelDecided
            }
            EffectGroupCloseOutcome::Cancel | EffectGroupCloseOutcome::Refused { .. } => {
                EffectGroupAdmissionResponse::Refused
            }
        },
        EffectGroupLifecycle::Retired { .. } => EffectGroupAdmissionResponse::Retired,
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
            cleanup: EffectGroupCleanup::Complete,
        };
        assert_eq!(
            decide_group_child_admission(&retired, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::Retired
        );
    }
}
