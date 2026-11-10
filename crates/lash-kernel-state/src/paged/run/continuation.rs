//! A sealed run of the bytecode core (cutover contract §5, §6): its heap
//! root and everything the machine holds beside it, keyed by document
//! coordinates, never by bytecode position (`K-SITE-001`).
//!
//! Every value the machine holds outside an object is one of the heap
//! root's values, named here by its index into [`HeapRoot::roots`]; the
//! objects those values reach are the heap's.
//!
//! A frame names its unit by the unit's Link, kind and site, and where it stands
//! by a site, a phase and which compiled copy of the site; its slots by
//! the site that declared each, what the slot holds there, and the copy;
//! its operands in the order the opening build's resume table lists them;
//! its handlers by the `try` that installed each. The opening build's
//! tables map these back to positions, and a coordinate they do not hold
//! refuses the open.

use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::{FunctionId, Name, OperationUse};
use lash_kernel_value::sites::{EffectIdentity, Site, TaskIdentity, WaitCancellation};
use lash_kernel_value::tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};
use lash_kernel_value::types::Type;

use super::super::PageKind;
use super::super::source::grant;
use super::super::tree::Access;
use super::root::HeapRoot;

/// A struct whose fields encode in order.
macro_rules! record {
    ($(#[$meta:meta])* pub struct $name:ident {
        $($(#[$field_meta:meta])* pub $field:ident : $ty:ty,)*
    }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name {
            $($(#[$field_meta])* pub $field: $ty,)*
        }

        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.seq(record!(@count $($field)*));
                $(self.$field.encode(out);)*
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                input.expect_struct(record!(@count $($field)*))?;
                Ok(Self {
                    $($field: <$ty as TokenCodec>::decode(input)?,)*
                })
            }
        }
    };
    (@count) => { 0u64 };
    (@count $head:ident $($tail:ident)*) => { 1u64 + record!(@count $($tail)*) };
}

/// A variant `tag` with one field.
fn one<T: TokenCodec>(out: &mut TokenWriter, tag: u32, value: &T) {
    out.sum(tag, 1);
    value.encode(out);
}

record! {
    /// A sealed run: its heap and the machine's state beside it.
    pub struct RunRecord {
        pub heap: HeapRoot,
        /// What the run started on.
        pub start: RunStart,
        /// The operation uses the run binds as reads (`K-EFF-012`).
        pub reads: Vec<OperationUse>,
        /// What the run has paid.
        pub charged: u64,
        /// The total a charge refusal reported, once one has.
        pub refused: Option<u64>,
        /// Each host binding's value, then each session binding's of
        /// [`Self::session`], then each saved function's of
        /// [`Self::functions`].
        pub globals: Vec<u32>,
        /// The carried session's bindings `main` names nowhere, ascending:
        /// the globals past the program's.
        pub session: Vec<Name>,
        /// The carried session's saved functions, ascending by name: the
        /// globals past the session's.
        pub functions: Vec<super::session::RunFunction>,
        /// The session page the carried session came from, whose rows the
        /// cell's carry edits.
        pub carried: Option<PageRef>,
        /// Values the scheduler holds outside any task.
        pub held: Vec<u32>,
        /// The task whose context the machine holds.
        pub running: u32,
        /// Whether a task is installed in the machine's context.
        pub installed: bool,
        /// The tasks that have not ended, `main` included.
        pub live: u32,
        /// Every task, in spawn order; `main` first.
        pub tasks: Vec<RunTask>,
        /// Ready task numbers, in the order they run (`K-TASK-003`).
        pub ready: Vec<u32>,
        /// Every wait handed out and not yet run with, by number.
        pub waits: Vec<RunWait>,
        /// Wait numbers withdrawn after being handed out (`K-MACH-004`).
        pub withdrawn: Vec<u64>,
        pub next_wait: u64,
        /// The waits requested since the last park, in request order.
        pub issued: Vec<u64>,
        /// Cancellations the next park reports, in order (`K-MACH-003`).
        pub cancellations: Vec<WaitCancellation>,
        /// List joins, by number.
        pub joins: Vec<RunJoin>,
        pub next_join: u64,
        /// The live resume closure's build helpers (contract §7): those
        /// the code reachable from each frame's resume point and each
        /// handler runs, calls or names, frames' own bodies included.
        /// A helper only completed calls ran is not among them.
        pub pins: Vec<FunctionId>,
        /// The build helpers whose closures or function values the heap
        /// has held: what any stored code value may still call. Kept
        /// across seals, never shrunk, since a cold page's values are not
        /// read to seal.
        pub held_code: Vec<FunctionId>,
    }
}

/// What a run started on: its program is compiled again from it.
#[derive(Clone, Debug, PartialEq)]
pub enum RunStart {
    Main,
    /// An export, by name.
    Entry(Name),
    /// A callable a run published (`K-CLO-004`), its frozen captures
    /// with it.
    Callable(Box<lash_kernel_doc::FunctionValue>),
}

impl TokenCodec for RunStart {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Main => out.sum(0, 0),
            Self::Entry(name) => one(out, 1, name),
            Self::Callable(function) => one(out, 2, function.as_ref()),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 0) => Self::Main,
            (1, 1) => Self::Entry(Name::decode(input)?),
            (2, 1) => Self::Callable(Box::new(lash_kernel_doc::FunctionValue::decode(input)?)),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

record! {
    /// One task.
    pub struct RunTask {
        pub identity: TaskIdentity,
        /// Its frames, outermost first.
        pub frames: Vec<RunFrame>,
        pub handlers: Vec<RunHandler>,
        /// The arguments of its frames' bills, in order.
        pub billed: Vec<u32>,
        pub state: RunTaskState,
        /// A delivered `then yield` wait owes one more turn (`K-TASK-005`).
        pub turn: bool,
        /// How many times it has run each action site.
        pub occurrences: Vec<RunOccurrence>,
        /// Where its spawner stood, when none of its frames is document
        /// code (`K-EFF-008`).
        pub spawn_origin: Option<Site>,
        /// A member of a list `join` that returned or raised (`K-TASK-016`).
        pub passed: bool,
        /// A `join` raised its error (`K-TASK-016`).
        pub observed: bool,
        /// The joins waiting on it, in the order they began.
        pub joiners: Vec<RunJoiner>,
    }
}

record! {
    /// How many times a task has run the action site `site` of Link
    /// `link`.
    pub struct RunOccurrence {
        pub link: LinkId,
        pub site: Site,
        pub count: u64,
    }
}

record! {
    /// One activation.
    pub struct RunFrame {
        pub unit: RunUnit,
        /// Where it stands: a site, the phase there and the compiled copy.
        pub site: Site,
        pub phase: PointPhase,
        pub copy: u32,
        /// Its slots, each named by its declaring site in the unit; a slot
        /// the opening build's unit declares and the frame does not hold, as
        /// a succession's new local, opens absent.
        pub slots: Vec<RunSlot>,
        /// The operands above its slots, in the resume table's order.
        pub operands: Vec<u32>,
        /// The closure it runs.
        pub closure: Option<u32>,
        /// The completion formula its call owes (`K-CHG-003`).
        pub bill: Option<RunBill>,
    }
}

record! {
    /// A unit, by its Link, kind and site, and whether it is the compile
    /// that runs inside a covered library extent (`K-CHG-007`): a library
    /// body has one compile of each, with its own resume and charge tables.
    pub struct RunUnit {
        pub link: LinkId,
        pub kind: UnitKind,
        pub site: Site,
        pub covered: bool,
    }
}

record! {
    /// One slot: the site that declared it, what it holds there and which
    /// compiled copy, and its value.
    pub struct RunSlot {
        pub site: Vec<u32>,
        pub role: SlotRole,
        pub copy: u32,
        pub value: u32,
    }
}

record! {
    /// A handler, by the `try` that installed it: the frame it was
    /// installed in, the `try`'s path and copy among the unit's `try`s at
    /// that path, and the stack height it truncates to, above the frame's
    /// base.
    pub struct RunHandler {
        pub frame: u32,
        pub site: Vec<u32>,
        pub copy: u32,
        pub height: u32,
    }
}

record! {
    /// The completion formula a frame owes, its unit's function's: where
    /// its arguments start among the task's billed values.
    pub struct RunBill {
        pub from: u32,
    }
}

record! {
    /// A waited `perform`'s wait.
    pub struct RunWait {
        pub number: u64,
        pub task: u32,
        pub phase: WaitPhase,
        /// The type the `perform` decodes its result by.
        pub result: Type,
        pub then_yield: bool,
        /// What the next park hands over, until it does.
        pub request: Option<RunRequest>,
    }
}

record! {
    /// An effect request not yet handed over.
    pub struct RunRequest {
        pub identity: EffectIdentity,
        pub use_: OperationUse,
        pub args: Vec<Datum>,
        pub result: Type,
        pub origin: Site,
    }
}

record! {
    /// A list `join`.
    pub struct RunJoin {
        pub number: u64,
        pub joiner: u32,
        pub mode: JoinKind,
        pub members: Vec<u32>,
    }
}

/// What a unit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnitKind {
    Main,
    Declared,
    Body,
    Closure,
    Spawn,
}

/// Where at a site a frame stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointPhase {
    Call,
    Ready,
    Issued,
    Went,
    Statement,
    Back,
}

