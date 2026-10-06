use super::*;
use lash::SessionId;

// The workbench's session roster: which sessions the sidebar lists, which one
// a query-less `/api/` call serves, and how a retired slot is replaced or,
// for a deleted chat, removed.

/// One row of the workbench's durable session roster.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkbenchSessionEntry {
    pub(crate) session_id: SessionId,
    /// The operator's name for this session, or the id when they gave none.
    pub(crate) name: String,
    pub(crate) created_at_ms: i64,
    pub(crate) last_active_ms: i64,
}

/// The sessions this workbench knows about, and which one is current.
///
/// Two durable files, because they answer two questions and the first is
/// load-bearing for every driver in the battery: `session-id` stays exactly
/// what it was — the plain-text id a query-less `/api/` call resolves to, which
/// the runbooks read and write directly — and `sessions.json` beside it is the
/// roster the session list renders, one row per session.
///
/// A session the roster does not know still resolves, which is how every
/// pre-roster deployment and every ad-hoc `?session_id=` tab reads.
#[derive(Clone, Debug)]
pub(crate) struct WorkbenchSessions {
    pub(crate) current: Arc<Mutex<SessionId>>,
    pub(crate) path: Option<Arc<PathBuf>>,
    pub(crate) roster: Arc<Mutex<BTreeMap<SessionId, WorkbenchSessionEntry>>>,
    pub(crate) roster_path: Option<Arc<PathBuf>>,
    /// Which replacement each retired id was rotated onto, so the rotation is
    /// a fact this process can be asked for again instead of an event only the
    /// caller that performed it ever saw. Two callers race for it — the delete
    /// settle and the reset route — and both must be handed the same answer.
    replacements: Arc<Mutex<BTreeMap<SessionId, SessionId>>>,
    /// Retirements the operator asked to delete rather than reset. Such a slot
    /// is not rotated onto a fresh session: its row leaves the roster and its
    /// successor is the most recent chat that remains.
    removals: Arc<Mutex<BTreeSet<SessionId>>>,
}

impl WorkbenchSessions {
    #[cfg(test)]
    pub(crate) fn fresh() -> Self {
        Self {
            current: Arc::new(Mutex::new(new_session_id())),
            path: None,
            roster: Arc::new(Mutex::new(BTreeMap::new())),
            roster_path: None,
            replacements: Arc::new(Mutex::new(BTreeMap::new())),
            removals: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub(crate) fn persistent(path: PathBuf) -> AnyhowResult<Self> {
        let current = match std::fs::read_to_string(&path) {
            Ok(session_id) if !session_id.trim().is_empty() => SessionId::parse(session_id)?,
            Ok(_) => new_session_id(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => new_session_id(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read workbench session id `{}`", path.display()));
            }
        };
        let roster_path = path.with_file_name(SESSION_ROSTER_FILE_NAME);
        let roster = match std::fs::read(&roster_path) {
            Ok(bytes) => serde_json::from_slice::<Vec<WorkbenchSessionEntry>>(&bytes)
                .with_context(|| format!("decode workbench sessions `{}`", roster_path.display()))?
                .into_iter()
                .map(|entry| (entry.session_id.clone(), entry))
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("read workbench sessions `{}`", roster_path.display())
                });
            }
        };
        let ids = Self {
            current: Arc::new(Mutex::new(current)),
            path: Some(Arc::new(path)),
            roster: Arc::new(Mutex::new(roster)),
            roster_path: Some(Arc::new(roster_path)),
            replacements: Arc::new(Mutex::new(BTreeMap::new())),
            removals: Arc::new(Mutex::new(BTreeSet::new())),
        };
        ids.persist();
        Ok(ids)
    }

    pub(crate) fn current(&self) -> SessionId {
        self.current.lock_recover().clone()
    }

