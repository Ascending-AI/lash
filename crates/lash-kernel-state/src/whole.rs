//! A parked run as one whole binary structure (FIG-5887 §3, §10): the body
//! of node kind 16, KernelWhole.
//!
//! [`KernelWhole`] is [`ParkedRun`] with three substitutions, and nothing
//! else changed:
//!
//! - the run pins its helper release by the reference of the exact release
//!   manifest it was written under, and its owner by an OwnerBinding
//!   reference. The library functions it calls are its document's manifest,
//!   which a restore reads from the document rather than from the state;
//! - a wait holds its wait number, its effect identity, whether it is an
//!   effect or a sleep, and how far it has come: never its arguments or its
//!   sleep's length. Those were frozen once, in the admission the embedder
//!   recorded when the park handed the wait out, and a restore resolves the
//!   wait's request from that admission under the same execution;
//! - a committed outcome is a reference to a FrozenValue node, so equal
//!   outcomes are one node.
//!
//! Every export writes the whole structure: there is no baseline, no page
//! and no dependency on an earlier export. Mutable objects keep their
//! run-local identities, which preserves aliasing and cycles. Text and
//! bytes are written whole; the graph layer places a large one in shared
//! leaves.
//!
//! A wait still [`PerformState::Requested`] has never been handed out and so
//! has no admission: a whole state is published only at a park, and
//! [`KernelWhole::capture`] refuses one.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::graph_tokens::{
    ChildRef, CodecVariant, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
};
use lash_kernel_doc::{
    Datum, DocumentId, EffectIdentity, EffectName, FrozenDatum, FunctionId, Name, Object, ObjectId,
    TaskId, TaskIdentity, Value,
};

use crate::schema::{
    Binding, Bound, Call, EffectOutcome, Ended, Entered, Finally, Held, Incoming, ListJoin, Loop,
    Occurrence, ParkedCall, ParkedRun, ParkedTask, Pause, Perform, PerformState, ReadyQueue,
    Request, Run, SleepOutcome, Task, TaskState, WaitId,
};

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(problem: &str) -> TokenError {
    TokenError::Invalid(problem.to_owned())
}

/// Reads a sum header and checks the variant's field count with `arity`.
pub(crate) fn variant(
    input: &mut TokenReader<'_>,
    arity: impl FnOnce(u32) -> Option<u64>,
) -> Result<u32, TokenError> {
    let (variant, fields) = input.sum()?;
    let expected = arity(variant).ok_or_else(|| unsupported(variant))?;
    TokenReader::expect_fields(fields, expected)?;
    Ok(variant)
}

/// Kind 16's body: a parked run whole.
#[derive(Clone, Debug, PartialEq)]
pub struct KernelWhole {
    /// The kernel version the run executes under.
    pub kernel: u32,
    /// The document the run executes; its semantic identity is checked
    /// against the document a restore is given.
    pub document: DocumentId,
    /// The ReleaseManifest the run pins.
    pub release: ChildRef,
    /// The OwnerBinding of the execution the state belongs to.
    pub owner: ChildRef,
    pub charged: u64,
    pub objects_allocated: u64,
    pub waits_issued: u64,
    pub ready: ReadyQueue,
    pub withdrawn: BTreeSet<WaitId>,
    pub unreported: Vec<WaitId>,
    /// The effects the run performs as reads (`K-EFF-012`).
    pub reads: BTreeSet<EffectName>,
    pub session: BTreeMap<Name, Value>,
    pub tasks: Vec<WholeTask>,
    pub objects: BTreeMap<ObjectId, Object>,
}

/// A task as a whole state holds it: [`Task`] and its active calls, with a
/// wait as [`WholeWait`].
#[derive(Clone, Debug, PartialEq)]
pub struct WholeTask {
    pub identity: TaskIdentity,
    pub state: WholeTaskState,
    pub joiners: Vec<TaskId>,
    pub observed: bool,
    pub passed: bool,
    pub failed: bool,
    pub occurrences: Vec<Occurrence>,
    pub calls: Vec<ParkedCall>,
}