/// A list join's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    All,
    AllSettled,
    Race,
    Any,
}

/// Fieldless enums encode as their variant.
macro_rules! tags {
    ($($name:ident { $($variant:ident = $tag:literal,)* })*) => {$(
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.sum(
                    match self {
                        $(Self::$variant => $tag,)*
                    },
                    0,
                );
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let (tag, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 0)?;
                match tag {
                    $($tag => Ok(Self::$variant),)*
                    variant => Err(TokenError::UnsupportedVariant { variant }),
                }
            }
        }
    )*};
}

tags! {
    UnitKind { Main = 0, Declared = 1, Body = 2, Closure = 3, Spawn = 4, }
    PointPhase { Call = 0, Ready = 1, Issued = 2, Went = 3, Statement = 4, Back = 5, }
    JoinKind { All = 0, AllSettled = 1, Race = 2, Any = 3, }
}

/// What a slot holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotRole {
    Variable(Name),
    Argument(u32),
    Held,
    Passes,
    Loop(u32),
}

impl TokenCodec for SlotRole {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Variable(name) => one(out, 0, name),
            Self::Argument(at) => one(out, 1, at),
            Self::Held => out.sum(2, 0),
            Self::Passes => out.sum(3, 0),
            Self::Loop(at) => one(out, 4, at),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 1) => Self::Variable(Name::decode(input)?),
            (1, 1) => Self::Argument(u32::decode(input)?),
            (2, 0) => Self::Held,
            (3, 0) => Self::Passes,
            (4, 1) => Self::Loop(u32::decode(input)?),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// A task's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunTaskState {
    Running,
    Ready(RunResume),
    /// Waiting on a `perform`'s wait.
    Effect(u64),
    /// Waiting on a task's end.
    Join(u32),
    /// Waiting on a list join.
    JoinMany(u64),
    Returned(u32),
    Raised(u32),
}

