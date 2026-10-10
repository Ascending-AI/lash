//! The operation semantics: what each [`Op`] means, written once.
//!
//! Each op's meaning is a function generic over [`Sem`]. Three
//! instantiations run it: concrete (the interpreter executes it), symbolic
//! (it emits machine code) and abstract (it propagates facts for partial
//! evaluation). The op function fixes the order of charges, checks,
//! reservations, raises and host calls once, so no tier can reorder them.
//!
//! [`SEMANTICS`] declares, per op kind, the charge events its semantics
//! emits itself (`K-CHG-002`, in order) and the runtime functions it may
//! call. [`crate::verify_semantics`] checks that it covers every op, that
//! every runtime function of the table is reached by some op, and that
//! every dynamic charge event has an op that emits it.
//!
//! [`Op`]: crate::Op

use lash_kernel_doc::ValueKind;

use crate::abi::{ExitKind, RtFn};
use crate::kir::{BoundaryIx, LocalRef, MsgIx, OpKind, Reg};

/// How an op's semantics leaves it: on to the next op, or out of the
/// activation with an exit. A raise that a `try` of this frame takes
/// follows the block's exception edge instead of leaving.
pub type Flow = Result<(), ExitKind>;

/// A set of value kinds, one bit per [`ValueKind`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KindSet(pub u32);

impl KindSet {
    pub const NONE: Self = Self(0);
    pub const NULL: Self = Self::of(ValueKind::Null);
    pub const ABSENT: Self = Self::of(ValueKind::Absent);
    pub const BOOL: Self = Self::of(ValueKind::Bool);
    pub const INT: Self = Self::of(ValueKind::Int);
    pub const FLOAT: Self = Self::of(ValueKind::Float);
    pub const TEXT: Self = Self::of(ValueKind::Text);
    pub const BYTES: Self = Self::of(ValueKind::Bytes);
    pub const TIMESTAMP: Self = Self::of(ValueKind::Timestamp);
    pub const TUPLE: Self = Self::of(ValueKind::Tuple);
    pub const LIST: Self = Self::of(ValueKind::List);
    pub const MAP: Self = Self::of(ValueKind::Map);
    pub const SET: Self = Self::of(ValueKind::Set);
    pub const RECORD: Self = Self::of(ValueKind::Record);
    pub const CLOSURE: Self = Self::of(ValueKind::Closure);
    pub const ERROR: Self = Self::of(ValueKind::Error);
    pub const TASK: Self = Self::of(ValueKind::Task);
    pub const FUNCTION: Self = Self::of(ValueKind::Function);
    pub const HANDLE: Self = Self::of(ValueKind::Handle);
    pub const REF: Self = Self::of(ValueKind::Ref);
    /// Every kind.
    pub const ALL: Self = Self((1 << (ValueKind::Ref as u32 + 1)) - 1);

    pub const fn of(kind: ValueKind) -> Self {
        Self(1 << kind as u32)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, kind: ValueKind) -> bool {
        self.0 & Self::of(kind).0 != 0
    }
}

/// The kind of an error the machine raises itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ErrKind {
    TypeError,
    UnboundVariable,
    KeyMissing,
    InvalidKey,
    NumberRange,
    TooDeep,
    EffectResult,
    EmptyJoin,
    JoinSelf,
    Cancelled,
}

impl ErrKind {
    pub const ALL: &'static [Self] = &[
        Self::TypeError,
        Self::UnboundVariable,
        Self::KeyMissing,
        Self::InvalidKey,
        Self::NumberRange,
        Self::TooDeep,
        Self::EffectResult,
        Self::EmptyJoin,
        Self::JoinSelf,
        Self::Cancelled,
    ];

    /// The error's kind as a guest reads it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TypeError => "type_error",
            Self::UnboundVariable => "unbound_variable",
            Self::KeyMissing => "key_missing",
            Self::InvalidKey => "invalid_key",
            Self::NumberRange => "number_range",
            Self::TooDeep => "too_deep",
            Self::EffectResult => "effect_result",
            Self::EmptyJoin => "empty_join",
            Self::JoinSelf => "join_self",
            Self::Cancelled => "cancelled",
        }
    }
}

/// An argument of a runtime call.
pub enum RtArg<S: Sem + ?Sized> {
    Val(S::Val),
    Reg(Reg),
    U32(u32),
    U64(S::U64),
}