/// [`TaskState`], its variant numbers included, with a wait as
/// [`WholeWait`].
#[derive(Clone, Debug, PartialEq)]
pub enum WholeTaskState {
    Ready,
    Resuming(Incoming),
    Performing(WholeWait),
    Joining(TaskId),
    JoiningMany(ListJoin),
    Ended(Ended),
}

/// A wait as a whole state holds it: no request body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeWait {
    pub wait: WaitId,
    pub identity: EffectIdentity,
    pub kind: WaitKind,
    pub state: WaitState,
}

/// Whether a wait is a `perform` or a `sleep`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    Effect = 0,
    Sleep = 1,
}

/// [`PerformState`] with a committed outcome by reference. A requested
/// wait is never published, so it has no state here; the numbers are
/// [`PerformState`]'s, and its `Requested` (0) is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitState {
    Admitted,
    Committed(CommittedOutcome),
}

/// A committed outcome: an effect's result or failure as a FrozenValue
/// reference, or a sleep's elapse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommittedOutcome {
    Completed(ChildRef),
    Failed(ChildRef),
    Elapsed,
}

/// A wait's request as the admission the embedder recorded holds it: what a
/// restore rebuilds the in-memory request from.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmittedRequest {
    Effect {
        effect: EffectName,
        args: Vec<Datum>,
    },
    Sleep {
        duration: Pause,
    },
}

/// One admitted wait, keyed by its effect identity: what a parent hands a
/// worker beside a whole state to resume it.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedWait {
    pub identity: EffectIdentity,
    pub request: AdmittedRequest,
}

/// Why a whole state was not written or not resumed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WholeError<E> {
    /// A wait the run requested has not been handed out: it has no
    /// admission, and the state is not at a park.
    #[error("wait {0:?} has not been handed out; a whole state is written only at a park")]
    Unpublished(WaitId),
    /// No admission under this execution holds a parked wait's request.
    #[error("no admission holds the request of wait {0:?}")]
    AdmissionMissing(Box<EffectIdentity>),
    /// The admission holds a request of another kind than the wait.
    #[error("the admission of wait {0:?} is not the kind of the wait")]
    AdmissionMismatch(Box<EffectIdentity>),
    /// Staging or reading a FrozenValue failed.
    #[error("a frozen value failed: {0}")]
    Frozen(E),
}

impl KernelWhole {
    /// The whole state of `parked`, which pins `release` and belongs to
    /// `owner`. Each committed outcome is staged once with `freeze`.
    ///
    /// # Errors
    ///
    /// [`WholeError::Unpublished`] for a wait not yet handed out, and the
    /// first error `freeze` returns.
    pub fn capture<E>(
        parked: &ParkedRun,
        release: ChildRef,
        owner: ChildRef,
        mut freeze: impl FnMut(&FrozenDatum) -> Result<ChildRef, E>,
    ) -> Result<Self, WholeError<E>> {
        let run = &parked.run;
        let mut tasks = Vec::with_capacity(parked.tasks.len());
        for task in &parked.tasks {
            let handle = &task.handle;
            let state = match &handle.state {
                TaskState::Ready => WholeTaskState::Ready,
                TaskState::Resuming(incoming) => WholeTaskState::Resuming(incoming.clone()),
                TaskState::Performing(perform) => {
                    WholeTaskState::Performing(capture_wait(perform, &mut freeze)?)
                }
                TaskState::Joining(joined) => WholeTaskState::Joining(*joined),
                TaskState::JoiningMany(join) => WholeTaskState::JoiningMany(join.clone()),
                TaskState::Ended(ended) => WholeTaskState::Ended(ended.clone()),
            };
            tasks.push(WholeTask {
                identity: handle.identity.clone(),
                state,
                joiners: handle.joiners.clone(),
                observed: handle.observed,
                passed: handle.passed,
                failed: handle.failed,
                occurrences: handle.occurrences.clone(),
                calls: task.calls.clone(),
            });
        }
        Ok(Self {
            kernel: run.kernel,
            document: run.document,
            release,
            owner,
            charged: run.charged,
            objects_allocated: run.objects_allocated,
            waits_issued: run.waits_issued,
            ready: run.ready.clone(),
            withdrawn: run.withdrawn.clone(),
            unreported: run.unreported.clone(),
            reads: run.reads.clone(),
            session: parked.session.clone(),
            tasks,
            objects: parked.objects.clone(),
        })
    }

