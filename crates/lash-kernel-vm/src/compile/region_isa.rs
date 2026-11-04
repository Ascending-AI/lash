//! Ephemeral, source-bound region programs. Only verification may seal code.
#![allow(
    dead_code,
    reason = "ISA integration interfaces precede lane C execution"
)]

use super::room::region::calls::region_isa::CallPlan;
use super::room::{Refused, Source};
use super::{CodeId, Executable, StmtId};
use lash_kernel_doc::Value;
use lash_kernel_state::kernel_internals::logical::LeaseValue;
use std::{convert::Infallible, marker::PhantomData, sync::Arc};

mod recovery;
pub(crate) use recovery::{GraphRecovery, RecoveryMap};

pub(crate) struct RegionDraft {
    code: CodeId,
    source: Arc<Source>,
    ops: Box<[RegionOp]>,
    recovery: Box<[RecoveryMap]>,
    call_sources: Box<[(StmtId, u32)]>,
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
impl<T> Reg<T> {
    pub(crate) fn index(&self) -> u32 {
        self.index
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pc(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrefixId(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallId(u32);
#[cfg(test)]
impl CallId {
    pub(crate) fn fixture(index: u32) -> Self {
        Self(index)
    }
}

impl CallId {
    pub(crate) fn index(self) -> u32 {
        self.0
    }
}

pub(crate) struct IntKernel {
    sealed: Infallible,
}
pub(crate) struct CheckedKernel {
    sealed: Infallible,
}
pub(crate) struct HeapKernel {
    source: crate::compile::room::Point,
    phases: Box<[HeapPhase]>,
}
struct HeapPhase {
    source: crate::compile::room::Point,
    kind: HeapPhaseKind,
}
enum HeapPhaseKind {
    Allocate,
    Member,
    Grow,
    Hold,
    Release,
}

pub(crate) struct ReadSpec {
    sealed: Infallible,
}
pub(crate) struct BoundarySpec {
    sealed: Infallible,
}

pub(crate) enum RegionOp {
    Constant {
        dst: Reg<LeaseValue>,
        value: Value,
        next: Pc,
    },
    Move {
        dst: Reg<LeaseValue>,
        src: Reg<LeaseValue>,
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
        base: Reg<LeaseValue>,
        dst: Reg<LeaseValue>,
        next: Pc,
    },
    Checked {
        kernel: CheckedKernel,
        args: Box<[Reg<LeaseValue>]>,
        dst: Reg<LeaseValue>,
        next: Pc,
    },
    Heap {
        kernel: HeapKernel,
        args: Box<[Reg<LeaseValue>]>,
        dst: Option<Reg<LeaseValue>>,
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
        value: Reg<LeaseValue>,
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
    pub(crate) fn code(&self) -> CodeId {
        self.code
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    /// Untrusted source coordinates indexed by CallId. Call compilation
    /// authenticates each coordinate against the original executable.
    pub(crate) fn call_sources(&self) -> &[(StmtId, u32)] {
        &self.call_sources
    }
    pub(crate) fn ops(&self) -> &[RegionOp] {
        &self.ops
    }
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
                graph: GraphRecovery::fixture(&continuation),
                continuation,
            }]),
            call_sources: Box::new([]),
        }
    }
    /// Source coordinates remain untrusted; this builds no verified code.
    pub(crate) fn calls(
        exe: &Executable,
        code: CodeId,
        sites: &[(StmtId, u32)],
    ) -> Result<RegionDraft, Refused> {
        if code.0 as usize >= exe.code_count() {
            return Err(Refused::Source);
        }
        let end = u32::try_from(sites.len()).map_err(|_| Refused::Path)?;
        Ok(RegionDraft {
            code,
            source: Arc::clone(&exe.code(code).source_room),
            ops: (0..end)
                .map(|index| RegionOp::Call {
                    site: CallId(index),
                    next: Pc(index + 1),
                })
                .collect(),
            recovery: Box::new([]),
            call_sources: sites.into(),
        })
    }
    pub(crate) fn heap(exe: &Executable, code: CodeId, continuation: Point) -> RegionDraft {
        let mut draft = exit(exe, code, continuation.clone());
        draft.ops = Box::new([RegionOp::Heap {
            kernel: HeapKernel {
                source: continuation,
                phases: Box::new([]),
            },
            args: Box::new([]),
            dst: None,
            next: Pc(0),
        }]);
        draft
    }
}