impl TokenCodec for RunTaskState {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Running => out.sum(0, 0),
            Self::Ready(resume) => one(out, 1, resume),
            Self::Effect(wait) => one(out, 2, wait),
            Self::Join(task) => one(out, 3, task),
            Self::JoinMany(join) => one(out, 4, join),
            Self::Returned(value) => one(out, 5, value),
            Self::Raised(value) => one(out, 6, value),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 0) => Self::Running,
            (1, 1) => Self::Ready(RunResume::decode(input)?),
            (2, 1) => Self::Effect(u64::decode(input)?),
            (3, 1) => Self::Join(u32::decode(input)?),
            (4, 1) => Self::JoinMany(u64::decode(input)?),
            (5, 1) => Self::Returned(u32::decode(input)?),
            (6, 1) => Self::Raised(u32::decode(input)?),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// What a ready task resumes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunResume {
    Continue,
    Value(u32),
    Raise(u32),
    /// The committed outcome of its wait.
    Outcome(u64),
    /// The task it joined has ended.
    Joined(u32),
}

impl TokenCodec for RunResume {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Continue => out.sum(0, 0),
            Self::Value(value) => one(out, 1, value),
            Self::Raise(value) => one(out, 2, value),
            Self::Outcome(wait) => one(out, 3, wait),
            Self::Joined(task) => one(out, 4, task),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 0) => Self::Continue,
            (1, 1) => Self::Value(u32::decode(input)?),
            (2, 1) => Self::Raise(u32::decode(input)?),
            (3, 1) => Self::Outcome(u64::decode(input)?),
            (4, 1) => Self::Joined(u32::decode(input)?),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// A join waiting on a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunJoiner {
    Single(u32),
    List(u64),
}

impl TokenCodec for RunJoiner {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Single(task) => one(out, 0, task),
            Self::List(join) => one(out, 1, join),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 1) => Self::Single(u32::decode(input)?),
            (1, 1) => Self::List(u64::decode(input)?),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// Where a wait stands.
#[derive(Clone, Debug, PartialEq)]
pub enum WaitPhase {
    /// Requested since the last park: no embedder has it.
    Issued,
    /// Handed over at a park.
    Admitted,
    /// Delivered as the host gave it, decoded when the task consumes it.
    Completed(Datum),
    /// Delivered as failed: the host's error, as a [`Datum::Error`].
    Failed(Datum),
}

