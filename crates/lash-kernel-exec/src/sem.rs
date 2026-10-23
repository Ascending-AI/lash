//! The operation semantics' declaration: what each [`Op`] charges.
//!
//! [`SEMANTICS`] declares, per op kind, the charge events its meaning
//! emits itself (`K-CHG-002`, in order) and whether it can fail.
//! [`crate::verify_semantics`] checks that it covers every op and that every
//! dynamic charge event has an op that emits it. Each operation's meaning
//! itself is written once, as the machine method the interpreter executes
//! (`lash-kernel-vm`'s `machine::ops`).
//!
//! [`Op`]: crate::Op

use crate::kir::OpKind;

/// A charge event of `K-CHG-002`, as the interpreter orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChargeEvent {
    /// 1 per statement, before it runs: the machine's, as it starts the
    /// statement's range.
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

    /// Whether lowering knows the amount: a fixed event may stand in a
    /// charge region ([`crate::RegionTable`]); a dynamic one is a runtime
    /// amount.
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
    /// The charge events the op's semantics emits itself, in order. The
    /// fixed events of expression nodes and constructions are the charge
    /// region's, which its head admits.
    pub events: &'static [ChargeEvent],
    /// Whether the op can raise, halt (other than by its own charge),
    /// reserve memory or call the host or a native: charges never
    /// coalesce across it.
    pub fallible: bool,
}

const fn spec(op: OpKind, events: &'static [ChargeEvent], fallible: bool) -> OpSpec {
    OpSpec {
        op,
        events,
        fallible,
    }
}

use ChargeEvent as E;
use OpKind as K;

/// Every op kind's declared semantics, in [`OpKind::ALL`] order.
pub const SEMANTICS: &[OpSpec] = &[
    spec(K::Region, &[E::ExprNode, E::Construction], true),
    spec(K::Const, &[], false),
    spec(K::Local, &[], true),
    spec(K::Session, &[], true),
    spec(K::Unbound, &[], true),
    spec(K::Field, &[], true),
    spec(K::Index, &[], true),
    spec(K::MakeTuple, &[], true),
    spec(K::MakeList, &[], true),
    spec(K::MakeSet, &[], true),
    spec(K::MakeMap, &[], true),
    spec(K::MakeRecord, &[], true),
    spec(K::Closure, &[], true),
    spec(K::Call, &[E::NativeFormula, E::BodyFormula], true),
    spec(K::Operator, &[E::NativeFormula, E::BodyFormula], true),
    spec(K::Pin, &[], true),
    spec(
        K::Act,
        &[E::AtomArg, E::CopyOut, E::NativeFormula, E::BodyFormula],
        true,
    ),
    spec(
        K::Nested,
        &[E::CopyOut, E::NativeFormula, E::BodyFormula],
        true,
    ),
    spec(K::Bind, &[], true),
    spec(K::BindShared, &[], true),
    spec(K::BindSession, &[], true),
    spec(K::WriteLocal, &[], true),
    spec(K::WriteSession, &[], true),
    spec(K::WriteUnbound, &[], true),
    spec(K::SetField, &[], true),
    spec(K::SetIndex, &[], true),
    spec(K::RemoveField, &[], true),
    spec(K::RemoveIndex, &[], true),
    spec(K::Jump, &[], false),
    spec(K::JumpIfFalse, &[], true),
    spec(K::LetBind, &[], true),
    spec(K::LetBindShared, &[], true),
    spec(K::LetEnd, &[], false),
    spec(K::Branch, &[], true),
    spec(K::ForIn, &[E::LoopTest], true),
    spec(K::While, &[E::LoopTest], true),
    spec(K::Test, &[], true),
    spec(K::Try, &[], false),
    spec(K::Break, &[], false),
    spec(K::Continue, &[], false),
    spec(K::Return, &[E::CopyOut, E::BodyFormula], true),
    spec(
        K::ReturnVia,
        &[E::CopyOut, E::NativeFormula, E::BodyFormula],
        true,
    ),
    spec(K::Throw, &[], true),
    spec(K::Print, &[E::CopyOut], true),
    spec(K::Finish, &[E::CopyOut], true),
    spec(K::Fail, &[E::CopyOut], true),
    spec(K::End, &[], false),
];
