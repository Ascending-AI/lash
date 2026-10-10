//! The lash kernel's executor interface.
//!
//! The kernel keeps one durable meaning: documents, function definitions,
//! content identities, statement sites and parked runs. Below it, every
//! executable form is a disposable derivative built against this crate:
//!
//! - [`KirFunction`]: the executor IR, a flat CFG of semantic operations
//!   with an explicit [`Boundary`] at every step boundary of the machine;
//! - [`Sem`] and [`SEMANTICS`]: each op's meaning, written once and
//!   instantiated by every tier, with its charge events and runtime calls;
//! - [`RtContext`], [`RtFn`] and [`EXEC_ABI_DIGEST`]: the one context and
//!   the allowlisted runtime table compiled code talks to the machine
//!   through;
//! - [`FrameMap`]: how a compiled exit materializes the canonical frame,
//!   so parked-run capture and restore stay unchanged;
//! - [`ArtifactKey`]: what compiled code is derived from;
//! - [`verify_semantics`] and [`verify_function`]: the coverage invariants.
//!
//! The crate holds types and checks only; it depends on the document model
//! and no machine.

mod abi;
mod boundary;
mod digest;
pub mod fixtures;
mod key;
mod kir;
mod maps;
mod sem;
mod verify;

pub use abi::{
    AbiTy, CompiledEntry, ExitKind, ExitRecord, RawSlot, RtContext, RtEntry, RtFn, RtGroup, RtSig,
    RtStatus, RtTable,
};
pub use boundary::{Boundary, BoundaryKind, ControlRecipe, CtlStatic, TryPhaseStatic};
pub use digest::{EXEC_ABI_DIGEST, abi_description};
pub use key::{
    ArtifactKey, CodeFlags, Endian, KeyInput, LibRunKind, MapDetail, OptLevel, Subject, TargetId,
    VariantFactDigest,
};
pub use kir::{
    ActionIx, ActionKind, BoundaryIx, CalleeRef, CodeIx, ConstIx, Departure, FieldIx, FieldListIx,
    KAction, KBlock, KBlockIx, KLoop, KTry, KirFunction, LibIx, LocalRef, LoopIx, LoopKind, MsgIx,
    NameIx, Op, OpKind, OpOrigin, OpRange, Operand, Origin, Reg, RegRange, Scope, ScopeIx, SlotAt,
    SlotInfo, StmtIx, TryIx, VarRef,
};
pub use maps::{
    DepartLoc, FrameMap, Loc, LoopLoc, MAP_ENCODING_VERSION, MapDecodeError, baseline_maps,
    decode_maps, encode_maps,
};
pub use sem::{ChargeEvent, ErrKind, Flow, KindSet, OpSpec, RtArg, RtRet, SEMANTICS, Sem};
pub use verify::{At, CoverageError, KirError, verify_function, verify_semantics};

#[cfg(test)]
mod tests;
