//! `ScopedEffectController`: a controller paired with the execution scope it
//! serves, plus the `ScopeBoundController` re-scoping seam.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

/// A controller built for one scope that can build itself for another: what
/// an engine-side controller that must know the scope of every effect it runs
/// hands to [`ScopedEffectController::owned`], so the runtime's rescoping (a
/// turn under a process, a child under its parent) keeps the scope exact.
pub trait ScopeBoundController: RuntimeEffectController {
    /// This controller, bound to `admitted` instead; it lives as long as this one does.
    fn for_scope<'a>(&self, admitted: AdmittedScope) -> Arc<dyn ScopeBoundController + 'a>
    where
        Self: 'a;
}

pub(in crate::runtime::effect::executor) enum ScopedEffectControllerInner<'run> {
    Borrowed(&'run dyn RuntimeEffectController),
    Shared(Arc<dyn RuntimeEffectController>),
    /// A controller built for this scope alone and living no longer than the
    /// borrow it wraps: an engine-side controller that records the scope's
    /// executing effects under the scope (FIG-2499).
    Owned(Arc<dyn ScopeBoundController + 'run>),
}

impl Clone for ScopedEffectControllerInner<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(controller) => Self::Borrowed(*controller),
            Self::Shared(controller) => Self::Shared(Arc::clone(controller)),
            Self::Owned(controller) => Self::Owned(Arc::clone(controller)),
        }
    }
}

/// Scoped low-level controller plus the admitted execution scope it is
/// serving.
#[derive(Clone)]
pub struct ScopedEffectController<'run> {
    pub(in crate::runtime::effect::executor) controller: ScopedEffectControllerInner<'run>,
    /// The scope this controller serves plus, when it is a process, the
    /// store-minted incarnation it was admitted under.
    ///
    /// [`ExecutionScope::Process`] carries the reusable process *name* and
    /// nothing else, which ADR 0099 §1 names as the gap: the opener that owns
    /// durable work is the name bound to one incarnation, and "a retired or
    /// mismatched incarnation is refused, never rebound to the current process
    /// carrying the same name". Keeping the pair in one [`AdmittedScope`]
    /// means a process-scoped controller always carries its incarnation —
    /// the half-admitted shape is unconstructible, so any execution running
    /// under this controller can name its opener
    /// ([`Self::admitted_process`]).
    pub(in crate::runtime::effect::executor) admitted: AdmittedScope,
    /// The journal guard of the replayed language command this controller
    /// serves, when it serves one (FIG-3586). Every journal write made
    /// through this controller asks it first.
    pub(in crate::runtime::effect::executor) journal_guard: Option<Arc<CommandJournalGuard>>,
}

/// A replayed language command's say over the journal writes made under it
/// (FIG-3586).
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
///   made it with nothing under the namespace (an orchestrating body that
///   issued no nested effect, a call settled in preparation), and the host
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
/// ([`RuntimeEffectLocalExecutor::served_only_refusal`]). The engine serves an
/// outcome its journal holds; one it holds none for would reach the drifted
/// tool live, so the engine refuses it with the drift instead, running and
/// recording nothing (FIG-3719). A wait on an external completion dispatches
/// nothing, so it carries no refusal.
#[derive(Debug)]
pub struct CommandJournalGuard {
    refusal: Option<RefusedWriteRange>,
    fence: Option<RecordedKeyFence>,
    served_only: Option<ServedOnlyRange>,
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
    fn judges(&self, key: &str) -> bool {
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
    pub fn admit(&self, key: Option<&str>) -> Result<(), RuntimeEffectControllerError> {
        self.touched
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

impl<'run> ScopedEffectController<'run> {
    /// This controller serving one replayed language command: every journal
    /// write made through it asks `guard` first (FIG-3586).
    #[must_use]
    pub fn with_journal_guard(mut self, guard: Arc<CommandJournalGuard>) -> Self {
        self.journal_guard = Some(guard);
        self
    }

    /// The command guard this controller serves under, when it serves one.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn journal_guard(&self) -> Option<Arc<CommandJournalGuard>> {
        self.journal_guard.clone()
    }

    /// Asks this controller's command guard, when it has one, to admit a
    /// journal write. Every path that writes the journal under a scoped
    /// controller without going through [`Self::execute_effect`] — a group
    /// open, a proxied process command — asks this first.
    pub fn admit_journal_write(&self) -> Result<(), RuntimeEffectControllerError> {
        self.admit_journal_write_at(None)
    }

    /// [`Self::admit_journal_write`] for a write at `key`.
    pub fn admit_journal_write_at(
        &self,
        key: Option<&str>,
    ) -> Result<(), RuntimeEffectControllerError> {
        match &self.journal_guard {
            Some(guard) => guard.admit(key),
            None => Ok(()),
        }
    }

    pub fn execution_scope(&self) -> &ExecutionScope {
        self.admitted.scope()
    }

    pub fn admitted_scope(&self) -> &AdmittedScope {
        &self.admitted
    }

    /// The process incarnation this controller's scope was admitted under.
    ///
    /// `None` for every non-process scope. A `Process` scope always carries
    /// `Some`: [`AdmittedScope`] makes the unpinned shape unconstructible, so
    /// `None` here is "not a process", never "not yet bound" (ADR 0099 §1).
    pub fn admitted_process(&self) -> Option<&crate::ProcessRef> {
        self.admitted.process_ref()
    }

    pub fn borrowed(
        controller: &'run dyn RuntimeEffectController,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Borrowed(controller),
            admitted,
            journal_guard: None,
        })
    }

