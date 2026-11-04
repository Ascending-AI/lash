//! Ephemeral, source-bound region programs. Only verification may seal code.
#![allow(
    dead_code,
    reason = "ISA integration interfaces precede lane C execution"
)]

use super::room::region::calls::region_isa::CallPlan;
use super::room::{Refused, Source};
use super::{CodeId, Executable};
use lash_kernel_doc::Value;
use std::{convert::Infallible, marker::PhantomData, sync::Arc};

mod recovery;
pub(crate) use recovery::RecoveryMap;

pub(crate) struct RegionDraft {
    code: CodeId,
    source: Arc<Source>,
    ops: Box<[RegionOp]>,
    recovery: Box<[RecoveryMap]>,
}

pub(crate) struct RegionCode {
    sealed: Infallible,
    entry: Pc,
    ops: Box<[RegionOp]>,
    recovery: Box<[RecoveryMap]>,
    calls: Box<[CallPlan]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reg<T> {
    index: u32,
    kind: PhantomData<fn() -> T>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pc(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrefixId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallId(u32);

pub(crate) struct IntKernel {
    sealed: Infallible,
}
pub(crate) struct CheckedKernel {
    sealed: Infallible,
}
pub(crate) struct ReadSpec {
    sealed: Infallible,
}
pub(crate) struct BoundarySpec {
    sealed: Infallible,
}

pub(crate) enum RegionOp {
    Constant {
        dst: Reg<Value>,
        value: Value,
        next: Pc,
    },
    Move {
        dst: Reg<Value>,
        src: Reg<Value>,
        next: Pc,
    },
    IntBinary {
        kernel: IntKernel,
        lhs: Reg<i64>,
        rhs: Reg<i64>,
        dst: Reg<i64>,
        next: Pc,
    },
    Read {
        spec: ReadSpec,
        base: Reg<Value>,
        dst: Reg<Value>,
        next: Pc,
    },
    Checked {
        kernel: CheckedKernel,
        args: Box<[Reg<Value>]>,
        dst: Reg<Value>,
        next: Pc,
    },
    Branch {
        test: Reg<bool>,
        yes: Pc,
        no: Pc,
    },
    Call {
        site: CallId,
        next: Pc,
    },
    Return {
        value: Reg<Value>,
    },
    Boundary {
        spec: BoundarySpec,
    },
    Exit {
        prefix: PrefixId,
    },
}

pub(crate) fn compile_region(_exe: &Executable, _code: CodeId) -> Result<RegionDraft, Refused> {
    Err(Refused::Source)
}

pub(crate) fn verify_region(
    _exe: &Executable,
    _code: CodeId,
    _draft: RegionDraft,
    _calls: &[CallPlan],
) -> Result<Arc<RegionCode>, Refused> {
    Err(Refused::Source)
}

impl RegionDraft {
    pub(crate) fn has_calls(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op, RegionOp::Call { .. }))
    }
}

impl RegionCode {
    pub(crate) fn entry(&self) -> Pc {
        self.entry
    }
    pub(crate) fn op(&self, pc: Pc) -> Option<&RegionOp> {
        self.ops.get(pc.0 as usize)
    }
    pub(crate) fn recovery(&self, prefix: PrefixId) -> Option<&RecoveryMap> {
        self.recovery.get(prefix.0 as usize)
    }
    pub(crate) fn call(&self, site: CallId) -> Option<&CallPlan> {
        self.calls.get(site.0 as usize)
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod fixtures {
    use super::*;
    use crate::compile::room::Point;
    /// An untrusted draft only. It cannot mint RegionCode or a kernel.
    pub(crate) fn exit(exe: &Executable, code: CodeId, continuation: Point) -> RegionDraft {
        RegionDraft {
            code,
            source: Arc::clone(&exe.code(code).source_room),
            ops: Box::new([RegionOp::Exit {
                prefix: PrefixId(0),
            }]),
            recovery: Box::new([RecoveryMap {
                roles: Box::new([]),
                operands: Box::new([]),
                pending: None,
                continuation,
            }]),
        }
    }
}