    /// Replace one retired roster slot without disturbing a session selected
    /// while the durable delete was settling.
    pub(crate) fn replace(&self, retired_session_id: &SessionId) -> (SessionId, bool) {
        let replacement_session_id = new_session_id();
        // Roster then current is the shared lock order with `select`: removing
        // the retired row and conditionally moving the pointer are one local
        // decision, so no selector can reinstall the tombstoned id between
        // those halves.
        let mut roster = self.roster.lock_recover();
        let carried = roster.remove(retired_session_id);
        let name = carried
            .map(|entry| entry.name)
            .unwrap_or_else(|| retired_session_id.to_string());
        let now_ms = chrono::Utc::now().timestamp_millis();
        roster.insert(
            replacement_session_id.clone(),
            WorkbenchSessionEntry {
                session_id: replacement_session_id.clone(),
                name,
                created_at_ms: now_ms,
                last_active_ms: now_ms,
            },
        );
        let mut current = self.current.lock_recover();
        let replaced_current = *current == *retired_session_id;
        if replaced_current {
            *current = replacement_session_id.clone();
        }
        drop(current);
        self.persist_roster(&roster);
        drop(roster);
        if replaced_current {
            self.persist();
        }
        (replacement_session_id, replaced_current)
    }

    /// Rotate a retired slot exactly once, whoever asks and however often.
    ///
    /// The rotation is what takes the page off a tombstoned id, and the reset
    /// route is not the only path that reaches it: the delete's own settlement
    /// rotates too, because a delete that completed durably must not leave the
    /// roster pointing at the tombstone when the route's result is lost (a
    /// dropped request, an ambiguous attach). Recording the replacement is
    /// what keeps those two callers from stranding a second empty session:
    /// whoever arrives later is handed the id the first one installed.
    pub(crate) fn replace_retired(&self, retired_session_id: &SessionId) -> (SessionId, bool) {
        // Replacements then roster then current is the lock order every
        // rotation takes, so the recorded answer and the roster it describes
        // cannot disagree across a race.
        let mut replacements = self.replacements.lock_recover();
        if let Some(replacement) = replacements.get(retired_session_id) {
            let replaced_current = *self.current.lock_recover() == *replacement;
            return (replacement.clone(), replaced_current);
        }
        let (replacement, replaced_current) =
            if self.removals.lock_recover().contains(retired_session_id) {
                self.remove(retired_session_id)
            } else {
                self.replace(retired_session_id)
            };
        replacements.insert(retired_session_id.clone(), replacement.clone());
        (replacement, replaced_current)
    }

    /// Record that `session_id`'s retirement is a delete, before it starts:
    /// whichever caller settles it then removes the row instead of rotating it.
    pub(crate) fn mark_for_removal(&self, session_id: &SessionId) {
        self.removals.lock_recover().insert(session_id.clone());
    }

    /// Forget a delete intent whose retirement did not happen.
    pub(crate) fn unmark_for_removal(&self, session_id: &SessionId) {
        self.removals.lock_recover().remove(session_id);
    }

    /// Drop a deleted session's row and hand back its successor: the most
    /// recently active chat left, or a fresh untitled one when none is.
    fn remove(&self, deleted_session_id: &SessionId) -> (SessionId, bool) {
        // Roster then current, the lock order `replace` and `select` take.
        let mut roster = self.roster.lock_recover();
        roster.remove(deleted_session_id);
        let successor = roster
            .values()
            .max_by_key(|entry| {
                (
                    entry.last_active_ms.max(entry.created_at_ms),
                    entry.session_id.clone(),
                )
            })
            .map(|entry| entry.session_id.clone())
            .unwrap_or_else(|| {
                let fresh = new_session_id();
                let now_ms = chrono::Utc::now().timestamp_millis();
                roster.insert(
                    fresh.clone(),
                    WorkbenchSessionEntry {
                        session_id: fresh.clone(),
                        name: fresh.to_string(),
                        created_at_ms: now_ms,
                        last_active_ms: now_ms,
                    },
                );
                fresh
            });
        let mut current = self.current.lock_recover();
        let replaced_current = *current == *deleted_session_id;
        if replaced_current {
            *current = successor.clone();
        }
        drop(current);
        self.persist_roster(&roster);
        drop(roster);
        if replaced_current {
            self.persist();
        }
        (successor, replaced_current)
    }

    #[cfg(test)]
    pub(crate) fn rotate(&self) -> (SessionId, SessionId) {
        let old = self.current();
        let (new, replaced_current) = self.replace(&old);
        debug_assert!(replaced_current);
        (old, new)
    }

    /// Add a session to the roster, or refresh the row of one already there.
    pub(crate) fn record(&self, session_id: SessionId, name: String) -> WorkbenchSessionEntry {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut roster = self.roster.lock_recover();
        let entry = roster
            .entry(session_id.clone())
            .and_modify(|entry| {
                entry.name = name.clone();
                entry.last_active_ms = now_ms;
            })
            .or_insert(WorkbenchSessionEntry {
                session_id,
                name,
                created_at_ms: now_ms,
                last_active_ms: now_ms,
            })
            .clone();
        self.persist_roster(&roster);
        entry
    }

