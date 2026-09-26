use super::*;
use lash::SessionId;
use lash::TurnId;

/// Who claimed a turn.
///
/// The kind travels with the claim rather than being re-derived from the
/// turn id's prefix. Turn ids keep their prefixes because they are useful in
/// traces; they are not load-bearing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkbenchTurnKind {
    /// A turn the browser started through `/api/turn`.
    User,
    /// A turn the queued-work drain started.
    Queued,
}

impl WorkbenchTurnKind {
    /// Recover a kind for a turn restored from an active-turns file written
    /// before the kind was persisted.
    ///
    /// This is the one surviving turn-id sniff and the only place it is
    /// allowed: it reads a file this build did not write, where the fact is
    /// genuinely absent and the id prefix is the only evidence left. Every
    /// live claim carries its kind, so nothing else may call this.
    fn legacy_from_turn_id(turn_id: &TurnId) -> Self {
        if turn_id.starts_with(QUEUED_TURN_ID_PREFIX) {
            Self::Queued
        } else {
            Self::User
        }
    }
}

/// The id prefix queued turns are minted with, for traces and for reading a
/// pre-kind active-turns file.
pub(crate) const QUEUED_TURN_ID_PREFIX: &str = "workbench-queued-";

/// One session's single claimed turn.
///
/// The turn id, the workflow that owns it and the optimistic prompt the page
/// is waiting to see replayed are one value under one lock, so no reader can
/// observe a turn without its prompt or a prompt whose turn has been removed.
#[derive(Clone, Debug)]
struct ActiveTurnSlot {
    turn_id: TurnId,
    kind: WorkbenchTurnKind,
    prompt: Option<ActiveTurnPrompt>,
}

/// A session's claimed turn as a reader sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveTurn {
    pub(crate) address: lash::TurnAddress,
    pub(crate) kind: WorkbenchTurnKind,
    pub(crate) prompt: Option<ActiveTurnPrompt>,
}

/// The in-process turn registry and the session fence, under one lock.
///
/// Turn admission and session retirement race on exactly one fact — whether
/// this session may still start work — so the retirement marks live in the
/// same ledger the turn claim reads. A claim taken under this lock is either
/// ordered before the delete marked the session (and the delete then cancels
/// and settles it) or refused by the mark; there is no third interleaving.
///
/// A session holds at most one turn *structurally*: the ledger is keyed by
/// session, so the claim is a lookup and "two active turns for one session" is
/// not a state this type can be in. It used to be a `BTreeSet<(SessionId,
/// TurnId)>` beside a separately locked prompt map, where the invariant was
/// enforced by a linear scan that the test-only inserts bypassed, and where a
/// projection reading the turn and then its prompt could see a removal land
/// between the two.
///
/// Only the turns are persisted. The marks are an in-process ordering device:
/// after a restart the durable session tombstone is the authority, and every
/// admission read consults it as well (`AppState::admit_session`).
#[derive(Clone, Default)]
pub(crate) struct ActiveTurns {
    inner: Arc<Mutex<ActiveTurnLedger>>,
    pub(crate) path: Option<Arc<PathBuf>>,
    /// The roots this process follows to settlement, and the sessions it
    /// watches for roots its engine starts on its own. In-process only.
    pub(crate) follows: crate::restate::RootFollows,
}

#[derive(Default)]
struct ActiveTurnLedger {
    turns: BTreeMap<SessionId, ActiveTurnSlot>,
    retirements: BTreeMap<SessionId, SessionRetirement>,
}

/// Where a session stands in retirement, as recorded by this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionRetirement {
    /// A delete is in flight, or its outcome is still unconfirmed. Turn
    /// admission refuses; a delete retry may proceed.
    Retiring,
    /// The durable tombstone is confirmed. Every session-bound surface refuses.
    Retired,
}

/// The outcome of claiming the idle slot for a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActiveTurnClaim {
    /// The turn now owns the session's single active slot.
    Claimed,
    /// Another turn owns the slot; the caller queues or refuses.
    Busy,
    /// The session is retiring or retired; no turn may start.
    Refused(SessionRetirement),
}