/// What a runtime call gives back: how the op goes on, or an amount for a
/// dynamic charge event.
pub enum RtRet<S: Sem + ?Sized> {
    Flow(Flow),
    Amount(S::U64),
}

/// What an op's semantics is written against.
///
/// - Concrete: `branch` runs one closure, `rt` calls the runtime function
///   directly, and `charge` is the machine's charge.
/// - Symbolic: `branch` emits two blocks, `rt` emits a call through the
///   table, and `charge` emits the anchor sequence.
/// - Abstract: values are facts, `branch` on a known bool follows one arm
///   and joins otherwise, and `charge` accumulates a residual charge.
pub trait Sem {
    type Val: Clone;
    type Bool;
    type Kind;
    type U64: From<u64>;

    /// A charge anchor (`K-CHG-002`, `K-CHG-003`, `K-CHG-008`).
    fn charge(&mut self, units: Self::U64) -> Flow;
    /// A memory anchor, which may collect.
    fn reserve(&mut self, bytes: Self::U64) -> Flow;
    /// Roots a value for the step and reserves its size.
    fn pin(&mut self, v: &Self::Val) -> Flow;
    fn kind(&mut self, v: &Self::Val) -> Self::Kind;
    fn kind_is(&mut self, k: &Self::Kind, want: KindSet) -> Self::Bool;
    fn branch<R>(
        &mut self,
        c: Self::Bool,
        t: impl FnOnce(&mut Self) -> R,
        e: impl FnOnce(&mut Self) -> R,
    ) -> R;
    /// An allowlisted runtime call.
    fn rt<const N: usize>(&mut self, f: RtFn, args: [RtArg<Self>; N]) -> RtRet<Self>;
    fn raise(&mut self, kind: ErrKind, message: MsgIx) -> Flow;
    fn reg(&mut self, r: Reg) -> Self::Val;
    fn set_reg(&mut self, r: Reg, v: Self::Val);
    fn slot(&mut self, l: LocalRef) -> Self::Val;
    fn set_slot(&mut self, l: LocalRef, v: Self::Val);
    /// The slice test at a Boundary, and the exit there.
    fn boundary(&mut self, b: BoundaryIx) -> Flow;
}

/// A charge event of `K-CHG-002`, as the interpreter orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChargeEvent {
    /// 1 per statement, before it runs.
    Statement,
    /// 1 per expression node, in pre-order, before its sub-expressions.
    ExprNode,
    /// 1 per call argument that is an atom.
    AtomArg,
    /// The members written by a construction, after they are evaluated and
    /// before the allocation.
    Construction,
    /// The cost table's loop test, before the test.
    LoopTest,
    /// A native call's formula, at its return (or with result size 0 when
    /// it raises) (`K-CHG-003`).
    NativeFormula,
    /// A value's deep size when it is copied out or decoded.
    CopyOut,
    /// A library call's formula when the native-backed body it ran ends.
    BodyFormula,
}

impl ChargeEvent {
    pub const ALL: &'static [Self] = &[
        Self::Statement,
        Self::ExprNode,
        Self::AtomArg,
        Self::Construction,
        Self::LoopTest,
        Self::NativeFormula,
        Self::CopyOut,
        Self::BodyFormula,
    ];

    /// Whether lowering knows the amount: a fixed event may be an explicit
    /// [`crate::Op::Charge`]; a dynamic one is a runtime amount.
    pub const fn is_fixed(self) -> bool {
        matches!(
            self,
            Self::Statement | Self::ExprNode | Self::AtomArg | Self::Construction | Self::LoopTest
        )
    }
}

/// One op kind's declared semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpSpec {
    pub op: OpKind,
    /// The charge events the op's semantics emits itself, in order. Fixed
    /// events of its sub-expressions and the loop test are explicit
    /// `Charge` ops that lowering places before them.
    pub events: &'static [ChargeEvent],
    /// The runtime functions the op's semantics may call.
    pub calls: &'static [RtFn],
    /// Whether the op can raise, halt (other than by its own charge),
    /// reserve memory or call the host or a native: charges never
    /// coalesce across it (§4.2).
    pub fallible: bool,
}

const fn spec(
    op: OpKind,
    events: &'static [ChargeEvent],
    calls: &'static [RtFn],
    fallible: bool,
) -> OpSpec {
    OpSpec {
        op,
        events,
        calls,
        fallible,
    }
}