    /// The parked run this whole state is, for the document whose manifest
    /// lists `functions`. Each wait's request is resolved with `admitted`
    /// and each committed outcome read with `thaw`.
    ///
    /// # Errors
    ///
    /// [`WholeError::AdmissionMissing`] or [`WholeError::AdmissionMismatch`]
    /// for a wait no admission matches, and the first error `thaw` returns.
    pub fn restore<E>(
        self,
        functions: BTreeSet<FunctionId>,
        mut thaw: impl FnMut(ChildRef) -> Result<Datum, E>,
        mut admitted: impl FnMut(&EffectIdentity) -> Option<AdmittedRequest>,
    ) -> Result<ParkedRun, WholeError<E>> {
        let mut tasks = Vec::with_capacity(self.tasks.len());
        for task in self.tasks {
            let state = match task.state {
                WholeTaskState::Ready => TaskState::Ready,
                WholeTaskState::Resuming(incoming) => TaskState::Resuming(incoming),
                WholeTaskState::Performing(wait) => {
                    TaskState::Performing(Box::new(restore_wait(wait, &mut thaw, &mut admitted)?))
                }
                WholeTaskState::Joining(joined) => TaskState::Joining(joined),
                WholeTaskState::JoiningMany(join) => TaskState::JoiningMany(join),
                WholeTaskState::Ended(ended) => TaskState::Ended(ended),
            };
            tasks.push(ParkedTask {
                handle: Task {
                    identity: task.identity,
                    state,
                    joiners: task.joiners,
                    observed: task.observed,
                    passed: task.passed,
                    failed: task.failed,
                    occurrences: task.occurrences,
                },
                calls: task.calls,
            });
        }
        Ok(ParkedRun {
            run: Run {
                kernel: self.kernel,
                document: self.document,
                functions,
                charged: self.charged,
                objects_allocated: self.objects_allocated,
                waits_issued: self.waits_issued,
                ready: self.ready,
                withdrawn: self.withdrawn,
                unreported: self.unreported,
                reads: self.reads,
            },
            session: self.session,
            tasks,
            objects: self.objects,
        })
    }

    /// Every wait the state holds, in task order.
    pub fn waits(&self) -> impl Iterator<Item = &WholeWait> {
        self.tasks.iter().filter_map(|task| match &task.state {
            WholeTaskState::Performing(wait) => Some(wait),
            _ => None,
        })
    }
}

fn capture_wait<E>(
    perform: &Perform,
    freeze: &mut impl FnMut(&FrozenDatum) -> Result<ChildRef, E>,
) -> Result<WholeWait, WholeError<E>> {
    let unpublished = || WholeError::Unpublished(perform.wait);
    let (identity, kind, state) = match &perform.request {
        Request::Effect {
            identity, state, ..
        } => {
            let state = match state {
                PerformState::Requested => return Err(unpublished()),
                PerformState::Admitted => WaitState::Admitted,
                PerformState::Committed(EffectOutcome::Completed(datum)) => {
                    WaitState::Committed(CommittedOutcome::Completed(
                        freeze(&FrozenDatum(datum.clone())).map_err(WholeError::Frozen)?,
                    ))
                }
                PerformState::Committed(EffectOutcome::Failed(error)) => {
                    WaitState::Committed(CommittedOutcome::Failed(
                        freeze(&FrozenDatum(Datum::Error(Box::new(error.clone()))))
                            .map_err(WholeError::Frozen)?,
                    ))
                }
            };
            (identity, WaitKind::Effect, state)
        }
        Request::Sleep {
            identity, state, ..
        } => {
            let state = match state {
                PerformState::Requested => return Err(unpublished()),
                PerformState::Admitted => WaitState::Admitted,
                PerformState::Committed(SleepOutcome::Elapsed) => {
                    WaitState::Committed(CommittedOutcome::Elapsed)
                }
            };
            (identity, WaitKind::Sleep, state)
        }
    };
    Ok(WholeWait {
        wait: perform.wait,
        identity: identity.clone(),
        kind,
        state,
    })
}

