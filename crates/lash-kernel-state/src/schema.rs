//! The parked-run schema (`docs/kernel/parked-state.md`).
//!
//! Every coordinate is a [`Site`] of the document (`K-SITE-001` to
//! `K-SITE-005`); every value is a kernel [`Value`], and every heap object
//! a kernel [`Object`] under the identity the run gave it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use lash_kernel_doc::{
    Datum, DocumentId, EffectIdentity, EffectName, ErrorDatum, FunctionId, JoinMode, Name, Object,
    ObjectId, Site, TaskId, TaskIdentity, Value,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One wait a machine handed out, within one run. Numbers are taken in
/// request order from 0.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WaitId(pub u64);

/// A parked run, whole: what it pins, its meters, its tasks and every
/// heap object they reach.
///
/// It is stored as one document, or as the parts [`ParkedRun::save`]
/// writes, of which a later save rewrites only those that changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParkedRun {
    pub run: Run,
    /// The session's bindings (`K-SES-001`). Empty for a run of an entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session: BTreeMap<Name, Value>,
    /// Every task the run has started, by handle number: `main` first.
    pub tasks: Vec<ParkedTask>,
    /// Every heap object a root reaches, by identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub objects: BTreeMap<ObjectId, Object>,
}

/// A task with its active calls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParkedTask {
    pub handle: Task,
    /// The task's active calls, the outermost first. Empty once it ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ParkedCall>,
}

/// An active call with the values its control state alone holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParkedCall {
    pub call: Call,
    #[serde(default, skip_serializing_if = "Held::is_empty")]
    pub held: Held,
}

/// What a parked run pins and what it has used: the part of the state
/// that belongs to no root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// The kernel version the run executes under (`K-VER-001`). It owns
    /// this schema.
    pub kernel: u32,
    /// The document the run executes (`K-ID-001`).
    pub document: DocumentId,
    /// Every library function the document's manifest lists. A function's
    /// identity covers its charge formula and its guard.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub functions: BTreeSet<FunctionId>,
    /// What the run has been charged, in charge units (`K-CHG-001`).
    pub charged: u64,
    /// How many heap objects the run has allocated: the identity its next
    /// object takes.
    pub objects_allocated: u64,
    /// How many waits the run has issued: the number its next wait takes.
    pub waits_issued: u64,
    /// The tasks that are ready, in the order they run (`K-TASK-003`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ready: Vec<TaskId>,
    /// Every wait the run withdrew after handing it out. An outcome that
    /// arrives for one is dropped (`K-MACH-004`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub withdrawn: BTreeSet<WaitId>,
    /// The withdrawn waits the next park reports (`K-MACH-003`), in the
    /// order they were withdrawn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreported: Vec<WaitId>,
}

/// A task's handle: who the task is, what it waits on or how it ended,
/// and who waits on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub identity: TaskIdentity,
    pub state: TaskState,
    /// The tasks waiting in a `join` on this handle, alone or in a list, in
    /// the order their joins began: its ending wakes them in this order
    /// (`K-TASK-008`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub joiners: Vec<TaskId>,
    /// A `join` on the handle raised the task's error (`K-TASK-016`).
    pub observed: bool,
    /// The task is or was a member of a list `join` that has returned or
    /// raised: its error counts as observed and the run's end cancels it
    /// (`K-TASK-016`, `K-TASK-018`).
    pub passed: bool,
    /// The task ended in an error. It outlives the error itself, which is
    /// dropped once no handle reaches the task.
    pub failed: bool,
    /// How many times the task has run each `spawn`, `perform` and
    /// `sleep`, by the action's site, in site order (`K-SITE-002`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub occurrences: Vec<Occurrence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Occurrence {
    pub site: Site,
    pub count: u64,
}

/// Where a task is in its life.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Ready: its pending statement runs next.
    Ready,
    /// Ready: its pending statement continues with this.
    Resuming(Incoming),
    /// In a `perform` or a `sleep`. The task is ready once the wait's
    /// outcome is committed, and waits until then.
    Performing(Box<Perform>),
    /// Waiting in a `join` on one handle.
    Joining(TaskId),
    /// Waiting in a `join` on a list.
    JoiningMany(ListJoin),
    Ended(Ended),
}

/// What a ready task's pending statement continues with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Incoming {
    /// The value of its action.
    Value(Value),
    /// A raise at its action: a list `join` that failed, a cancel.
    Raise(Value),
    /// The result of this task, which has ended (`K-TASK-009`).
    Joined(TaskId),
}

/// One wait: a `perform` or a `sleep` a task is in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Perform {
    pub wait: WaitId,
    pub request: Request,
    pub state: PerformState,
}

/// What a wait asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Effect {
        identity: EffectIdentity,
        effect: EffectName,
        /// The arguments, copied out of the run (`K-EFF-002`).
        args: Vec<Datum>,
    },
    Sleep {
        identity: EffectIdentity,
        duration: Pause,
    },
}

/// A sleep's length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub seconds: u64,
    /// Below one second.
    pub nanoseconds: u32,
}

