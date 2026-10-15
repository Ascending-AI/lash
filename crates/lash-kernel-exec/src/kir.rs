//! Kir: the executor IR, the flat stream the machine runs.
//!
//! Every statement of an admitted document and of a library body is
//! lowered to one contiguous range of [`Op`]s: its expressions in
//! evaluation order with an explicit [`Op::Charge`] at each node's
//! pre-order position, then the one op that completes the statement or
//! leaves it where its step ends. The machine's control stack (blocks,
//! loops, `try` statements and frames) stays the only control state, so a
//! range starts at a step Boundary and ends at the next, and an action's
//! continuation, the op right after its [`Op::Act`], is the Boundary a
//! call returns or a wait resumes to.
//!
//! Registers hold one statement's temporaries only. Each op names its
//! source, destination and scratch registers, relative to the register
//! window the statement runs in; no register is live at a Boundary
//! (`K-STMT-004`), and registers are not roots.
//!
//! Kir is a private derivative of the durable document: it is derived
//! deterministically, never saved, and changing its shape changes no
//! identity.

macro_rules! index {
    ($($(#[$meta:meta])* $name:ident;)*) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl $name {
            /// The index as a position in its table.
            pub const fn ix(self) -> usize {
                self.0 as usize
            }
        }
    )*};
}

index! {
    /// A temporary of the statement running, relative to its window.
    Reg;
    /// A literal in the stream's constant pool.
    ConstIx;
    /// A session or unbound variable's name in the stream's name pool.
    NameIx;
    /// A field name in the stream's field pool.
    FieldIx;
    /// A record construction's field names in the stream's field lists.
    FieldListIx;
    /// A function body or closure body of the executable.
    CodeIx;
    /// A library function of the executable.
    LibIx;
    /// A kernel block of the executable.
    BlockIx;
}

impl Reg {
    /// The register after this one.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A variable as the code that declares it reaches it: its slot, by
/// declaration order, and where the slot sits in a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalRef {
    pub slot: u32,
    pub at: u32,
}

/// Consecutive registers: call arguments and construction members.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegRange {
    pub start: Reg,
    pub len: u32,
}

impl RegRange {
    pub fn regs(self) -> impl Iterator<Item = Reg> {
        (self.start.0..self.start.0.saturating_add(self.len)).map(Reg)
    }

    /// One past the last register.
    pub const fn end(self) -> u32 {
        self.start.0.saturating_add(self.len)
    }
}

/// Consecutive ops of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpRange {
    pub start: u32,
    pub len: u32,
}

impl OpRange {
    /// One past the last op.
    pub const fn end(self) -> u32 {
        self.start.saturating_add(self.len)
    }
}