fn restore_wait<E>(
    wait: WholeWait,
    thaw: &mut impl FnMut(ChildRef) -> Result<Datum, E>,
    admitted: &mut impl FnMut(&EffectIdentity) -> Option<AdmittedRequest>,
) -> Result<Perform, WholeError<E>> {
    let request = admitted(&wait.identity)
        .ok_or_else(|| WholeError::AdmissionMissing(Box::new(wait.identity.clone())))?;
    let mismatch = || WholeError::AdmissionMismatch(Box::new(wait.identity.clone()));
    let request = match (request, wait.kind) {
        (AdmittedRequest::Effect { effect, args }, WaitKind::Effect) => Request::Effect {
            identity: wait.identity.clone(),
            effect,
            args,
            state: match wait.state {
                WaitState::Admitted => PerformState::Admitted,
                WaitState::Committed(CommittedOutcome::Completed(outcome)) => {
                    PerformState::Committed(EffectOutcome::Completed(
                        thaw(outcome).map_err(WholeError::Frozen)?,
                    ))
                }
                WaitState::Committed(CommittedOutcome::Failed(outcome)) => {
                    match thaw(outcome).map_err(WholeError::Frozen)? {
                        Datum::Error(error) => {
                            PerformState::Committed(EffectOutcome::Failed(*error))
                        }
                        _ => return Err(mismatch()),
                    }
                }
                WaitState::Committed(CommittedOutcome::Elapsed) => return Err(mismatch()),
            },
        },
        (AdmittedRequest::Sleep { duration }, WaitKind::Sleep) => Request::Sleep {
            identity: wait.identity.clone(),
            duration,
            state: match wait.state {
                WaitState::Admitted => PerformState::Admitted,
                WaitState::Committed(CommittedOutcome::Elapsed) => {
                    PerformState::Committed(SleepOutcome::Elapsed)
                }
                WaitState::Committed(_) => return Err(mismatch()),
            },
        },
        _ => return Err(mismatch()),
    };
    Ok(Perform {
        wait: wait.wait,
        request,
    })
}

// The codec.

impl TokenCodec for WaitId {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned().map(Self)
    }
}

impl TokenCodec for ReadyQueue {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.len() as u64);
        for task in self.iter() {
            task.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::try_from(Vec::<TaskId>::decode(input)?).map_err(invalid)
    }
}

impl TokenCodec for Pause {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.unsigned(self.seconds);
        out.unsigned(u64::from(self.nanoseconds));
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let pause = Self {
            seconds: input.unsigned()?,
            nanoseconds: input.unsigned_u32()?,
        };
        pause
            .duration()
            .map(|_| pause)
            .ok_or_else(|| invalid("a pause's nanoseconds reach a second"))
    }
}

impl TokenCodec for Occurrence {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.site.encode(out);
        out.unsigned(self.count);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: lash_kernel_doc::Site::decode(input)?,
            count: input.unsigned()?,
        })
    }
}

impl TokenCodec for Incoming {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Value(value) | Self::Raise(value) => value.encode(out),
            Self::Joined(task) => task.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 2).then_some(1))? {
                0 => Self::Value(Value::decode(input)?),
                1 => Self::Raise(Value::decode(input)?),
                _ => Self::Joined(TaskId::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Ended {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Returned(value) | Self::Raised(value) => value.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Returned(Value::decode(input)?),
                _ => Self::Raised(Value::decode(input)?),
            },
        )
    }
}

impl TokenCodec for ListJoin {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.mode.encode(out);
        self.members.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            mode: lash_kernel_doc::JoinMode::decode(input)?,
            members: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Bound {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Value(value) => value.encode(out),
            Self::Shared(cell) => cell.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Value(Value::decode(input)?),
                _ => Self::Shared(ObjectId::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Binding {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.name.encode(out);
        self.declared.encode(out);
        self.value.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: Name::decode(input)?,
            declared: lash_kernel_doc::Site::decode(input)?,
            value: Bound::decode(input)?,
        })
    }
}