    /// Validates the admitted scope and binds an owned controller for
    /// effect-host implementors that must move the scoped host across an
    /// asynchronous boundary.
    pub fn shared(
        controller: Arc<dyn RuntimeEffectController>,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Shared(controller),
            admitted,
            journal_guard: None,
        })
    }

    /// Validates the admitted scope and binds a controller built for that
    /// scope and bounded by the borrow it wraps, for effect-host implementors
    /// whose engine-side controller must know the scope of every effect it
    /// runs.
    pub fn owned(
        controller: Arc<dyn ScopeBoundController + 'run>,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Owned(controller),
            admitted,
            journal_guard: None,
        })
    }

    /// Exposes controller to effect-host implementors while scoping and journaling durable effects.
    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => *controller,
            ScopedEffectControllerInner::Shared(controller) => controller.as_ref(),
            ScopedEffectControllerInner::Owned(controller) => controller.as_ref(),
        }
    }

    fn validate_envelope_scope(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<(), RuntimeEffectControllerError> {
        envelope
            .invocation
            .validate_execution_scope(self.admitted.scope())
    }

    /// Executes an effect only after proving that its address belongs to this
    /// controller's admitted scope.
    pub async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.validate_envelope_scope(&envelope)?;
        let mut local_executor = local_executor;
        if let Some(guard) = &self.journal_guard {
            guard.admit(Some(envelope.invocation.replay_key()))?;
            // A wait on an external completion dispatches nothing
            // (FIG-3587): only a dispatching effect is served only.
            let dispatches = !matches!(
                envelope.command,
                crate::RuntimeEffectCommand::AwaitEvent { .. }
                    | crate::RuntimeEffectCommand::PeekAwaitEvent { .. }
            );
            let key = envelope.invocation.replay_key();
            if let Some(range) = guard
                .served_only
                .as_ref()
                .filter(|range| dispatches && range.judges(key))
            {
                let refusal = range.refusal.clone();
                local_executor =
                    local_executor.serving_only_from_journal(refusal, Arc::clone(guard));
            }
        }
        self.controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    /// Exposes scope id to effect-host implementors while scoping and journaling durable effects.
    pub fn scope_id(&self) -> &str {
        self.admitted.scope().id()
    }

    /// Exposes turn id to effect-host implementors while scoping and journaling durable
    /// effects.
    pub fn turn_id(&self) -> Option<&TurnId> {
        self.admitted.scope().turn_id()
    }

    /// The complete turn-cancel trio for a wait built directly against this
    /// scope, for the wait sites that have no `RuntimeExecutionContext` to ask.
    /// The trio it yields is always observing: a scope alone cannot say whether
    /// the enclosing execution opted out.
    ///
    /// That is exact for the turn-driver and test-dispatch sites that call this
    /// producer: both only run work that observes turn cancellation. Process
    /// bodies instead build one unobserved trio at their execution boundary and
    /// carry it through retry sleeps and deferred-tool awaits whole.
    pub(crate) fn turn_cancel_wait(&self, cancellation: CancellationToken) -> TurnCancelWait {
        TurnCancelWait::observing(cancellation, self.admitted.scope().clone())
    }

    pub fn to_static(&self) -> Option<ScopedEffectController<'static>> {
        let ScopedEffectControllerInner::Shared(controller) = &self.controller else {
            return None;
        };
        Some(ScopedEffectController {
            controller: ScopedEffectControllerInner::Shared(Arc::clone(controller)),
            admitted: self.admitted.clone(),
            journal_guard: self.journal_guard.clone(),
        })
    }

    pub fn owned_controller(&self) -> Option<Arc<dyn RuntimeEffectController>> {
        match &self.controller {
            ScopedEffectControllerInner::Shared(controller) => Some(Arc::clone(controller)),
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                None
            }
        }
    }
}