/// One kernel operation. `Op` names semantic operations, not machine
/// instructions: each has one meaning, the machine's, and
/// [`crate::SEMANTICS`] states its charge events. An op that builds a value
/// from operands it reads in place (a member read, a construction, a call)
/// writes it over the first of them.
///
/// A fused op does what the ops it stands for do, in their order, and never
/// spans a Boundary. Lowering fuses only for an even layout: an odd one
/// runs every op apart, so that the layout laws compare the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// `units` fixed one-unit charge events (`K-CHG-002`): expression nodes
    /// and atom arguments, each charged before what it evaluates. Adjacent
    /// events coalesce; a run that crosses its bound inside them is charged
    /// up to the event that crosses, as one at a time would be.
    Charge {
        units: u32,
    },
    /// A `Charge` of `units` and the [`Op::Local`] after it, as one op.
    ChargeLocal {
        units: u8,
        dst: Reg,
        local: LocalRef,
    },
    /// A `Charge` of `units` and the [`Op::Const`] after it, as one op.
    ChargeConst {
        units: u8,
        dst: Reg,
        c: ConstIx,
    },
    Const {
        dst: Reg,
        c: ConstIx,
    },
    /// Reads a frame slot; an empty one raises `unbound_variable`.
    Local {
        dst: Reg,
        local: LocalRef,
    },
    /// Reads a session binding (`K-SES-001`).
    Session {
        dst: Reg,
        name: NameIx,
    },
    /// Reads a name no enclosing scope declares (`K-FORM-003`): it raises,
    /// so nothing reaches `dst`.
    Unbound {
        dst: Reg,
        name: NameIx,
    },
    /// `K-FORM-009`: field `field` of the record or error in `at`.
    Field {
        at: Reg,
        field: FieldIx,
    },
    /// `K-FORM-010`: what `at` holds at the index in the next register.
    Index {
        at: Reg,
    },
    MakeTuple {
        items: RegRange,
    },
    MakeList {
        items: RegRange,
    },
    MakeSet {
        items: RegRange,
    },
    /// `entries` holds key, value, key, value, ...
    MakeMap {
        entries: RegRange,
    },
    MakeRecord {
        fields: FieldListIx,
        values: RegRange,
    },
    Closure {
        dst: Reg,
        code: CodeIx,
    },
    /// A library call inside an expression: a native implementation's
    /// guard, call, retry after a collection, pin and formula, or a
    /// native-backed body run to its end inside the expression
    /// (`K-STMT-002`).
    Call {
        lib: LibIx,
        args: RegRange,
    },
    Clock {
        dst: Reg,
    },
    Random {
        dst: Reg,
    },
    /// `read` through the handle in `at` with the request in the next
    /// register (`K-HOST-004`).
    HostRead {
        at: Reg,
    },
    /// Keeps the value in `src` live until the step ends.
    Pin {
        src: Reg,
    },
    /// The statement's action (`K-STMT-001`): its atoms are read and it
    /// starts. One that completes at once leaves its value in register 0
    /// and goes on to the next op; one that waits or enters a frame ends the
    /// step, and the next op is where the statement resumes, its value in
    /// register 0.
    Act,
    /// Declares a variable with its first value (`K-FORM-004`).
    Bind {
        local: LocalRef,
        src: Reg,
    },
    /// Declares a variable a closure shares, in a heap cell.
    BindShared {
        local: LocalRef,
        src: Reg,
    },
    /// Declares a session binding.
    BindSession {
        name: NameIx,
        src: Reg,
    },
    WriteLocal {
        local: LocalRef,
        src: Reg,
    },
    WriteSession {
        name: NameIx,
        src: Reg,
    },
    /// Assigns a name no scope declares: it raises after reserving.
    WriteUnbound {
        name: NameIx,
        src: Reg,
    },
    /// `K-FORM-005`.
    SetField {
        target: Reg,
        field: FieldIx,
        src: Reg,
    },
    /// `K-FORM-006`: the index is in the register after `target`.
    SetIndex {
        target: Reg,
        src: Reg,
    },
    /// `K-FORM-007`.
    RemoveField {
        target: Reg,
        field: FieldIx,
    },
    /// `K-FORM-007`: the index is in the register after `target`.
    RemoveIndex {
        target: Reg,
    },
    /// Forward expression control flow, relative to the following op.
    Jump {
        skip: u32,
    },
    JumpIfFalse {
        cond: Reg,
        skip: u32,
    },
    /// Binds an expression local and continues within the statement.
    LetBind {
        local: LocalRef,
        src: Reg,
    },
    LetBindShared {
        local: LocalRef,
        src: Reg,
    },
    /// Releases the expression local after its body; captured cells survive.
    LetEnd {
        local: LocalRef,
    },
    /// Enters `then_` or `else_` on the bool in `cond`, or raises.
    Branch {
        cond: Reg,
        then_: BlockIx,
        else_: BlockIx,
    },
    /// Starts a `for` over `iterable` and takes its first iteration step.
    ForIn {
        iterable: Reg,
    },
    /// Starts a `while` and takes its first iteration step. The ops after
    /// it, to its [`Op::Test`], are its condition.
    While,
    /// A `while` condition's value: the end of its test.
    Test {
        cond: Reg,
    },
    Try {
        body: BlockIx,
    },
    Break,
    Continue,
    Return {
        src: Reg,
    },
    Throw {
        src: Reg,
    },
    Print {
        src: Reg,
    },
    Finish {
        src: Reg,
    },
    Fail {
        src: Reg,
    },
    /// The statement completes in its block.
    End,
}