impl TokenCodec for WaitPhase {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Issued => out.sum(0, 0),
            Self::Admitted => out.sum(1, 0),
            Self::Completed(datum) => one(out, 2, datum),
            Self::Failed(datum) => one(out, 3, datum),
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match input.sum()? {
            (0, 0) => Self::Issued,
            (1, 0) => Self::Admitted,
            (2, 1) => Self::Completed(Datum::decode(input)?),
            (3, 1) => Self::Failed(Datum::decode(input)?),
            (variant, _) => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

/// The most bytes of encoded state one segment page holds, under the
/// format's node bound.
pub const SEGMENT_BYTES: usize = 60 * 1024;

record! {
    /// A run root page's body: the directory and carried session pages it
    /// retains and the segments of the run's encoded [`RunRecord`], in
    /// order.
    pub struct RunPage {
        pub directory: Option<PageRef>,
        pub carried: Option<PageRef>,
        pub segments: Vec<PageRef>,
        pub length: u64,
    }
}

/// Seals `record` under a run root page: its encoded state in segments,
/// the root naming them and the directory's root page as its children.
///
/// # Errors
///
/// The page port's faults.
pub fn write(access: &Access<'_>, record: &RunRecord) -> Result<PageRef, HeapFault> {
    let bytes = encode_body(record);
    let directory = record.heap.directory.page;
    let mut segments = Vec::with_capacity(bytes.len().div_ceil(SEGMENT_BYTES));
    for chunk in bytes.chunks(SEGMENT_BYTES) {
        let mut out = TokenWriter::new();
        out.bytes(chunk);
        segments.push(access.seal(PageKind::RUN_SEGMENT, out.into_bytes(), Vec::new())?);
    }
    let page = RunPage {
        directory,
        carried: record.carried,
        segments,
        length: bytes.len() as u64,
    };
    let children = page
        .directory
        .iter()
        .chain(&page.carried)
        .chain(&page.segments)
        .copied()
        .collect();
    access.seal(PageKind::RUN_ROOT, encode_body(&page), children)
}

/// The run sealed under run root page `page`.
///
/// # Errors
///
/// The page port's faults; [`HeapFault::Corrupt`] for a page that is not
/// a run root, a segment that is not one, or state that does not decode
/// or names another directory than its root retains.
pub fn read(access: &Access<'_>, page: PageRef) -> Result<RunRecord, HeapFault> {
    let (root, bytes) = read_segmented(access, page)?;
    let mut record: RunRecord = decode_body(&bytes).map_err(|error| HeapFault::Corrupt {
        page: page.id,
        reason: error.to_string(),
    })?;
    retain(page, &mut record.heap, root.directory)?;
    if record.carried.map(|page| page.id) != root.carried.map(|page| page.id) {
        return Err(HeapFault::Corrupt {
            page: page.id,
            reason: "the run names another carried session than its root".into(),
        });
    }
    record.carried = root.carried;
    Ok(record)
}

/// The root page `page`, the pages it retains granted, and the bytes its
/// segments hold.
fn read_segmented(access: &Access<'_>, page: PageRef) -> Result<(RunPage, Vec<u8>), HeapFault> {
    let corrupt = |reason: String| HeapFault::Corrupt {
        page: page.id,
        reason,
    };
    let answer = access.read(page)?;
    if answer.kind == PageKind::KEYED_LEAF {
        // The paged engine parked its run under its heap's header, a keyed
        // leaf; no run of the core is rooted at one.
        return Err(HeapFault::Retired {
            page: page.id,
            state: lash_kernel_value::tokens::RetiredState::ParkedRun,
        });
    }
    if answer.kind != PageKind::RUN_ROOT {
        return Err(corrupt("not a run root page".into()));
    }
    let mut root: RunPage =
        decode_body(&answer.tokens).map_err(|error| corrupt(error.to_string()))?;
    for held in [&mut root.directory, &mut root.carried]
        .into_iter()
        .flatten()
    {
        grant(held, &answer.children)?;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(root.length).unwrap_or(0));
    for segment in &mut root.segments {
        grant(segment, &answer.children)?;
        let read = access.read(*segment)?;
        if read.kind != PageKind::RUN_SEGMENT {
            return Err(corrupt("a run segment is not one".into()));
        }
        let mut input = TokenReader::new(&read.tokens);
        let chunk = input.bytes().map_err(|error| corrupt(error.to_string()))?;
        bytes.extend_from_slice(chunk);
    }
    if bytes.len() as u64 != root.length {
        return Err(corrupt("the run's segments hold another length".into()));
    }
    Ok((root, bytes))
}

/// Points `heap`'s directory at the granted `directory` its root page
/// `page` retains, refusing a heap that names another.
fn retain(page: PageRef, heap: &mut HeapRoot, directory: Option<PageRef>) -> Result<(), HeapFault> {
    if heap.directory.page.map(|page| page.id) != directory.map(|page| page.id) {
        return Err(HeapFault::Corrupt {
            page: page.id,
            reason: "the run names another directory than its root".into(),
        });
    }
    heap.directory.page = directory;
    Ok(())
}

/// The run sealed under run root page `page`, read through `source`, and
/// its heap's directory over the same port.
///
/// # Errors
///
/// As [`read`].
pub fn open(
    source: Box<dyn super::super::PageSource>,
    page: PageRef,
) -> Result<(RunRecord, super::directory::Directory), HeapFault> {
    let access = Access::new(source);
    let record = read(&access, page)?;
    let directory = super::directory::Directory::over(record.heap.directory, access);
    Ok((record, directory))
}
