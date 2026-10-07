//! A replayed language command's journal guard (FIG-3586), split out of the
//! deleted scoped controller verbatim. A cell resumes from its snapshot
//! (L7, FIG-5177) and never replays a command; the guard goes with the
//! commands' issue path, once a cell's operation runs as its tool's own
//! admitted execution (L4, FIG-5174) and a process resumes from its snapshot
//! (L7b, FIG-5198).

use std::sync::Arc;

use crate::RuntimeEffectControllerError;

///
/// A re-executed lashlang run knows, from one read of its key namespace,
/// whether the journal holds anything at a command's ordinal and anything
/// beyond it. It cannot know before the command runs whether the command will
/// write — a tool call can settle during preparation, an aggregate's leaves
/// can all fail before any is admitted — so it hands the command this guard
/// instead, on the controller the command's effects are issued through:
///
/// * a command the journal holds rows for is **open**: its writes pass, and
///   the guard remembers that one was made, so a command that no longer
///   writes what the journal recorded is caught after it returns;
/// * a command the journal holds nothing for while it still holds entries
///   beyond it is **refusing**: its first write under the run's namespace, or
///   one that names no key (a group open, a proxied process command), is
///   refused with the run's divergence, before anything is claimed, because
///   the recorded run did not dispatch it there and nothing may be dispatched
///   live inside a recorded run. A write outside the namespace — the call's
///   presentation, keyed by its call id — passes: the recorded run may have
///   made it with nothing under the namespace (a call settled in
///   preparation), and the host
///   judges it against its own record (FIG-3680).
///
/// A command the journal holds while it still holds entries beyond it is
/// **fenced by key**: a write to a key under the run's namespace that the
/// journal does not hold is refused, because the recorded run wrote nothing
/// there and something past it is recorded — a leaf whose operation moved to
/// another kind, a handle awaited in another order. Keys outside the
/// namespace (an incorporation record, a nested process's own journal) are
/// the host's to judge and pass.
///
/// A command that calls a host tool binding which drifted since the pass that
/// wrote the journal (FIG-3587) is also **served only**: every effect it
/// issues that would dispatch — a tool attempt, a retry sleep, anything but a
/// pure wait — carries the drift refusal to its engine on the local executor
/// ([`RuntimeEffectLocalExecutor::served_only_refusal`](crate::RuntimeEffectLocalExecutor::served_only_refusal)). The engine serves an
/// outcome its journal holds; one it holds none for would reach the drifted
/// tool live, so the engine refuses it with the drift instead, running and
/// recording nothing (FIG-3719). A wait on an external completion dispatches
/// nothing, so it carries no refusal.
#[derive(Debug)]
pub struct CommandJournalGuard {
    refusal: Option<RefusedWriteRange>,
    fence: Option<RecordedKeyFence>,
    pub(crate) served_only: Option<ServedOnlyRange>,
    touched: std::sync::atomic::AtomicBool,
    tripped: std::sync::Mutex<Option<RuntimeEffectControllerError>>,
}

/// The recorded keys of a run's namespace a replayed command's writes must
/// land on while the journal holds entries beyond it (FIG-3586).
#[derive(Clone, Debug)]
pub struct RecordedKeyFence {
    /// Every replay key the journal holds in `[lower, upper]`.
    pub keys: Arc<std::collections::BTreeSet<String>>,
    /// The namespace's closed key range, compared bytewise.
    pub lower: String,
    pub upper: String,
    /// The refusal a write to an unrecorded key in the range meets.
    pub refusal: RuntimeEffectControllerError,
}

impl RecordedKeyFence {
    fn refuses(&self, key: &str) -> Option<RuntimeEffectControllerError> {
        let judged = self.lower.as_str() <= key && key <= self.upper.as_str();
        (judged && !self.keys.contains(key)).then(|| {
            let mut refusal = self.refusal.clone();
            refusal.message = format!(
                "{} (it wrote `{key}`, which the journal does not hold)",
                refusal.message
            );
            refusal
        })
    }
}

/// The run namespace a command the journal holds nothing for is refused
/// writes in, while the journal still holds entries beyond it (FIG-3586,
/// FIG-3680). A write that names no key cannot be placed, so it is refused
/// too; a keyed write outside the namespace is the host's to judge and passes.
#[derive(Clone, Debug)]
pub struct RefusedWriteRange {
    /// The namespace's closed key range, compared bytewise.
    pub lower: String,
    pub upper: String,
    /// The refusal a write in the range meets.
    pub refusal: RuntimeEffectControllerError,
}