/// The kind of an [`Op`], without its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpKind {
    Charge,
    ChargeLocal,
    ChargeConst,
    Const,
    Local,
    Session,
    Unbound,
    Field,
    Index,
    MakeTuple,
    MakeList,
    MakeSet,
    MakeMap,
    MakeRecord,
    Closure,
    Call,
    Clock,
    Random,
    HostRead,
    Pin,
    Act,
    Bind,
    BindShared,
    BindSession,
    WriteLocal,
    WriteSession,
    WriteUnbound,
    SetField,
    SetIndex,
    RemoveField,
    RemoveIndex,
    Jump,
    JumpIfFalse,
    LetBind,
    LetBindShared,
    LetEnd,
    Branch,
    ForIn,
    While,
    Test,
    Try,
    Break,
    Continue,
    Return,
    Throw,
    Print,
    Finish,
    Fail,
    End,
}

impl OpKind {
    /// Every kind, in declaration order: the order [`crate::SEMANTICS`]
    /// states them in.
    pub const ALL: &'static [Self] = &[
        Self::Charge,
        Self::ChargeLocal,
        Self::ChargeConst,
        Self::Const,
        Self::Local,
        Self::Session,
        Self::Unbound,
        Self::Field,
        Self::Index,
        Self::MakeTuple,
        Self::MakeList,
        Self::MakeSet,
        Self::MakeMap,
        Self::MakeRecord,
        Self::Closure,
        Self::Call,
        Self::Clock,
        Self::Random,
        Self::HostRead,
        Self::Pin,
        Self::Act,
        Self::Bind,
        Self::BindShared,
        Self::BindSession,
        Self::WriteLocal,
        Self::WriteSession,
        Self::WriteUnbound,
        Self::SetField,
        Self::SetIndex,
        Self::RemoveField,
        Self::RemoveIndex,
        Self::Jump,
        Self::JumpIfFalse,
        Self::LetBind,
        Self::LetBindShared,
        Self::LetEnd,
        Self::Branch,
        Self::ForIn,
        Self::While,
        Self::Test,
        Self::Try,
        Self::Break,
        Self::Continue,
        Self::Return,
        Self::Throw,
        Self::Print,
        Self::Finish,
        Self::Fail,
        Self::End,
    ];

    /// Whether ops of this kind end their range: the statement completes
    /// or its step ends, or a `while` condition's test has its value.
    pub const fn is_terminator(self) -> bool {
        !matches!(
            self,
            Self::Charge
                | Self::ChargeLocal
                | Self::ChargeConst
                | Self::Const
                | Self::Local
                | Self::Session
                | Self::Unbound
                | Self::Field
                | Self::Index
                | Self::MakeTuple
                | Self::MakeList
                | Self::MakeSet
                | Self::MakeMap
                | Self::MakeRecord
                | Self::Closure
                | Self::Call
                | Self::Clock
                | Self::Random
                | Self::HostRead
                | Self::Pin
                | Self::Act
                | Self::Jump
                | Self::JumpIfFalse
                | Self::LetBind
                | Self::LetBindShared
                | Self::LetEnd
        )
    }
}

/// An index an op holds, by the table it points into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operand {
    Reg(Reg),
    Regs(RegRange),
    Local(LocalRef),
    Const(ConstIx),
    Name(NameIx),
    Field(FieldIx),
    FieldList(FieldListIx),
    Code(CodeIx),
    Lib(LibIx),
    Block(BlockIx),
}

