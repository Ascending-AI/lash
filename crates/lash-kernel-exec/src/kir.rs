//! Kir: the executor IR.
//!
//! A [`KirFunction`] is one function body lowered to a flat control-flow
//! graph of semantic operations. Registers hold expression temporaries
//! only; bindings live in the canonical frame's slots. Every step boundary
//! of the machine is an explicit [`Boundary`], so every tier stops at the
//! same places and every exit can materialize the frame the interpreter
//! would hold there.
//!
//! Kir is a private derivative of the durable document: it is derived
//! deterministically, never serialized into a parked run, and changing its
//! shape changes no identity (only the artifact key, through
//! [`crate::EXEC_ABI_DIGEST`]).

use lash_kernel_doc::{EffectName, FunctionId, JoinMode, Name, Site, Type, Unit, Value};

use crate::boundary::Boundary;

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
    /// An expression temporary of one activation. No register is live at a
    /// [`Boundary`] (`K-STMT-004`), and registers are not roots.
    Reg;
    /// An entry of [`KirFunction::consts`].
    ConstIx;
    /// An entry of [`KirFunction::names`].
    NameIx;
    /// An entry of [`KirFunction::fields`].
    FieldIx;
    /// An entry of [`KirFunction::field_lists`].
    FieldListIx;
    /// An entry of [`KirFunction::codes`]: a closure or declared function
    /// this body names.
    CodeIx;
    /// An entry of [`KirFunction::libs`]: a library function this body calls.
    LibIx;
    /// An entry of [`KirFunction::blocks`]. Block 0 is the entry.
    KBlockIx;
    /// An entry of [`KirFunction::boundaries`].
    BoundaryIx;
    /// An entry of [`KirFunction::stmts`]: a kernel statement of the body.
    StmtIx;
    /// An entry of [`KirFunction::actions`].
    ActionIx;
    /// An entry of [`KirFunction::loops`].
    LoopIx;
    /// An entry of [`KirFunction::tries`].
    TryIx;
    /// An entry of [`KirFunction::scopes`]: a kernel block of the body.
    ScopeIx;
    /// A slot's position in a frame ([`LocalRef::at`]).
    SlotAt;
    /// A fixed diagnostic message, by its position in the semantics' message
    /// table.
    MsgIx;
}

/// A variable as the code that declares it reaches it: its slot, where the
/// slot sits in a frame, and whether a closure shares it (then the slot
/// holds a heap cell).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalRef {
    pub slot: u32,
    pub at: u32,
    pub shared: bool,
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
}

/// Consecutive ops of [`KirFunction::ops`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpRange {
    pub start: u32,
    pub len: u32,
}

/// A variable a read or write names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VarRef {
    Local(LocalRef),
    /// A session binding, looked up by name (`K-SES-001`).
    Session(NameIx),
    /// A name no enclosing scope declares (`K-FORM-003`): reading it raises.
    Unbound(NameIx),
}

/// The code a [`KirFunction`] is the body of.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// A library function's body.
    Library(FunctionId),
    /// A document's `main` or one of its declared functions
    /// ([`Unit::Main`] or [`Unit::Function`]).
    Document(Unit),
    /// A closure, by the site of its body.
    Closure(Site),
}

/// A frame slot: the key a parked run restores it by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlotInfo {
    pub name: Name,
    /// The node that declares the variable.
    pub declared: Site,
    /// A closure shares the variable, so it lives in a heap cell.
    pub shared: bool,
}

/// A kernel block of the body: what `Control::Block` names, and the slots
/// whose variables end with it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    pub site: Site,
    /// Frame positions of the variables the block declares.
    pub declares: Box<[u32]>,
}

/// A basic block of the CFG: a range of ops whose last op, and only that
/// op, is a terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KBlock {
    pub ops: OpRange,
    /// Where a raise inside the block goes: the handler of the innermost
    /// `try` of this frame that takes it, or none (the raise leaves the
    /// activation).
    pub exception: Option<KBlockIx>,
}

/// Where an op came from: its statement's site and the child path of the
/// expression node inside it, for diagnostics and logical stack traces.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OpOrigin {
    pub stmt: Site,
    pub child: Box<[u32]>,
}

/// A loop of the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KLoop {
    pub stmt: StmtIx,
    pub kind: LoopKind,
    /// The block that tests the loop's continuation.
    pub test: KBlockIx,
    /// The block after the loop.
    pub exit: KBlockIx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LoopKind {
    For { binding: LocalRef },
    While,
}