pub mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`ScopedEffectController`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    #[async_trait::async_trait]
    pub trait ScopedEffectControllerFacadeOps {
        fn execution_scope(&self) -> &ExecutionScope;

        /// Borrowed controllers are proxied across the process task boundary; shared
        /// controllers can be passed through directly.
        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
    }

    #[async_trait::async_trait]
    impl ScopedEffectControllerFacadeOps for ScopedEffectController<'_> {
        fn execution_scope(&self) -> &ExecutionScope {
            self.admitted.scope()
        }

        #[expect(
            clippy::expect_used,
            reason = "the effect-task proxy owns the controller it lends"
        )]
        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            self.validate_envelope_scope(&envelope)?;
            let controller = self.controller();
            let (owned_controller, task_requests) = if let Some(owned) = self.owned_controller() {
                (owned, None)
            } else {
                let (proxy, requests) =
                    EffectTaskController::scoped(controller, self.admitted.clone())?;
                (
                    proxy
                        .owned_controller()
                        .expect("effect-task proxy owns its controller"),
                    Some(requests),
                )
            };
            let local_executor = local_executor.with_process_effect_controller(owned_controller);
            if let Some(task_requests) = task_requests {
                drive_effect_controller_task(
                    controller,
                    self.execution_scope().clone(),
                    envelope,
                    local_executor,
                    task_requests,
                )
                .await
            } else {
                self.execute_effect(envelope, local_executor).await
            }
        }
    }
}

#[cfg(test)]
mod admitted_scope_tests {
    use super::*;
    use crate::{AdmittedScopeError, ProcessIncarnation, ProcessRef};

    fn process_ref(name: &str, incarnation: u64) -> ProcessRef {
        ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )
    }

    /// Admission and rescope are the controller-construction half: no effect
    /// runs, so the controller behind the scope is one with no host.
    fn shared_controller() -> Arc<dyn RuntimeEffectController> {
        Arc::new(crate::testing::UnavailableEffectController)
    }

    /// A process scope alone cannot name its opener: the incarnation the
    /// admission authority bound is part of the controller's construction, so
    /// the half-admitted shape has no constructor to build (ADR 0099 §1).
    #[test]
    fn a_process_scoped_controller_carries_the_admitted_incarnation() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 4)),
        )
        .expect("process scope");

        assert_eq!(scoped.admitted_process(), Some(&process_ref("worker", 4)));
        assert_eq!(scoped.execution_scope(), &ExecutionScope::process("worker"));
    }

    /// The checked constructor refuses the two ways to lie about a process:
    /// no incarnation at all, and an incarnation of another process. Both are
    /// `AdmittedScope` construction errors — they never reach the controller.
    #[test]
    fn a_process_scope_must_name_its_own_incarnation() {
        assert!(matches!(
            AdmittedScope::new(ExecutionScope::process("worker"), None),
            Err(AdmittedScopeError::ProcessIncarnationMissing { .. })
        ));
        assert!(matches!(
            AdmittedScope::new(
                ExecutionScope::process("worker"),
                Some(process_ref("indexer", 1)),
            ),
            Err(AdmittedScopeError::ProcessPinMismatch { .. })
        ));
    }

    /// A turn scope has an opener of its own and never borrows a process's.
    #[test]
    fn a_turn_scope_refuses_an_admitted_incarnation() {
        assert!(matches!(
            AdmittedScope::new(
                ExecutionScope::turn("session-1", "turn-1"),
                Some(process_ref("worker", 1)),
            ),
            Err(AdmittedScopeError::NonProcessScopePinned { .. })
        ));
    }

    /// The child session turn a process backs is spawned onto its own task, so
    /// the binding has to survive the `'static` conversion that spawn requires.
    #[test]
    fn the_admitted_incarnation_survives_the_static_conversion() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 9)),
        )
        .expect("process scope");

        assert_eq!(
            scoped
                .to_static()
                .expect("a shared controller is static")
                .admitted_process(),
            Some(&process_ref("worker", 9))
        );
        assert_eq!(
            scoped
                .into_static()
                .map_err(|_| "a shared controller is static")
                .expect("static")
                .admitted_process(),
            Some(&process_ref("worker", 9))
        );
    }

    /// A rescope is a different opener, so it never inherits the incarnation of
    /// the scope it left.
    #[test]
    fn a_rescope_drops_the_admitted_incarnation() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 2)),
        )
        .expect("process scope");

        let rescoped = scoped
            .rescope(AdmittedScope::turn("session-1", "turn-1"))
            .expect("rescope onto a turn");

        assert!(rescoped.admitted_process().is_none());
    }

    /// A non-process controller has no pin a process target could match, so
    /// it can never rescope into a process controller: process admission only
    /// exists at construction.
    #[test]
    fn a_non_process_controller_cannot_rescope_into_a_process() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::turn("session-1", "turn-1"),
        )
        .expect("turn scope");

        let error = scoped
            .rescope(AdmittedScope::process(process_ref("worker", 3)))
            .err()
            .expect("a turn controller cannot become a process controller");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused
        );
    }
}
