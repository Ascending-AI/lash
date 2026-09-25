use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCleanupFacts {
    pub replay_keys: Vec<String>,
    pub dispatcher: EffectGroupDispatchState,
    #[serde(with = "btree_map_as_pairs")]
    pub dispatched: BTreeMap<usize, String>,
    pub wait_scope: ExecutionScope,
}

impl EffectGroupCleanupFacts {
    /// The declared arity, derived from the retained replay keys — the same
    /// answer [`EffectGroupShape::children`] gives on the live shape.
    pub(crate) fn children(&self) -> usize {
        self.replay_keys.len()
    }

    /// The replay key of a child position, as a terminal error when the
    /// retirement facts do not have one. Same pairing, same independent public
    /// fields, and the same refusal as [`EffectGroupShape::replay_key`].
    pub(crate) fn replay_key(&self, position: usize) -> Result<&str, TerminalError> {
        self.replay_keys
            .get(position)
            .map(String::as_str)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "effect-group retirement facts have no replay key for child {position} of {}",
                    self.replay_keys.len()
                ))
            })
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
        effective: EffectGroupCloseDisposition,
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
pub enum EffectGroupCloseDisposition {
    RunToCompletion,
    Cancel,
    Refused { reason: EffectGroupRefusal },
}

impl From<LoserPolicy> for EffectGroupCloseDisposition {
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
/// The durable twin of the SQL stores' `runtime_effect_replay.commit_state`:
/// `record_settlement` is the child's final record reaching the point and
/// `close`/`retirement_cancel` are the cancel disposition reaching it, and
/// whichever wrote first holds it. `CancelDecided` is what turns a late
/// `record_settlement` into the typed `CancelDecided` refusal rather than an
/// indistinguishable `Duplicate`. A child absent from the map is `pending` —
/// the SQL tiers' `pending` spelled as "no row yet" — and `drained` has no
/// variant because this tier fuses commit and drain.
///
/// `commit_seq` is the child's durable position in the group's final-commit
/// order, allocated from `next_commit_seq` at the point. On this tier commit
/// and rank stay fused — a child's declared intents land inside its journaled
/// run before `record_settlement` is reached, so there is no post-commit drain
/// to gate — but the two counters still diverge: a cancel-decided child holds
/// a rank and no commit position, so commit order is its own recorded fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupChildCommitState {
    Committed { commit_seq: u64 },
    CancelDecided,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupStateLiveRecord {
    pub(crate) shape: EffectGroupShape,
    pub(crate) next_rank: u64,
    /// The final-commit counter: the position `record_settlement` allocates
    /// from, the `next_seq` twin the settlement rank allocates from.
    pub(crate) next_commit_seq: u64,
    /// Every child whose §4 point is taken, by either side.
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) commit_states: BTreeMap<usize, EffectGroupChildCommitState>,
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) settlements: BTreeMap<u64, EffectGroupSettlementRecord>,
    #[serde(with = "btree_map_as_pairs")]
    pub(crate) settled_positions: BTreeMap<usize, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct EffectGroupStateRecord {
    pub(crate) shape_digest: String,
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
        lifecycle: EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Complete,
        },
    };

    assert_eq!(
        serde_json::to_value(record).expect("serialize completed retirement tombstone"),
        serde_json::json!({
            "shape_digest": "shape-digest",
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
        EffectGroupLifecycle::Preparing {
            dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
            ..
        } => match dispatched.get(&position) {
            None => EffectGroupAdmissionResponse::NotYetRecorded,
            Some(id) if id == invocation_id => EffectGroupAdmissionResponse::Admitted,
            Some(_) => EffectGroupAdmissionResponse::AttachExpired,
        },
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
            EffectGroupCloseDisposition::RunToCompletion => match addresses.get(&position) {
                Some(id) if id == invocation_id => EffectGroupAdmissionResponse::Admitted,
                Some(_) => EffectGroupAdmissionResponse::AttachExpired,
                None => EffectGroupAdmissionResponse::Refused,
            },
            EffectGroupCloseDisposition::Cancel
                if matches!(
                    live.commit_states.get(&position),
                    Some(EffectGroupChildCommitState::CancelDecided)
                ) =>
            {
                EffectGroupAdmissionResponse::CancelDecided
            }
            EffectGroupCloseDisposition::Cancel | EffectGroupCloseDisposition::Refused { .. } => {
                EffectGroupAdmissionResponse::Refused
            }
        },
        EffectGroupLifecycle::Preparing { .. } => EffectGroupAdmissionResponse::NotYetRecorded,
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
                wait_scope: ExecutionScope::runtime_operation("group"),
                membership: vec!["{}".to_owned()],
                opener: lash_core::AdmittedScope::turn("session", "turn"),
            },
            next_rank: 1,
            next_commit_seq: 1,
            commit_states: BTreeMap::new(),
            settlements: BTreeMap::new(),
            settled_positions: BTreeMap::new(),
        }
    }

    fn adopted_dispatch(dispatched: &[usize]) -> EffectGroupDispatchState {
        EffectGroupDispatchState::Adopted {
            id: "dispatcher-1".to_owned(),
            dispatched: dispatched
                .iter()
                .map(|position| (*position, format!("child-invocation-{position}")))
                .collect(),
        }
    }

    #[test]
    fn a_retained_child_id_admits_its_own_invocation() {
        let lifecycle = EffectGroupLifecycle::Preparing {
            dispatch: adopted_dispatch(&[0]),
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
            EffectGroupLifecycle::Preparing {
                dispatch: adopted_dispatch(&[0]),
                live: live_record(),
            },
            EffectGroupLifecycle::Ready {
                addresses: [(0, "child-invocation-0".to_owned())].into_iter().collect(),
                live: live_record(),
            },
            EffectGroupLifecycle::Closed {
                effective: EffectGroupCloseDisposition::RunToCompletion,
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
            dispatch: adopted_dispatch(&[]),
            live: live_record(),
        };
        assert_eq!(
            decide_group_child_admission(&preparing, 0, "child-invocation-0"),
            EffectGroupAdmissionResponse::NotYetRecorded
        );
        let cancelled = EffectGroupLifecycle::Closed {
            effective: EffectGroupCloseDisposition::Cancel,
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
            effective: EffectGroupCloseDisposition::Cancel,
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