/// A `try` of the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KTry {
    pub stmt: StmtIx,
    /// The `catch` binding and the handler block.
    pub catch: Option<(LocalRef, KBlockIx)>,
    /// The `finally` block.
    pub finally: Option<KBlockIx>,
}

/// An action statement's action: the work an [`Op::Act`] exits to the
/// machine for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KAction {
    pub site: Site,
    pub kind: ActionKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Call {
        callee: CalleeRef,
        args: RegRange,
    },
    Perform {
        effect: EffectName,
        args: RegRange,
        result: Type,
    },
    Sleep(Reg),
    Join(Reg),
    JoinMany(JoinMode, Reg),
    Yield,
    Spawn {
        callee: CalleeRef,
        args: RegRange,
    },
    Cancel(Reg),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CalleeRef {
    Declared(CodeIx),
    Value(VarRef),
    Library(LibIx),
}

/// How control leaves a `finally` when it completes: the departure the
/// `finally` interrupted (`K-FORM-014` to `K-FORM-017`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Departure {
    Normal,
    Break(LoopIx),
    Continue(LoopIx),
    Return(Reg),
    Throw(Reg),
}

/// One function body lowered to Kir.
#[derive(Clone, Debug, PartialEq)]
pub struct KirFunction {
    pub origin: Origin,
    /// Whether the body's forms are charged: false for the body of a
    /// library function with a native implementation (`K-CHG-007`).
    pub charged: bool,
    pub params: Box<[LocalRef]>,
    pub slots: Box<[SlotInfo]>,
    /// Flat; blocks are ranges of it.
    pub ops: Box<[Op]>,
    pub blocks: Box<[KBlock]>,
    /// Every step boundary of the machine in this body, with its static
    /// frame shape.
    pub boundaries: Box<[Boundary]>,
    /// The number of registers an activation uses.
    pub regs: u32,
    pub consts: Box<[Value]>,
    pub names: Box<[Name]>,
    pub fields: Box<[Box<str>]>,
    pub field_lists: Box<[Box<[FieldIx]>]>,
    pub codes: Box<[Origin]>,
    /// The library functions the body calls, by identity. A function's
    /// formula and guard plans are the library's, found through it.
    pub libs: Box<[FunctionId]>,
    pub stmts: Box<[Site]>,
    pub scopes: Box<[Scope]>,
    pub actions: Box<[KAction]>,
    pub loops: Box<[KLoop]>,
    pub tries: Box<[KTry]>,
    /// One per op: where it came from.
    pub origins: Box<[OpOrigin]>,
}