impl From<Duration> for Pause {
    fn from(duration: Duration) -> Self {
        Self {
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        }
    }
}

impl Pause {
    /// The duration, or `None` when the nanoseconds reach a second.
    pub fn duration(self) -> Option<Duration> {
        (self.nanoseconds < 1_000_000_000).then(|| Duration::new(self.seconds, self.nanoseconds))
    }
}

/// How far a wait has come.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PerformState {
    /// Requested since the last park. The embedder has not seen it: the
    /// next park hands it out.
    Requested,
    /// Handed out at a park, which the embedder admitted with the state
    /// it saved (`K-TASK-024`). Its outcome has not been delivered.
    Admitted,
    /// Its outcome is committed and delivered, and the task has not yet
    /// run with it.
    Committed(Outcome),
}

/// How a wait ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed(Datum),
    Failed(ErrorDatum),
    Elapsed,
}

/// A `join` on a list that has not been decided. Which members have ended
/// is each member's own state, and when the join wakes is its place among
/// each running member's joiners.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListJoin {
    pub mode: JoinMode,
    pub members: Vec<TaskId>,
}

/// How a task ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    Returned(Value),
    Raised(Value),
}

/// One active call: where it stands, its variables, and the loops and
/// cleanup blocks it is inside.
///
/// The call runs the function body that holds `statement`: a unit's, or
/// the closure body nearest above it. The blocks, `try` statements and
/// handlers around the statement are derived from its site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The site of the call's pending statement (`K-SITE-001`). For every
    /// call but a task's innermost it is the calling statement, whose
    /// action is under way. For the innermost the task's state says which:
    /// a task that is merely ready issues the statement, and any other
    /// continues it with its action's value. An index one past the last
    /// statement of a block stands for the block's end.
    pub statement: Site,
    /// The call's bound variables, in the order the function body
    /// declares them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    /// The loops around the statement inside this call, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<Loop>,
    /// The `finally` blocks being run around the statement inside this
    /// call, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finally: Vec<Finally>,
}

/// One bound variable of a call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub name: Name,
    /// The node that declares the variable: the `let` statement, or the
    /// block whose parameter, loop binding or `catch` binding it is. A
    /// variable a closure captured is declared by the closure's body.
    pub declared: Site,
    pub value: Bound,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Bound {
    Value(Value),
    /// A variable a closure shares: the [`Object::Variable`] that holds it
    /// (`K-CLO-001`).
    Shared(ObjectId),
}

/// One loop a call is inside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Loop {
    /// The site of the `for` or the `while` (`K-SITE-003`).
    pub site: Site,
    /// How many iterations the loop has started. The one it is in is this
    /// less one (`K-EFF-008`).
    pub started: u64,
    /// For a `for`: how many of the collection's elements the loop has
    /// passed. In a list or a tuple it is the next index; in a map or a
    /// set, the number of present entries at or before the one last
    /// visited (`K-ITER-002`, `K-ITER-003`). Absent for a `while`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
}

/// One `finally` block a call is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finally {
    /// The site of the `try` statement.
    pub site: Site,
    pub entered: Entered,
}

/// How control reached a `finally`, which is how it leaves when the block
/// completes (`K-FORM-017`). A `throw` and a `return` carry a value, held
/// in [`Held::departing`]; a `break` and a `continue` go to the innermost
/// loop around the `try`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Entered {
    Normal,
    Throw,
    Return,
    Break,
    Continue,
}

/// The values a call's control state holds and no variable does.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Held {
    /// What each `for` of the call iterates, outermost first: the list,
    /// map or set, or the tuple whose elements the loop takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iterated: Vec<Value>,
    /// The value each `finally` entered by a `throw` or a `return` leaves
    /// with, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub departing: Vec<Value>,
    /// The arguments of a call of a library function's body, which its
    /// charge formula measures when the call ends (`K-CHG-003`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<Value>>,
}

impl Held {
    pub fn is_empty(&self) -> bool {
        self.iterated.is_empty() && self.departing.is_empty() && self.arguments.is_none()
    }
}

/// A root of the saved state: what one fragment is of.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Root {
    /// A session binding, by name.
    Session(Name),
    /// A task's handle.
    Task(TaskId),
    /// The bindings and position of a task's active call; depth 0 is the
    /// outermost.
    Call { task: TaskId, depth: u32 },
    /// The values that call's control state alone holds.
    Held { task: TaskId, depth: u32 },
}

/// What a root holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootState {
    Session(Value),
    Task(Box<Task>),
    Call(Call),
    Held(Held),
}

/// The stored header: the run's pins and meters, and every root in the
/// order ownership is decided in. It is written at every save.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub run: Run,
    /// The session bindings by name, then each task by handle number: its
    /// handle, then each of its calls from the outermost, a call's
    /// bindings before its held values.
    pub roots: Vec<Root>,
}

/// One stored fragment: a root and the heap objects it owns. An object
/// is owned by the first root, in the header's order, that reaches it; a
/// reference to an object another root owns is its identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fragment {
    pub root: RootState,
    /// The objects the root owns, ascending by identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<Owned>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    pub id: ObjectId,
    pub object: Object,
}