impl TokenCodec for Loop {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.site.encode(out);
        out.unsigned(self.started);
        self.position.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            site: lash_kernel_doc::Site::decode(input)?,
            started: input.unsigned()?,
            position: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Entered {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 4).then_some(0))? {
                0 => Self::Normal,
                1 => Self::Throw,
                2 => Self::Return,
                3 => Self::Break,
                _ => Self::Continue,
            },
        )
    }
}

impl TokenCodec for Finally {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.site.encode(out);
        self.entered.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: lash_kernel_doc::Site::decode(input)?,
            entered: Entered::decode(input)?,
        })
    }
}

impl TokenCodec for Held {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.iterated.encode(out);
        self.departing.encode(out);
        self.arguments.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            iterated: Vec::decode(input)?,
            departing: Vec::decode(input)?,
            arguments: Option::decode(input)?,
        })
    }
}

impl TokenCodec for Call {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.statement.encode(out);
        self.bindings.encode(out);
        self.loops.encode(out);
        self.finally.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            statement: lash_kernel_doc::Site::decode(input)?,
            bindings: Vec::decode(input)?,
            loops: Vec::decode(input)?,
            finally: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for ParkedCall {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.call.encode(out);
        self.held.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            call: Call::decode(input)?,
            held: Held::decode(input)?,
        })
    }
}

impl TokenCodec for WaitKind {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(*self as u32, 0);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(0))? {
                0 => Self::Effect,
                _ => Self::Sleep,
            },
        )
    }
}

/// [`WaitState`] takes [`PerformState`]'s numbers; [`CommittedOutcome`]
/// takes [`EffectOutcome`]'s, with the sleep's elapse after them.
impl TokenCodec for WaitState {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Admitted => out.sum(1, 0),
            Self::Committed(outcome) => {
                out.sum(2, 1);
                match outcome {
                    CommittedOutcome::Completed(frozen) => {
                        out.sum(0, 1);
                        frozen.encode(out);
                    }
                    CommittedOutcome::Failed(frozen) => {
                        out.sum(1, 1);
                        frozen.encode(out);
                    }
                    CommittedOutcome::Elapsed => out.sum(2, 0),
                }
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                1 => Some(0),
                2 => Some(1),
                _ => None,
            })? {
                1 => Self::Admitted,
                _ => Self::Committed(
                    match variant(input, |variant| match variant {
                        0 | 1 => Some(1),
                        2 => Some(0),
                        _ => None,
                    })? {
                        0 => CommittedOutcome::Completed(ChildRef::decode(input)?),
                        1 => CommittedOutcome::Failed(ChildRef::decode(input)?),
                        _ => CommittedOutcome::Elapsed,
                    },
                ),
            },
        )
    }
}

impl TokenCodec for WholeWait {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.wait.encode(out);
        self.identity.encode(out);
        self.kind.encode(out);
        self.state.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        let wait = Self {
            wait: WaitId::decode(input)?,
            identity: EffectIdentity::decode(input)?,
            kind: WaitKind::decode(input)?,
            state: WaitState::decode(input)?,
        };
        let consistent = match (&wait.kind, &wait.state) {
            (WaitKind::Effect, WaitState::Committed(CommittedOutcome::Elapsed)) => false,
            (WaitKind::Sleep, WaitState::Committed(outcome)) => {
                *outcome == CommittedOutcome::Elapsed
            }
            _ => true,
        };
        if consistent {
            Ok(wait)
        } else {
            Err(invalid("a wait's outcome is not its kind's"))
        }
    }
}

