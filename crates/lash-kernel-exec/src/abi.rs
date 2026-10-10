//! The runtime ABI between compiled code and the machine.
//!
//! Compiled code talks to the machine through one `#[repr(C)]`
//! [`RtContext`] and one allowlisted table of runtime functions, one slot
//! per [`RtFn`]. It can call only those slots and entries of its own image:
//! no name is looked up at runtime. Values never cross the ABI by pointer;
//! runtime functions read and write registers by index and take heap
//! objects by id. Every layout here is part of [`crate::EXEC_ABI_DIGEST`].

use std::ffi::c_void;
use std::sync::atomic::AtomicU32;

/// A register's raw contents, private to the machine. Stage 1 code never
/// reads it: only runtime functions interpret it. The optimized tier tests
/// `tag` inline and reads small payloads (null, absent, bool, small int,
/// float bits, and the object id of a list, map, set, record or closure);
/// every other value is an index into a machine-owned side table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct RawSlot {
    pub tag: u8,
    pub _pad: [u8; 7],
    pub payload: u64,
}

/// What compiled code writes before it returns to the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ExitRecord {
    pub kind: ExitKind,
    /// The Boundary the exit is taken at.
    pub boundary: u32,
    /// The [`crate::FrameMap`] that materializes the frame there.
    pub map: u32,
    /// The register holding the returned or raised value.
    pub value_reg: u32,
    /// Exit-specific: the watchdog code of a `Fault`.
    pub aux: u64,
}

/// Why compiled code returned to the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum ExitKind {
    /// The activation returned `value_reg`: the machine leaves the frame.
    Return = 0,
    /// A raise left the activation: no `try` of this frame took it.
    Raise = 1,
    /// An action statement started: call, perform, sleep, join, spawn,
    /// cancel or yield. The frame is canonical and awaits it.
    Act = 2,
    /// The run's slice ended at a Boundary.
    Slice = 3,
    /// A bound, `finish` or `fail` ended the run; the machine already holds
    /// the halt.
    Halt = 4,
    /// A speculative guard failed at a Boundary before any of that
    /// statement's effects (the optimized tier only).
    Deopt = 5,
    /// The watchdog tripped, or an engine fault: never guest-visible.
    Fault = 6,
}

impl ExitKind {
    pub const ALL: &'static [Self] = &[
        Self::Return,
        Self::Raise,
        Self::Act,
        Self::Slice,
        Self::Halt,
        Self::Deopt,
        Self::Fault,
    ];

    /// Whether the frame must be materialized at the exit: every exit that
    /// leaves the activation alive at a Boundary.
    pub const fn materializes(self) -> bool {
        matches!(self, Self::Act | Self::Slice | Self::Deopt)
    }
}

/// What a runtime function returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum RtStatus {
    Ok = 0,
    /// A loop has no next element.
    Done = 1,
    /// A raise is pending in the machine.
    Raise = 2,
    /// The run halted; the machine holds the halt.
    Halt = 3,
}

impl RtStatus {
    pub const ALL: &'static [Self] = &[Self::Ok, Self::Done, Self::Raise, Self::Halt];
}

/// The context of one compiled activation.
#[derive(Debug)]
#[repr(C)]
pub struct RtContext {
    /// The machine, opaque to compiled code.
    pub machine: *mut c_void,
    /// The allowlisted runtime functions.
    pub rt: *const RtTable,
    /// This activation's register window, owned by the machine.
    pub regs: *mut RawSlot,
    /// The machine's charge while compiled code runs.
    pub charged: u64,
    /// The run's charge bound.
    pub charge_bound: u64,
    /// The run's start charge plus its slice, saturating.
    pub slice_at: u64,
    /// Whether the running code is charged (`K-CHG-007`).
    pub charging: u32,
    /// The watchdog flag: nonzero exits `Fault`. Never charge.
    pub interrupt: *const AtomicU32,
    pub exit: ExitRecord,
}

/// A slot of the runtime table, type-erased: compiled code calls it with
/// the signature [`RtFn::signature`] states.
pub type RtEntry = unsafe extern "C" fn();

/// The allowlisted runtime functions, one slot per [`RtFn`] in its order.
#[derive(Debug)]
#[repr(C)]
pub struct RtTable {
    pub entries: [RtEntry; RtFn::ALL.len()],
}

/// A compiled function's entry: `entry` 0 is activation entry, `n` the
/// continuation of its `n`-th action site.
pub type CompiledEntry = unsafe extern "C" fn(ctx: *mut RtContext, entry: u32) -> ExitKind;

/// A runtime function's group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RtGroup {
    /// Charge, reservation and step anchors, emitted by the code generator
    /// around every op.
    Anchor,
    Slot,
    Heap,
    Control,
    Call,
    Host,
    /// Measures of values that anchors take as their amounts.
    Value,
    /// Exits and raises, emitted by the code generator.
    Exit,
}

/// A parameter or result type of a runtime function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AbiTy {
    /// `*mut RtContext`, always first.
    Ctx,
    /// A register index.
    Reg,
    U32,
    U64,
    /// An [`RtStatus`].
    Status,
}