/// One kernel operation. `Op` names semantic operations, not machine
/// instructions: each has one meaning, written once against
/// [`crate::Sem`], and [`crate::SEMANTICS`] states its charge events and
/// runtime calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// A statement starts: its unit of charge (`K-CHG-002`), the step's
    /// pins and fresh objects reset, and the slice test.
    Stmt {
        boundary: BoundaryIx,
    },
    /// A step that starts no statement: a block's end, and the loop test or
    /// `finally` it leads to. It charges nothing itself.
    Step {
        boundary: BoundaryIx,
    },
    Const {
        dst: Reg,
        c: ConstIx,
    },
    Read {
        dst: Reg,
        var: VarRef,
    },
    Write {
        var: VarRef,
        src: Reg,
    },
    /// Declares a variable with its first value.
    Bind {
        local: LocalRef,
        src: Reg,
    },
    /// `K-FORM-009`.
    Field {
        dst: Reg,
        target: Reg,
        field: FieldIx,
    },
    /// `K-FORM-010`.
    Index {
        dst: Reg,
        target: Reg,
        index: Reg,
    },
    SetField {
        target: Reg,
        field: FieldIx,
        src: Reg,
    },
    SetIndex {
        target: Reg,
        index: Reg,
        src: Reg,
    },
    Remove {
        target: Reg,
        key: Reg,
    },
    MakeTuple {
        dst: Reg,
        items: RegRange,
    },
    MakeList {
        dst: Reg,
        items: RegRange,
    },
    /// `entries` holds key, value, key, value, ...
    MakeMap {
        dst: Reg,
        entries: RegRange,
    },
    MakeSet {
        dst: Reg,
        items: RegRange,
    },
    MakeRecord {
        dst: Reg,
        fields: FieldListIx,
        values: RegRange,
    },
    Closure {
        dst: Reg,
        code: CodeIx,
    },
    /// A library call inside an expression with a native implementation:
    /// guard, native, retry after a collection, pin and formula.
    CallNative {
        dst: Reg,
        lib: LibIx,
        args: RegRange,
    },
    /// A library call inside an expression whose native-backed body runs
    /// to its end inside the expression (`K-STMT-002`).
    CallInline {
        dst: Reg,
        lib: LibIx,
        args: RegRange,
    },
    Clock {
        dst: Reg,
    },
    Random {
        dst: Reg,
    },
    HostRead {
        dst: Reg,
        handle: Reg,
        request: Reg,
    },
    /// Raises `type_error` unless `cond` is a bool.
    Branch {
        cond: Reg,
        then_: KBlockIx,
        else_: KBlockIx,
    },
    Jump {
        to: KBlockIx,
    },
    /// Starts a `for` over `iterable`: raises `type_error` unless it is a
    /// list, a tuple, a map or a set.
    ForInit {
        lp: LoopIx,
        iterable: Reg,
    },
    /// The next element into `bind` and on to `body`, or `done`.
    ForNext {
        lp: LoopIx,
        bind: LocalRef,
        body: KBlockIx,
        done: KBlockIx,
    },
    /// Starts a `while`.
    WhileInit {
        lp: LoopIx,
    },
    /// The continuation test: raises `type_error` unless `cond` is a bool.
    WhileTest {
        lp: LoopIx,
        cond: Reg,
        body: KBlockIx,
        done: KBlockIx,
    },
    /// Leaves a loop: removes its control.
    LoopExit {
        lp: LoopIx,
    },
    /// Enters a kernel block.
    Enter {
        scope: ScopeIx,
    },
    /// Leaves a kernel block: ends the variables it declared.
    Leave {
        scope: ScopeIx,
    },
    TryEnter {
        t: TryIx,
    },
    /// Takes the pending raise's error into `dst`: the first op of every
    /// block an exception edge reaches.
    Caught {
        dst: Reg,
    },
    /// A handler starts: the `try`'s phase becomes `catch`.
    CatchEnter {
        t: TryIx,
    },
    FinallyEnter {
        t: TryIx,
        departure: Departure,
    },
    /// A `finally` completed: the departure it interrupted resumes, or
    /// control goes on to `normal`.
    FinallyEnd {
        t: TryIx,
        normal: KBlockIx,
    },
    /// Leaves a `try` that completed normally: removes its control.
    TryExit {
        t: TryIx,
    },
    /// An action statement starts: the activation exits to the machine,
    /// and the action's result, if it has one, comes back in `dst` at
    /// `resume`.
    Act {
        dst: Reg,
        action: ActionIx,
        boundary: BoundaryIx,
        resume: KBlockIx,
    },
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
    /// A fixed charge anchor (`K-CHG-002`), in the interpreter's order.
    Charge {
        units: u64,
        event: crate::ChargeEvent,
    },
}

/// The kind of an [`Op`], without its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpKind {
    Stmt,
    Step,
    Const,
    Read,
    Write,
    Bind,
    Field,
    Index,
    SetField,
    SetIndex,
    Remove,
    MakeTuple,
    MakeList,
    MakeMap,
    MakeSet,
    MakeRecord,
    Closure,
    CallNative,
    CallInline,
    Clock,
    Random,
    HostRead,
    Branch,
    Jump,
    ForInit,
    ForNext,
    WhileInit,
    WhileTest,
    LoopExit,
    Enter,
    Leave,
    TryEnter,
    Caught,
    CatchEnter,
    FinallyEnter,
    FinallyEnd,
    TryExit,
    Act,
    Return,
    Throw,
    Print,
    Finish,
    Fail,
    Charge,
}

impl OpKind {
    /// Every kind, in declaration order: the order the ABI digest hashes.
    pub const ALL: &'static [Self] = &[
        Self::Stmt,
        Self::Step,
        Self::Const,
        Self::Read,
        Self::Write,
        Self::Bind,
        Self::Field,
        Self::Index,
        Self::SetField,
        Self::SetIndex,
        Self::Remove,
        Self::MakeTuple,
        Self::MakeList,
        Self::MakeMap,
        Self::MakeSet,
        Self::MakeRecord,
        Self::Closure,
        Self::CallNative,
        Self::CallInline,
        Self::Clock,
        Self::Random,
        Self::HostRead,
        Self::Branch,
        Self::Jump,
        Self::ForInit,
        Self::ForNext,
        Self::WhileInit,
        Self::WhileTest,
        Self::LoopExit,
        Self::Enter,
        Self::Leave,
        Self::TryEnter,
        Self::Caught,
        Self::CatchEnter,
        Self::FinallyEnter,
        Self::FinallyEnd,
        Self::TryExit,
        Self::Act,
        Self::Return,
        Self::Throw,
        Self::Print,
        Self::Finish,
        Self::Fail,
        Self::Charge,
    ];

    /// Whether ops of this kind end a basic block.
    pub const fn is_terminator(self) -> bool {
        matches!(
            self,
            Self::Branch
                | Self::Jump
                | Self::ForNext
                | Self::WhileTest
                | Self::FinallyEnd
                | Self::Act
                | Self::Return
                | Self::Throw
                | Self::Finish
                | Self::Fail
        )
    }
}