impl ActiveTurnClaim {
    #[cfg(test)]
    pub(crate) fn is_claimed(self) -> bool {
        matches!(self, Self::Claimed)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveTurnPrompt {
    pub(crate) text: String,
    pub(crate) attachment_id: Option<String>,
}

/// Cleans up a user turn's active-turn claim unless its `send()` is accepted.
///
/// This guard lives inside the detached admission task. It therefore runs when
/// the send returns an error, the task is cancelled during runtime shutdown,
/// or the task unwinds after a panic: it releases the claim, retires the
/// optimistic row and publishes the terminal failure the browser expects.
pub(crate) struct ActiveTurnSubmissionGuard {
    pub(crate) active_turns: ActiveTurns,
    pub(crate) failure_publisher: AppState,
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) armed: bool,
}

impl ActiveTurnSubmissionGuard {
    pub(crate) fn user_turn(state: &AppState, session_id: &SessionId, turn_id: &TurnId) -> Self {
        Self {
            active_turns: state.active_turns.clone(),
            failure_publisher: state.clone(),
            session_id: session_id.clone(),
            turn_id: TurnId::from(turn_id.to_string()),
            armed: true,
        }
    }

    pub(crate) fn complete(mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveTurnSubmissionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let already_panicking = std::thread::panicking();
        let removal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.active_turns.remove(&self.session_id, &self.turn_id);
        }));
        let publication = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.failure_publisher
                .publish_turn_failed(&self.session_id, &self.turn_id);
        }));
        let cleanup_panic = removal.err().or_else(|| publication.err());
        if let Some(payload) = cleanup_panic {
            if already_panicking {
                eprintln!("turn admission cleanup panicked while preserving the original panic");
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

/// The on-disk shape, unchanged apart from the additive `kinds` array.
///
/// `turns` and `prompts` keep their exact form, so a file written before the
/// kind existed still loads; entries it does not name fall back to
/// [`WorkbenchTurnKind::legacy_from_turn_id`]. The reader tolerates the states
/// the old two-array shape could represent and the new ledger cannot: a prompt
/// or kind naming a turn that is absent from `turns` is dropped, and a second
/// turn for a session that already has one is dropped, rather than panicking.
#[derive(Deserialize)]
pub(crate) struct PersistedActiveTurns {
    pub(crate) turns: BTreeSet<(SessionId, TurnId)>,
    #[serde(default)]
    pub(crate) prompts: Vec<PersistedActiveTurnPrompt>,
    #[serde(default)]
    pub(crate) kinds: Vec<PersistedActiveTurnKind>,
}

#[derive(Serialize)]
pub(crate) struct PersistedActiveTurnsRef<'a> {
    pub(crate) turns: BTreeSet<(&'a SessionId, &'a TurnId)>,
    pub(crate) prompts: Vec<PersistedActiveTurnPromptRef<'a>>,
    pub(crate) kinds: Vec<PersistedActiveTurnKindRef<'a>>,
}

#[derive(Deserialize)]
pub(crate) struct PersistedActiveTurnPrompt {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) attachment_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PersistedActiveTurnPromptRef<'a> {
    pub(crate) session_id: &'a SessionId,
    pub(crate) turn_id: &'a TurnId,
    pub(crate) prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<&'a str>,
}

#[derive(Deserialize)]
pub(crate) struct PersistedActiveTurnKind {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) kind: WorkbenchTurnKind,
}

#[derive(Serialize)]
pub(crate) struct PersistedActiveTurnKindRef<'a> {
    pub(crate) session_id: &'a SessionId,
    pub(crate) turn_id: &'a TurnId,
    pub(crate) kind: WorkbenchTurnKind,
}