use ChargeEvent as E;
use OpKind as K;
use RtFn as R;

/// Every op kind's declared semantics, in [`OpKind::ALL`] order.
pub const SEMANTICS: &[OpSpec] = &[
    spec(
        K::Stmt,
        &[E::Statement],
        &[R::StepBegin, R::Materialize],
        true,
    ),
    spec(K::Step, &[], &[R::StepBegin, R::Materialize], false),
    spec(K::Const, &[], &[], false),
    spec(
        K::Read,
        &[],
        &[R::SlotRead, R::SessionRead, R::RaiseAt],
        true,
    ),
    spec(
        K::Write,
        &[],
        &[R::ValueBytes, R::Reserve, R::SlotWrite, R::SessionWrite],
        true,
    ),
    spec(
        K::Bind,
        &[],
        &[R::ValueBytes, R::Reserve, R::SlotBind],
        true,
    ),
    spec(
        K::Field,
        &[],
        &[R::RecordField, R::ErrorField, R::RaiseAt],
        true,
    ),
    spec(K::Index, &[], &[R::Index, R::RaiseAt], true),
    spec(K::SetField, &[], &[R::Pin, R::WriteField, R::RaiseAt], true),
    spec(K::SetIndex, &[], &[R::Pin, R::WriteIndex, R::RaiseAt], true),
    spec(K::Remove, &[], &[R::RemoveMember, R::RaiseAt], true),
    spec(
        K::MakeTuple,
        &[E::Construction],
        &[R::AllocTuple, R::RaiseAt, R::Pin],
        true,
    ),
    spec(K::MakeList, &[E::Construction], &[R::AllocList], true),
    spec(
        K::MakeMap,
        &[E::Construction],
        &[R::AllocMap, R::RaiseAt],
        true,
    ),
    spec(
        K::MakeSet,
        &[E::Construction],
        &[R::AllocSet, R::RaiseAt],
        true,
    ),
    spec(K::MakeRecord, &[E::Construction], &[R::AllocRecord], true),
    spec(K::Closure, &[], &[R::ClosureNew], true),
    spec(K::CallNative, &[E::NativeFormula], &[R::CallNative], true),
    spec(
        K::CallInline,
        &[E::BodyFormula],
        &[R::CallInline, R::Formula],
        true,
    ),
    spec(K::Clock, &[], &[R::Clock], true),
    spec(K::Random, &[], &[R::Random], true),
    spec(K::HostRead, &[], &[R::HostRead, R::RaiseAt], true),
    spec(K::Branch, &[], &[R::RaiseAt], true),
    spec(K::Jump, &[], &[], false),
    spec(K::ForInit, &[], &[R::ForStart, R::RaiseAt], true),
    spec(K::ForNext, &[], &[R::ForNext], true),
    spec(K::WhileInit, &[], &[R::ControlPush], false),
    spec(K::WhileTest, &[], &[R::ControlPop, R::RaiseAt], true),
    spec(K::LoopExit, &[], &[R::ControlPop], false),
    spec(K::Enter, &[], &[R::ControlPush], false),
    spec(K::Leave, &[], &[R::ScopeEnd, R::ControlPop], false),
    spec(K::TryEnter, &[], &[R::ControlPush], false),
    spec(K::Caught, &[], &[R::TakeRaise], false),
    spec(K::CatchEnter, &[], &[R::TryCatch], false),
    spec(K::FinallyEnter, &[], &[R::FinallyEnter], false),
    spec(
        K::FinallyEnd,
        &[],
        &[R::FinallyEnd, R::ControlPop, R::RaiseValue],
        true,
    ),
    spec(K::TryExit, &[], &[R::ControlPop], false),
    spec(
        K::Act,
        &[],
        &[R::ResolveCallee, R::PushFrame, R::Materialize, R::Pin],
        true,
    ),
    spec(
        K::Return,
        &[E::CopyOut, E::BodyFormula],
        &[R::DeepSize, R::CopyOut, R::Formula],
        true,
    ),
    spec(K::Throw, &[], &[R::RaiseValue], true),
    spec(K::Print, &[E::CopyOut], &[R::DeepSize, R::Print], true),
    spec(K::Finish, &[E::CopyOut], &[R::DeepSize, R::HaltWith], true),
    spec(K::Fail, &[E::CopyOut], &[R::DeepSize, R::HaltWith], true),
    spec(K::Charge, &[], &[R::ChargeSlow], false),
];