/// An index an op holds, by the table it points into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operand {
    Reg(Reg),
    Regs(RegRange),
    Local(LocalRef),
    Var(VarRef),
    Block(KBlockIx),
    Boundary(BoundaryIx),
    Const(ConstIx),
    Field(FieldIx),
    FieldList(FieldListIx),
    Code(CodeIx),
    Lib(LibIx),
    Loop(LoopIx),
    Try(TryIx),
    Scope(ScopeIx),
    Action(ActionIx),
}

impl Op {
    pub const fn kind(&self) -> OpKind {
        match self {
            Self::Stmt { .. } => OpKind::Stmt,
            Self::Step { .. } => OpKind::Step,
            Self::Const { .. } => OpKind::Const,
            Self::Read { .. } => OpKind::Read,
            Self::Write { .. } => OpKind::Write,
            Self::Bind { .. } => OpKind::Bind,
            Self::Field { .. } => OpKind::Field,
            Self::Index { .. } => OpKind::Index,
            Self::SetField { .. } => OpKind::SetField,
            Self::SetIndex { .. } => OpKind::SetIndex,
            Self::Remove { .. } => OpKind::Remove,
            Self::MakeTuple { .. } => OpKind::MakeTuple,
            Self::MakeList { .. } => OpKind::MakeList,
            Self::MakeMap { .. } => OpKind::MakeMap,
            Self::MakeSet { .. } => OpKind::MakeSet,
            Self::MakeRecord { .. } => OpKind::MakeRecord,
            Self::Closure { .. } => OpKind::Closure,
            Self::CallNative { .. } => OpKind::CallNative,
            Self::CallInline { .. } => OpKind::CallInline,
            Self::Clock { .. } => OpKind::Clock,
            Self::Random { .. } => OpKind::Random,
            Self::HostRead { .. } => OpKind::HostRead,
            Self::Branch { .. } => OpKind::Branch,
            Self::Jump { .. } => OpKind::Jump,
            Self::ForInit { .. } => OpKind::ForInit,
            Self::ForNext { .. } => OpKind::ForNext,
            Self::WhileInit { .. } => OpKind::WhileInit,
            Self::WhileTest { .. } => OpKind::WhileTest,
            Self::LoopExit { .. } => OpKind::LoopExit,
            Self::Enter { .. } => OpKind::Enter,
            Self::Leave { .. } => OpKind::Leave,
            Self::TryEnter { .. } => OpKind::TryEnter,
            Self::Caught { .. } => OpKind::Caught,
            Self::CatchEnter { .. } => OpKind::CatchEnter,
            Self::FinallyEnter { .. } => OpKind::FinallyEnter,
            Self::FinallyEnd { .. } => OpKind::FinallyEnd,
            Self::TryExit { .. } => OpKind::TryExit,
            Self::Act { .. } => OpKind::Act,
            Self::Return { .. } => OpKind::Return,
            Self::Throw { .. } => OpKind::Throw,
            Self::Print { .. } => OpKind::Print,
            Self::Finish { .. } => OpKind::Finish,
            Self::Fail { .. } => OpKind::Fail,
            Self::Charge { .. } => OpKind::Charge,
        }
    }

    /// The boundary this op is the program point of: a step's start, or
    /// an action's call-return or wait-resume point.
    pub const fn boundary(&self) -> Option<BoundaryIx> {
        match self {
            Self::Stmt { boundary } | Self::Step { boundary } | Self::Act { boundary, .. } => {
                Some(*boundary)
            }
            _ => None,
        }
    }