impl ActiveTurns {
    pub(crate) fn persistent(path: PathBuf) -> AnyhowResult<Self> {
        let turns = match std::fs::read(&path) {
            Ok(bytes) => {
                let persisted: PersistedActiveTurns = serde_json::from_slice(&bytes)
                    .map_err(|error| {
                        let hint = serde_json::from_slice::<serde_json::Value>(&bytes)
                            .ok()
                            .filter(serde_json::Value::is_array)
                            .map(|_| {
                                "; legacy bare active turn set is no longer supported; \
                                 expected an object with `turns` and `prompts`"
                            })
                            .unwrap_or("");
                        anyhow::anyhow!("{error}{hint}")
                    })
                    .with_context(|| format!("decode active turns `{}`", path.display()))?;
                restore_ledger_turns(persisted)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("read active turns `{}`", path.display()));
            }
        };
        let active = Self {
            inner: Arc::new(Mutex::new(ActiveTurnLedger {
                turns,
                retirements: BTreeMap::new(),
            })),
            path: Some(Arc::new(path)),
            follows: crate::restate::RootFollows::default(),
        };
        active.persist();
        Ok(active)
    }

    #[cfg(test)]
    pub(crate) fn insert(
        &self,
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
        kind: WorkbenchTurnKind,
    ) {
        self.insert_with_prompt(session_id, turn_id, kind, None, None);
    }

    #[cfg(test)]
    pub(crate) fn insert_with_prompt(
        &self,
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
        kind: WorkbenchTurnKind,
        prompt: Option<String>,
        attachment_id: Option<String>,
    ) {
        let mut ledger = self.inner.lock_recover();
        ledger.turns.insert(
            session_id.into(),
            ActiveTurnSlot {
                turn_id: turn_id.into(),
                kind,
                prompt: prompt.map(|text| ActiveTurnPrompt {
                    text,
                    attachment_id,
                }),
            },
        );
        self.persist_snapshot(&ledger.turns);
    }

    #[cfg(test)]
    pub(crate) fn try_insert_for_idle_session(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        kind: WorkbenchTurnKind,
    ) -> ActiveTurnClaim {
        self.try_insert_with_prompt_for_idle_session(session_id, turn_id, kind, None, None)
    }

    /// Claim the session's single active slot for `turn_id`, unless the session
    /// is busy or fenced.
    ///
    /// The retirement read and the slot claim happen under one lock: a delete
    /// that marks the session before this claim refuses it, and a claim that
    /// lands first is a turn the delete will find in the registry and cancel.
    pub(crate) fn try_insert_with_prompt_for_idle_session(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        kind: WorkbenchTurnKind,
        prompt: Option<String>,
        attachment_id: Option<String>,
    ) -> ActiveTurnClaim {
        let mut ledger = self.inner.lock_recover();
        if let Some(retirement) = ledger.retirements.get(session_id) {
            return ActiveTurnClaim::Refused(*retirement);
        }
        if ledger.turns.contains_key(session_id) {
            return ActiveTurnClaim::Busy;
        }
        ledger.turns.insert(
            session_id.clone(),
            ActiveTurnSlot {
                turn_id: turn_id.clone(),
                kind,
                prompt: prompt.map(|text| ActiveTurnPrompt {
                    text,
                    attachment_id,
                }),
            },
        );
        self.persist_snapshot(&ledger.turns);
        ActiveTurnClaim::Claimed
    }

    /// Release `turn_id`'s claim on `session_id`.
    ///
    /// Addressed, not a session-wide clear: a slot already reclaimed by a later
    /// turn is left alone, which is what makes the submission guard's cleanup
    /// safe to run late.
    pub(crate) fn remove(&self, session_id: &SessionId, turn_id: &TurnId) {
        let mut ledger = self.inner.lock_recover();
        let holds_turn = ledger
            .turns
            .get(session_id)
            .is_some_and(|slot| slot.turn_id == *turn_id);
        if !holds_turn {
            return;
        }
        ledger.turns.remove(session_id);
        self.persist_snapshot(&ledger.turns);
    }

    pub(crate) fn contains(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        self.inner
            .lock_recover()
            .turns
            .get(session_id)
            .is_some_and(|slot| slot.turn_id == *turn_id)
    }

    /// Every session's claimed turn.
    pub(crate) fn snapshot(&self) -> Vec<ActiveTurn> {
        self.inner
            .lock_recover()
            .turns
            .iter()
            .map(|(session_id, slot)| ActiveTurn {
                address: lash::TurnAddress::new(session_id, &slot.turn_id),
                kind: slot.kind,
                prompt: slot.prompt.clone(),
            })
            .collect()
    }

    /// The session's claimed turn, with who claimed it and the prompt the
    /// page is waiting on, read together under one lock.
    pub(crate) fn for_session(&self, session_id: &SessionId) -> Option<ActiveTurn> {
        self.inner
            .lock_recover()
            .turns
            .get(session_id)
            .map(|slot| ActiveTurn {
                address: lash::TurnAddress::new(session_id, &slot.turn_id),
                kind: slot.kind,
                prompt: slot.prompt.clone(),
            })
    }

    /// Idempotent: a session already retiring or retired keeps its mark, and the return value
    /// says whether this call placed one.
    pub(crate) fn begin_retirement(&self, session_id: &SessionId) -> bool {
        let mut ledger = self.inner.lock_recover();
        if ledger.retirements.contains_key(session_id) {
            return false;
        }
        ledger
            .retirements
            .insert(session_id.clone(), SessionRetirement::Retiring);
        true
    }

    /// Record that the durable tombstone for `session_id` is confirmed, and
    /// retire the session's routing along with it.
    ///
    /// A cancel that could not attach a terminal keeps the turn in this
    /// registry on purpose: the turn is still routable and may yet commit its
    /// own terminal, so `turn.cancel_liveness_unknown` retains it rather than
    /// claim an outcome nobody observed. A confirmed tombstone ends that. The
    /// session id is gone, every store write against it is refused, and no
    /// terminal can land for it -- there is no route left to retain. Holding
    /// the rows left `for_session` non-empty for a deleted id for the life of
    /// the process, and the ledger is persisted, so past the next boot too
    /// (FIG-3018). The mark and the rows move under one lock, so no reader
    /// sees `Retired` beside a live route.
    pub(crate) fn confirm_retirement(&self, session_id: &SessionId) {
        let mut ledger = self.inner.lock_recover();
        ledger
            .retirements
            .insert(session_id.clone(), SessionRetirement::Retired);
        ledger.turns.remove(session_id);
        self.persist_snapshot(&ledger.turns);
    }

    /// Lift a retiring mark after the delete definitively failed and the
    /// session remains live. A confirmed retirement is never lifted: a deleted
    /// session id cannot come back.
    pub(crate) fn abandon_retirement(&self, session_id: &SessionId) {
        let mut ledger = self.inner.lock_recover();
        if ledger.retirements.get(session_id) == Some(&SessionRetirement::Retiring) {
            ledger.retirements.remove(session_id);
        }
    }

    pub(crate) fn retirement(&self, session_id: &SessionId) -> Option<SessionRetirement> {
        self.inner
            .lock_recover()
            .retirements
            .get(session_id)
            .copied()
    }

    /// The prompt held beside `turn_id`'s claim.
    ///
    /// Production reads it through [`Self::for_session`], which hands back the
    /// turn and its prompt from one read; this remains for tests that assert
    /// on the prompt alone.
    #[cfg(test)]
    pub(crate) fn prompt_for(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Option<ActiveTurnPrompt> {
        self.inner
            .lock_recover()
            .turns
            .get(session_id)
            .filter(|slot| slot.turn_id == *turn_id)
            .and_then(|slot| slot.prompt.clone())
    }

    pub(crate) fn persist(&self) {
        let ledger = self.inner.lock_recover();
        self.persist_snapshot(&ledger.turns);
    }

    #[expect(
        clippy::expect_used,
        reason = "PersistedActiveTurnsRef holds only serde-serializable plain data, so the \
                  encode cannot fail"
    )]
    fn persist_snapshot(&self, active: &BTreeMap<SessionId, ActiveTurnSlot>) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let turns = active
            .iter()
            .map(|(session_id, slot)| (session_id, &slot.turn_id))
            .collect();
        let prompts = active
            .iter()
            .filter_map(|(session_id, slot)| {
                slot.prompt
                    .as_ref()
                    .map(|prompt| PersistedActiveTurnPromptRef {
                        session_id,
                        turn_id: &slot.turn_id,
                        prompt: &prompt.text,
                        attachment_id: prompt.attachment_id.as_deref(),
                    })
            })
            .collect();
        let kinds = active
            .iter()
            .map(|(session_id, slot)| PersistedActiveTurnKindRef {
                session_id,
                turn_id: &slot.turn_id,
                kind: slot.kind,
            })
            .collect();
        let bytes = serde_json::to_vec(&PersistedActiveTurnsRef {
            turns,
            prompts,
            kinds,
        })
        .expect("serialize active turns");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .unwrap_or_else(|err| panic!("write active turns `{}`: {err}", temporary.display()));
        std::fs::rename(&temporary, path).unwrap_or_else(|err| {
            panic!(
                "replace active turns `{}` from `{}`: {err}",
                path.display(),
                temporary.display()
            )
        });
    }
}