impl Op {
    pub const fn kind(&self) -> OpKind {
        match self {
            Self::Charge { .. } => OpKind::Charge,
            Self::ChargeLocal { .. } => OpKind::ChargeLocal,
            Self::ChargeConst { .. } => OpKind::ChargeConst,
            Self::Const { .. } => OpKind::Const,
            Self::Local { .. } => OpKind::Local,
            Self::Session { .. } => OpKind::Session,
            Self::Unbound { .. } => OpKind::Unbound,
            Self::Field { .. } => OpKind::Field,
            Self::Index { .. } => OpKind::Index,
            Self::MakeTuple { .. } => OpKind::MakeTuple,
            Self::MakeList { .. } => OpKind::MakeList,
            Self::MakeSet { .. } => OpKind::MakeSet,
            Self::MakeMap { .. } => OpKind::MakeMap,
            Self::MakeRecord { .. } => OpKind::MakeRecord,
            Self::Closure { .. } => OpKind::Closure,
            Self::Call { .. } => OpKind::Call,
            Self::Clock { .. } => OpKind::Clock,
            Self::Random { .. } => OpKind::Random,
            Self::HostRead { .. } => OpKind::HostRead,
            Self::Pin { .. } => OpKind::Pin,
            Self::Act => OpKind::Act,
            Self::Bind { .. } => OpKind::Bind,
            Self::BindShared { .. } => OpKind::BindShared,
            Self::BindSession { .. } => OpKind::BindSession,
            Self::WriteLocal { .. } => OpKind::WriteLocal,
            Self::WriteSession { .. } => OpKind::WriteSession,
            Self::WriteUnbound { .. } => OpKind::WriteUnbound,
            Self::SetField { .. } => OpKind::SetField,
            Self::SetIndex { .. } => OpKind::SetIndex,
            Self::RemoveField { .. } => OpKind::RemoveField,
            Self::RemoveIndex { .. } => OpKind::RemoveIndex,
            Self::Jump { .. } => OpKind::Jump,
            Self::JumpIfFalse { .. } => OpKind::JumpIfFalse,
            Self::LetBind { .. } => OpKind::LetBind,
            Self::LetBindShared { .. } => OpKind::LetBindShared,
            Self::LetEnd { .. } => OpKind::LetEnd,
            Self::Branch { .. } => OpKind::Branch,
            Self::ForIn { .. } => OpKind::ForIn,
            Self::While => OpKind::While,
            Self::Test { .. } => OpKind::Test,
            Self::Try { .. } => OpKind::Try,
            Self::Break => OpKind::Break,
            Self::Continue => OpKind::Continue,
            Self::Return { .. } => OpKind::Return,
            Self::Throw { .. } => OpKind::Throw,
            Self::Print { .. } => OpKind::Print,
            Self::Finish { .. } => OpKind::Finish,
            Self::Fail { .. } => OpKind::Fail,
            Self::End => OpKind::End,
        }
    }

    /// The registers the op reads, in order.
    pub fn reads(&self) -> Vec<Reg> {
        match *self {
            Self::Field { at, .. } => vec![at],
            Self::Index { at } | Self::HostRead { at } => vec![at, at.next()],
            Self::MakeTuple { items }
            | Self::MakeList { items }
            | Self::MakeSet { items }
            | Self::MakeMap { entries: items }
            | Self::MakeRecord { values: items, .. }
            | Self::Call { args: items, .. } => items.regs().collect(),
            Self::Pin { src }
            | Self::LetBind { src, .. }
            | Self::LetBindShared { src, .. }
            | Self::Bind { src, .. }
            | Self::BindShared { src, .. }
            | Self::BindSession { src, .. }
            | Self::WriteLocal { src, .. }
            | Self::WriteSession { src, .. }
            | Self::WriteUnbound { src, .. }
            | Self::Return { src }
            | Self::Throw { src }
            | Self::Print { src }
            | Self::Finish { src }
            | Self::Fail { src } => vec![src],
            Self::SetField { target, src, .. } => vec![target, src],
            Self::SetIndex { target, src } => vec![target, target.next(), src],
            Self::RemoveField { target, .. } => vec![target],
            Self::RemoveIndex { target } => vec![target, target.next()],
            Self::Branch { cond, .. } | Self::Test { cond } | Self::JumpIfFalse { cond, .. } => {
                vec![cond]
            }
            Self::ForIn { iterable } => vec![iterable],
            Self::Charge { .. }
            | Self::ChargeLocal { .. }
            | Self::ChargeConst { .. }
            | Self::Const { .. }
            | Self::Local { .. }
            | Self::Session { .. }
            | Self::Unbound { .. }
            | Self::Closure { .. }
            | Self::Clock { .. }
            | Self::Random { .. }
            | Self::Act
            | Self::While
            | Self::Try { .. }
            | Self::Break
            | Self::Continue
            | Self::Jump { .. }
            | Self::LetEnd { .. }
            | Self::End => Vec::new(),
        }
    }