    /// The blocks control can go to from this op without a raise. A
    /// `FinallyEnd` also resumes the interrupted departure, whose target
    /// is the loop's or the activation's, not a block of its own.
    pub fn successors(&self) -> Vec<KBlockIx> {
        match *self {
            Self::Branch { then_, else_, .. } => vec![then_, else_],
            Self::Jump { to } => vec![to],
            Self::ForNext { body, done, .. } | Self::WhileTest { body, done, .. } => {
                vec![body, done]
            }
            Self::FinallyEnd { normal, .. } => vec![normal],
            Self::Act { resume, .. } => vec![resume],
            _ => Vec::new(),
        }
    }

    /// Every index the op holds, in field order.
    pub fn operands(&self) -> Vec<Operand> {
        use Operand as O;
        match *self {
            Self::Stmt { boundary } | Self::Step { boundary } => vec![O::Boundary(boundary)],
            Self::Const { dst, c } => vec![O::Reg(dst), O::Const(c)],
            Self::Read { dst, var } => vec![O::Reg(dst), O::Var(var)],
            Self::Write { var, src } => vec![O::Var(var), O::Reg(src)],
            Self::Bind { local, src } => vec![O::Local(local), O::Reg(src)],
            Self::Field { dst, target, field } => {
                vec![O::Reg(dst), O::Reg(target), O::Field(field)]
            }
            Self::Index { dst, target, index } => vec![O::Reg(dst), O::Reg(target), O::Reg(index)],
            Self::SetField { target, field, src } => {
                vec![O::Reg(target), O::Field(field), O::Reg(src)]
            }
            Self::SetIndex { target, index, src } => {
                vec![O::Reg(target), O::Reg(index), O::Reg(src)]
            }
            Self::Remove { target, key } => vec![O::Reg(target), O::Reg(key)],
            Self::MakeTuple { dst, items }
            | Self::MakeList { dst, items }
            | Self::MakeSet { dst, items }
            | Self::MakeMap {
                dst,
                entries: items,
            } => vec![O::Reg(dst), O::Regs(items)],
            Self::MakeRecord {
                dst,
                fields,
                values,
            } => vec![O::Reg(dst), O::FieldList(fields), O::Regs(values)],
            Self::Closure { dst, code } => vec![O::Reg(dst), O::Code(code)],
            Self::CallNative { dst, lib, args } | Self::CallInline { dst, lib, args } => {
                vec![O::Reg(dst), O::Lib(lib), O::Regs(args)]
            }
            Self::Clock { dst } | Self::Random { dst } => vec![O::Reg(dst)],
            Self::HostRead {
                dst,
                handle,
                request,
            } => vec![O::Reg(dst), O::Reg(handle), O::Reg(request)],
            Self::Branch { cond, then_, else_ } => {
                vec![O::Reg(cond), O::Block(then_), O::Block(else_)]
            }
            Self::Jump { to } => vec![O::Block(to)],
            Self::ForInit { lp, iterable } => vec![O::Loop(lp), O::Reg(iterable)],
            Self::ForNext {
                lp,
                bind,
                body,
                done,
            } => vec![O::Loop(lp), O::Local(bind), O::Block(body), O::Block(done)],
            Self::WhileInit { lp } | Self::LoopExit { lp } => vec![O::Loop(lp)],
            Self::WhileTest {
                lp,
                cond,
                body,
                done,
            } => vec![O::Loop(lp), O::Reg(cond), O::Block(body), O::Block(done)],
            Self::Enter { scope } | Self::Leave { scope } => vec![O::Scope(scope)],
            Self::TryEnter { t } | Self::TryExit { t } | Self::CatchEnter { t } => {
                vec![O::Try(t)]
            }
            Self::Caught { dst } => vec![O::Reg(dst)],
            Self::FinallyEnter { t, departure } => {
                let mut out = vec![O::Try(t)];
                match departure {
                    Departure::Normal => {}
                    Departure::Break(lp) | Departure::Continue(lp) => out.push(O::Loop(lp)),
                    Departure::Return(reg) | Departure::Throw(reg) => out.push(O::Reg(reg)),
                }
                out
            }
            Self::FinallyEnd { t, normal } => vec![O::Try(t), O::Block(normal)],
            Self::Act {
                dst,
                action,
                boundary,
                resume,
            } => vec![
                O::Reg(dst),
                O::Action(action),
                O::Boundary(boundary),
                O::Block(resume),
            ],
            Self::Return { src }
            | Self::Throw { src }
            | Self::Print { src }
            | Self::Finish { src }
            | Self::Fail { src } => vec![O::Reg(src)],
            Self::Charge { .. } => Vec::new(),
        }
    }
}