impl RefusedWriteRange {
    fn refuses(&self, key: Option<&str>) -> Option<RuntimeEffectControllerError> {
        let judged = key.is_none_or(|key| self.lower.as_str() <= key && key <= self.upper.as_str());
        judged.then(|| self.refusal.clone())
    }
}

/// The run namespace a served-only command's effects are judged in, and the
/// refusal an effect in it meets when its engine would run it live
/// (FIG-3587, FIG-3719). Effects outside the namespace — a result's
/// presentation, a nested process's own journal — are the host's
/// deterministic work and pass.
#[derive(Clone, Debug)]
pub struct ServedOnlyRange {
    /// The namespace's closed key range, compared bytewise.
    pub lower: String,
    pub upper: String,
    /// The refusal a live effect in the range meets.
    pub refusal: RuntimeEffectControllerError,
}

impl ServedOnlyRange {
    /// Every key the guarded controller issues: a refused recorded lease
    /// permits served replay without admitting any fresh effect.
    pub fn every_key(refusal: RuntimeEffectControllerError) -> Self {
        Self {
            lower: String::new(),
            upper: char::MAX.to_string(),
            refusal,
        }
    }

    pub(crate) fn judges(&self, key: &str) -> bool {
        self.lower.as_str() <= key && key <= self.upper.as_str()
    }
}

impl CommandJournalGuard {
    fn with(refusal: Option<RefusedWriteRange>, fence: Option<RecordedKeyFence>) -> Self {
        Self {
            refusal,
            fence,
            served_only: None,
            touched: std::sync::atomic::AtomicBool::new(false),
            tripped: std::sync::Mutex::new(None),
        }
    }

    /// A guard that admits every write and remembers whether one was made.
    pub fn open() -> Self {
        Self::with(None, None)
    }

    /// A guard that refuses the command's first write in `range` with its
    /// refusal.
    pub fn refusing(range: RefusedWriteRange) -> Self {
        Self::with(Some(range), None)
    }

    /// A guard that admits writes to the keys `fence` holds and refuses any
    /// other key in its range.
    pub fn fenced(fence: RecordedKeyFence) -> Self {
        Self::with(None, Some(fence))
    }

    /// This guard, also serving its command only from its journal: every
    /// dispatching effect it issues hands `refusal` to its engine, which
    /// serves a recorded outcome and refuses one it would run live
    /// (FIG-3587, FIG-3719).
    #[must_use]
    pub fn served_only(mut self, range: ServedOnlyRange) -> Self {
        self.served_only = Some(range);
        self
    }

    /// Records that an engine refused one of this command's effects with its
    /// served-only refusal, so the run stops on it however the effect's
    /// caller shaped the error.
    pub(crate) fn trip(&self, refusal: &RuntimeEffectControllerError) {
        self.tripped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(|| refusal.clone());
    }

    /// Whether the command asked to write the journal.
    pub fn touched(&self) -> bool {
        self.touched.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The refusal this guard handed a write, once one was refused: the
    /// command reached a write the journal does not hold where it was
    /// issued. However the refused write's caller shaped the error — a tool
    /// call answers the program with a failure — the run stops on it.
    pub fn tripped(&self) -> Option<RuntimeEffectControllerError> {
        self.tripped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Asks to write the journal under this command, at `key` when the
    /// write names one.
    ///
    /// Once any write under the command was refused, every later one is
    /// refused with the same refusal before it reaches the engine: the first
    /// refusal is the last thing the command's attempt asks of the engine
    /// (FIG-3719, FIG-3725).
    pub fn admit(&self, key: Option<&str>) -> Result<(), RuntimeEffectControllerError> {
        self.touched
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(tripped) = self.tripped() {
            return Err(tripped);
        }
        let refusal = self
            .refusal
            .as_ref()
            .and_then(|range| range.refuses(key))
            .or_else(|| {
                self.fence
                    .as_ref()
                    .zip(key)
                    .and_then(|(fence, key)| fence.refuses(key))
            });
        match refusal {
            Some(refusal) => {
                self.tripped
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_or_insert_with(|| refusal.clone());
                Err(refusal)
            }
            None => Ok(()),
        }
    }
}