    /// The register the op writes its value to.
    pub const fn writes(&self) -> Option<Reg> {
        match *self {
            Self::Const { dst, .. }
            | Self::ChargeConst { dst, .. }
            | Self::ChargeLocal { dst, .. }
            | Self::Local { dst, .. }
            | Self::Session { dst, .. }
            | Self::Unbound { dst, .. }
            | Self::Closure { dst, .. }
            | Self::Clock { dst }
            | Self::Random { dst } => Some(dst),
            Self::Field { at, .. } | Self::Index { at } | Self::HostRead { at } => Some(at),
            Self::MakeTuple { items }
            | Self::MakeList { items }
            | Self::MakeSet { items }
            | Self::MakeMap { entries: items }
            | Self::MakeRecord { values: items, .. }
            | Self::Call { args: items, .. } => Some(items.start),
            Self::Act => Some(Reg(0)),
            _ => None,
        }
    }

    /// Every index the op holds, in field order.
    pub fn operands(&self) -> Vec<Operand> {
        use Operand as O;
        let mut out: Vec<Operand> = match *self {
            Self::Const { c, .. } | Self::ChargeConst { c, .. } => vec![O::Const(c)],
            Self::Local { local, .. }
            | Self::ChargeLocal { local, .. }
            | Self::LetBind { local, .. }
            | Self::LetBindShared { local, .. }
            | Self::LetEnd { local }
            | Self::Bind { local, .. }
            | Self::BindShared { local, .. }
            | Self::WriteLocal { local, .. } => vec![O::Local(local)],
            Self::Session { name, .. }
            | Self::Unbound { name, .. }
            | Self::BindSession { name, .. }
            | Self::WriteSession { name, .. }
            | Self::WriteUnbound { name, .. } => vec![O::Name(name)],
            Self::Field { field, .. }
            | Self::SetField { field, .. }
            | Self::RemoveField { field, .. } => vec![O::Field(field)],
            Self::MakeRecord { fields, .. } => vec![O::FieldList(fields)],
            Self::Closure { code, .. } => vec![O::Code(code)],
            Self::Call { lib, .. } => vec![O::Lib(lib)],
            Self::Branch { then_, else_, .. } => vec![O::Block(then_), O::Block(else_)],
            Self::Try { body } => vec![O::Block(body)],
            _ => Vec::new(),
        };
        match *self {
            Self::MakeTuple { items }
            | Self::MakeList { items }
            | Self::MakeSet { items }
            | Self::MakeMap { entries: items }
            | Self::MakeRecord { values: items, .. }
            | Self::Call { args: items, .. } => out.push(O::Regs(items)),
            _ => {
                out.extend(self.reads().into_iter().map(O::Reg));
                out.extend(self.writes().map(O::Reg));
            }
        }
        out
    }
}

// An op is two words: decoding one is a load, and the stream stays dense.
const _: () = assert!(std::mem::size_of::<Op>() == 16);