/// [`WholeTaskState`] takes [`TaskState`]'s numbers.
impl TokenCodec for WholeTaskState {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Ready => out.sum(0, 0),
            Self::Resuming(incoming) => {
                out.sum(1, 1);
                incoming.encode(out);
            }
            Self::Performing(wait) => {
                out.sum(2, 1);
                wait.encode(out);
            }
            Self::Joining(task) => {
                out.sum(3, 1);
                task.encode(out);
            }
            Self::JoiningMany(join) => {
                out.sum(4, 1);
                join.encode(out);
            }
            Self::Ended(ended) => {
                out.sum(5, 1);
                ended.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(0),
                1..=5 => Some(1),
                _ => None,
            })? {
                0 => Self::Ready,
                1 => Self::Resuming(Incoming::decode(input)?),
                2 => Self::Performing(WholeWait::decode(input)?),
                3 => Self::Joining(TaskId::decode(input)?),
                4 => Self::JoiningMany(ListJoin::decode(input)?),
                _ => Self::Ended(Ended::decode(input)?),
            },
        )
    }
}

impl TokenCodec for WholeTask {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        self.identity.encode(out);
        self.state.encode(out);
        self.joiners.encode(out);
        out.bool(self.observed);
        out.bool(self.passed);
        out.bool(self.failed);
        self.occurrences.encode(out);
        self.calls.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        Ok(Self {
            identity: TaskIdentity::decode(input)?,
            state: WholeTaskState::decode(input)?,
            joiners: Vec::decode(input)?,
            observed: input.bool()?,
            passed: input.bool()?,
            failed: input.bool()?,
            occurrences: Vec::decode(input)?,
            calls: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for KernelWhole {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(14);
        out.unsigned(u64::from(self.kernel));
        self.document.encode(out);
        self.release.encode(out);
        self.owner.encode(out);
        out.unsigned(self.charged);
        out.unsigned(self.objects_allocated);
        out.unsigned(self.waits_issued);
        self.ready.encode(out);
        self.withdrawn.encode(out);
        self.unreported.encode(out);
        self.reads.encode(out);
        self.session.encode(out);
        self.tasks.encode(out);
        self.objects.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(14)?;
        let whole = Self {
            kernel: input.unsigned_u32()?,
            document: DocumentId::decode(input)?,
            release: ChildRef::decode(input)?,
            owner: ChildRef::decode(input)?,
            charged: input.unsigned()?,
            objects_allocated: input.unsigned()?,
            waits_issued: input.unsigned()?,
            ready: ReadyQueue::decode(input)?,
            withdrawn: BTreeSet::decode(input)?,
            unreported: Vec::decode(input)?,
            reads: BTreeSet::decode(input)?,
            session: BTreeMap::decode(input)?,
            tasks: Vec::decode(input)?,
            objects: BTreeMap::decode(input)?,
        };
        // Occurrences are in site order (`K-SITE-002`); a wait number is
        // one the run has issued; an object identity is one it allocated.
        for task in &whole.tasks {
            let sites: Vec<_> = task
                .occurrences
                .iter()
                .map(|occurrence| &occurrence.site)
                .collect();
            check_ascending(&sites)?;
        }
        let issued = |wait: &WaitId| wait.0 < whole.waits_issued;
        if !whole.waits().all(|wait| issued(&wait.wait))
            || !whole.withdrawn.iter().all(issued)
            || !whole.unreported.iter().all(issued)
            || whole
                .objects
                .last_key_value()
                .is_some_and(|(id, _)| id.0 >= whole.objects_allocated)
        {
            return Err(invalid(
                "a wait or an object is numbered past what the run has issued",
            ));
        }
        Ok(whole)
    }
}

impl TokenCodec for AdmittedRequest {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Effect { effect, args } => {
                out.sum(WaitKind::Effect as u32, 2);
                effect.encode(out);
                args.encode(out);
            }
            Self::Sleep { duration } => {
                out.sum(WaitKind::Sleep as u32, 1);
                duration.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(2),
                1 => Some(1),
                _ => None,
            })? {
                0 => Self::Effect {
                    effect: EffectName::decode(input)?,
                    args: Vec::decode(input)?,
                },
                _ => Self::Sleep {
                    duration: Pause::decode(input)?,
                },
            },
        )
    }
}

impl TokenCodec for AdmittedWait {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.identity.encode(out);
        self.request.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            identity: EffectIdentity::decode(input)?,
            request: AdmittedRequest::decode(input)?,
        })
    }
}