/// Rebuild the ledger from a decoded file, dropping what the new shape cannot
/// hold rather than failing the boot.
fn restore_ledger_turns(persisted: PersistedActiveTurns) -> BTreeMap<SessionId, ActiveTurnSlot> {
    let mut kinds = persisted
        .kinds
        .into_iter()
        .map(|kind| ((kind.session_id, kind.turn_id), kind.kind))
        .collect::<BTreeMap<_, _>>();
    let mut prompts = persisted
        .prompts
        .into_iter()
        .map(|prompt| {
            (
                (prompt.session_id, prompt.turn_id),
                ActiveTurnPrompt {
                    text: prompt.prompt,
                    attachment_id: prompt.attachment_id,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut turns = BTreeMap::<SessionId, ActiveTurnSlot>::new();
    for key in persisted.turns {
        if turns.contains_key(&key.0) {
            // The old set could hold two turns for one session; the ledger
            // cannot, and the first in key order is as good a survivor as any.
            continue;
        }
        let kind = kinds
            .remove(&key)
            .unwrap_or_else(|| WorkbenchTurnKind::legacy_from_turn_id(&key.1));
        let prompt = prompts.remove(&key);
        let (session_id, turn_id) = key;
        turns.insert(
            session_id,
            ActiveTurnSlot {
                turn_id,
                kind,
                prompt,
            },
        );
    }
    turns
}