    /// This is how the boot session joins the roster: a row that already
    /// exists wins.
    pub(crate) fn ensure(&self, session_id: &SessionId) {
        if self.roster.lock_recover().contains_key(session_id) {
            return;
        }
        self.record(session_id.clone(), session_id.to_string());
    }

    /// A sent prompt is what makes a session recently active, and the first
    /// prompt of a session nobody named is its title, the way a chat list
    /// titles a conversation by what it is about rather than by its id.
    pub(crate) fn record_prompt(&self, session_id: &SessionId, prompt: &str) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut roster = self.roster.lock_recover();
        let Some(entry) = roster.get_mut(session_id) else {
            return;
        };
        entry.last_active_ms = now_ms;
        if session_name_is_unnamed(&entry.name, session_id)
            && let Some(title) = session_title_from_prompt(prompt)
        {
            entry.name = title;
        }
        self.persist_roster(&roster);
    }

    /// A row for a session the roster never recorded, so the selector can show
    /// it without the read side writing to the roster.
    pub(crate) fn unrostered_entry(&self, session_id: SessionId) -> WorkbenchSessionEntry {
        WorkbenchSessionEntry {
            name: session_id.to_string(),
            session_id,
            created_at_ms: 0,
            last_active_ms: 0,
        }
    }

    pub(crate) fn entry(&self, session_id: &SessionId) -> Option<WorkbenchSessionEntry> {
        self.roster.lock_recover().get(session_id).cloned()
    }

    /// The roster, oldest first; the sidebar re-orders it by recency.
    pub(crate) fn list(&self) -> Vec<WorkbenchSessionEntry> {
        let mut entries = self
            .roster
            .lock_recover()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        entries
    }

    /// Selection is durable for the same reason the boot id is: a reload, a
    /// restart, and the drivers that read `<data-dir>/session-id` must all
    /// agree on which session the workbench is serving.
    pub(crate) fn select(&self, session_id: &SessionId) -> Option<WorkbenchSessionEntry> {
        let roster = self.roster.lock_recover();
        let entry = roster.get(session_id)?.clone();
        *self.current.lock_recover() = session_id.clone();
        drop(roster);
        self.persist();
        // Selecting is reading, not use: the list keeps its order under the
        // operator's click.
        Some(entry)
    }

    #[expect(
        clippy::expect_used,
        reason = "roster entries are serde structs of plain strings and enums, so the \
                  pretty JSON encode cannot fail"
    )]
    pub(crate) fn persist_roster(&self, roster: &BTreeMap<SessionId, WorkbenchSessionEntry>) {
        let Some(path) = self.roster_path.as_deref() else {
            return;
        };
        let entries = roster.values().cloned().collect::<Vec<_>>();
        let encoded =
            serde_json::to_vec_pretty(&entries).expect("workbench session roster serializes");
        crate::replace_file(path, &encoded, "session roster");
    }

    pub(crate) fn persist(&self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let current = self.current();
        crate::replace_file(path, current.as_str().as_bytes(), "session id");
    }
}

/// The longest title a first prompt is cut to, so a list row stays one line.
const SESSION_TITLE_MAX_CHARS: usize = 60;

/// A roster name nobody chose: the session's own id, or a generated id a
/// replacement carried over from the slot it rotated.
pub(crate) fn session_name_is_unnamed(name: &str, session_id: &SessionId) -> bool {
    name == session_id.as_str()
        || name
            .strip_prefix(SESSION_ID_PREFIX)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| {
                rest.len() == 32 && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

/// The prompt's first line, whitespace collapsed, cut on a char boundary.
pub(crate) fn session_title_from_prompt(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= SESSION_TITLE_MAX_CHARS {
        return Some(collapsed);
    }
    let cut = collapsed
        .chars()
        .take(SESSION_TITLE_MAX_CHARS - 1)
        .collect::<String>();
    Some(format!("{}…", cut.trim_end()))
}

pub(crate) fn new_session_id() -> SessionId {
    SessionId::prefixed(
        SESSION_ID_PREFIX,
        format_args!("-{}", uuid::Uuid::new_v4().simple()),
    )
}