/// A runtime function's signature: `ctx` first, then its arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RtSig {
    pub params: &'static [AbiTy],
    pub ret: AbiTy,
}

macro_rules! rt_fns {
    ($($group:ident { $($(#[$meta:meta])* $name:ident ($($param:ident),*) -> $ret:ident;)* })*) => {
        /// A runtime function of the allowlisted table.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum RtFn {
            $($($(#[$meta])* $name,)*)*
        }

        impl RtFn {
            /// Every runtime function, in table order.
            pub const ALL: &'static [Self] = &[$($(Self::$name,)*)*];

            pub const fn group(self) -> RtGroup {
                match self {
                    $($(Self::$name => RtGroup::$group,)*)*
                }
            }

            pub const fn signature(self) -> RtSig {
                match self {
                    $($(Self::$name => RtSig {
                        params: &[AbiTy::Ctx, $(AbiTy::$param),*],
                        ret: AbiTy::$ret,
                    },)*)*
                }
            }
        }
    };
}

rt_fns! {
    Anchor {
        /// The precise saturating add and bound test.
        ChargeSlow(U64) -> Status;
        /// A memory reservation, which may collect.
        Reserve(U64) -> Status;
        /// Roots a register's value for the step and reserves its size.
        Pin(Reg) -> Status;
        /// A step starts: the step's pins and fresh objects are dropped.
        StepBegin() -> Status;
    }
    Slot {
        /// Reads the slot at a frame position (through its cell when shared).
        SlotRead(Reg, U32) -> Status;
        /// Writes a declared variable; raises `unbound_variable` when its
        /// slot is empty. The value's reservation is the `Reserve` before it.
        SlotWrite(U32, Reg) -> Status;
        /// Declares a variable with its first value: a new cell when shared,
        /// otherwise in place after the `Reserve` before it.
        SlotBind(U32, Reg) -> Status;
        /// Ends the variables of a scope.
        ScopeEnd(U32) -> Status;
        SessionRead(Reg, U32) -> Status;
        SessionWrite(U32, Reg) -> Status;
    }
    Heap {
        /// `dst`, first member register, member count.
        AllocTuple(Reg, Reg, U32) -> Status;
        AllocList(Reg, Reg, U32) -> Status;
        AllocMap(Reg, Reg, U32) -> Status;
        AllocSet(Reg, Reg, U32) -> Status;
        /// `dst`, field list, first value register, value count.
        AllocRecord(Reg, U32, Reg, U32) -> Status;
        RecordField(Reg, Reg, U32) -> Status;
        ErrorField(Reg, Reg, U32) -> Status;
        Index(Reg, Reg, Reg) -> Status;
        WriteField(Reg, U32, Reg) -> Status;
        WriteIndex(Reg, Reg, Reg) -> Status;
        RemoveMember(Reg, Reg) -> Status;
        ClosureNew(Reg, U32) -> Status;
    }
    Control {
        /// Pushes a static control entry: its tag and table index.
        ControlPush(U32, U32) -> Status;
        ControlPop() -> Status;
        ForStart(U32, Reg) -> Status;
        /// The next element into a frame position, or `Done`.
        ForNext(U32, U32) -> Status;
        /// A handler starts: the try's phase becomes `catch`.
        TryCatch(U32) -> Status;
        /// A `finally` starts: the try, the departure's tag and operand.
        FinallyEnter(U32, U32, U32) -> Status;
        /// A `finally` completed: the departure it resumes, encoded.
        FinallyEnd(U32) -> U64;
    }
    Call {
        /// The whole of a native call inside an expression.
        CallNative(U32, Reg, Reg) -> Status;
        /// A native-backed body run inside an expression.
        CallInline(U32, Reg, Reg) -> Status;
        /// A library function's formula (0) or guard limit (1) over the
        /// argument registers and the result register.
        Formula(U32, U32, Reg, Reg) -> U64;
        ResolveCallee(Reg) -> Status;
        /// Pushes a frame: depth test and reservation.
        PushFrame(U32, Reg, U32) -> Status;
    }
    Host {
        Clock(Reg) -> Status;
        Random(Reg) -> Status;
        HostRead(Reg, Reg, Reg) -> Status;
        DeepSize(Reg) -> U64;
        Print(Reg) -> Status;
        CopyOut(Reg) -> Status;
    }
    Value {
        /// A value's reserved size, as a binding or a write reserves it.
        ValueBytes(Reg) -> U64;
    }
    Exit {
        /// Writes a Boundary's frame: the map, the spill area.
        Materialize(U32, U32) -> Status;
        /// Raises a typed error: its kind and message.
        RaiseAt(U32, U32) -> Status;
        RaiseValue(Reg) -> Status;
        /// Takes the pending raise's error into a register: the first work
        /// of every exception edge's target.
        TakeRaise(Reg) -> Status;
        /// Records `finish` (0) or `fail` (1) with the copied-out value.
        HaltWith(U32, Reg) -> Status;
    }
}
