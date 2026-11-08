//! The memory schedule (cutover contract §2, `K-BND-003`, R7): what a run
//! holds, in logical bytes, as a fixed function of what is resident and
//! materialized.
//!
//! Every entry depends only on a holding's kind and logical length, never
//! on a capacity, a chunking or an allocation history, so two runs that
//! hold the same values hold the same amount whatever they did before. A
//! shared object counts once, by its slot. The unit is the logical byte the
//! native nursery reserves in ([`lash_kernel_doc::arena::native::Nursery`]):
//! a value is [`VALUE`] bytes.
//!
//! | holding | schedule |
//! |---|---|
//! | object | [`OBJECT`] `+ unit(kind) × logical length` |
//! | list, tuple, set | [`VALUE`] per member |
//! | map, record | `2 ×` [`VALUE`] per entry (key and value) |
//! | long text, bytes | one per byte |
//! | big integer, timestamp | one per magnitude byte |
//! | error | `3 ×` [`VALUE`] (kind, message, data) |
//! | cell | [`VALUE`] |
//! | closure | [`VALUE`] per captured cell |
//! | function reference | [`CALLABLE`] |
//! | host handle | one per byte of its kind and id |
//! | frame | [`FRAME`] `+` [`VALUE`] `× (locals + operand height)` |
//! | raw outcome, until decoded | its deep size ([`raw`]) |
//! | the instance's program | [`OP`] per op, [`CONSTANT`] per constant |
//! | resident directory or tree node | [`NODE`] |
//!
//! [`OBJECT`] includes one mark-stack word ([`MARK`]), so a collection
//! needs no memory the run was not admitted for.
//!
//! **Physical overhead.** The resident heap's physical bytes stay within
//! `physical ≤ A × logical + B`, with [`PHYSICAL_A`] and [`PHYSICAL_B`].
//! Every object is a 64-byte slot (generation and a 56-byte body) and a
//! 296-byte facts row, in vectors that double, so at most 720 physical
//! bytes per object against an [`OBJECT`] of 24: `A = 32` covers it, and
//! members, texts and chunks cost at most a constant factor besides. `B`
//! covers the empty machine's tables.
//!
//! Measured on the bytecode core with a counting allocator (2026-11-08),
//! physical over logical bytes held at a park, the empty machine's ~30 to
//! 90 KiB subtracted: one list of 100,000 values 0.98; one 1 MiB text
//! 1.03; 20,000 integers appended 1.76; 20,000 records of two fields 6.79;
//! 20,000 pairs 8.92; 20,000 long texts 9.63; 20,000 one-member lists
//! 11.47; 5,000 closures over a cell 13.18. Small objects are dominated by
//! the facts row; a slimmer row lowers `A`.

use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_doc::arena::{Body, ErrorBody};
use lash_kernel_doc::{Datum, Integer};

use crate::bytecode::program::{Program, Unit};

/// One value: an operand, a local, a member, a key.
pub(crate) const VALUE: u64 = 16;

/// One mark-stack word: each object's schedule holds the word a collection
/// pushes for it (contract §2).
pub(crate) const MARK: u64 = 8;

/// An object's fixed part: its slot header and its mark-stack word.
pub(crate) const OBJECT: u64 = 16 + MARK;

/// A function reference's callable identity (a 32-byte digest).
pub(crate) const CALLABLE: u64 = 32;

/// A frame's fixed part: unit, position, base, closure and owed formula.
pub(crate) const FRAME: u64 = 48;

/// One op of the instance's program, with its charge, pace, step and
/// suffix entries.
pub(crate) const OP: u64 = 32;

/// One entry of the instance's constant pool.
pub(crate) const CONSTANT: u64 = VALUE;

/// One resident directory or tree node.
pub(crate) const NODE: u64 = 256;

/// The physical-overhead factor: `physical ≤ PHYSICAL_A × logical +
/// PHYSICAL_B` (contract §2).
pub(crate) const PHYSICAL_A: u64 = 32;

/// The physical-overhead constant, in bytes.
pub(crate) const PHYSICAL_B: u64 = 1 << 17;

#[inline]
fn values(n: u64) -> u64 {
    VALUE.saturating_mul(n)
}

#[inline]
fn integer(int: &Integer) -> u64 {
    int.bits().div_ceil(8)
}

/// What a resident `body` holds.
pub(crate) fn body(body: &Body) -> u64 {
    let length = match body {
        Body::Free => return 0,
        Body::BigInt(int) | Body::Timestamp(int) => integer(int),
        Body::LongText(text) => text.summary.utf8_bytes,
        Body::Bytes(bytes) => bytes.len,
        Body::Tuple(sequence) | Body::List(sequence) => values(sequence.len),
        Body::Set(table) => values(table.len),
        Body::Map(table) | Body::Record(table) => values(table.len.saturating_mul(2)),
        Body::Error(ErrorBody { .. }) => values(3),
        Body::Function(_) => CALLABLE,
        Body::Handle(handle) => (handle.kind.len() + handle.id.len()) as u64,
        Body::Closure { captures, .. } => values(captures.len() as u64),
        Body::Cell(_) => VALUE,
    };
    OBJECT.saturating_add(length)
}

/// What `object` holds once placed. A map or record built from pairs that
/// repeat a key is admitted for every pair: that is its construction peak.
pub(crate) fn new_object(object: &NewObject) -> u64 {
    let length = match object {
        NewObject::List(members) | NewObject::Tuple(members) | NewObject::Set(members) => {
            values(members.len() as u64)
        }
        NewObject::Map(pairs) | NewObject::Record(pairs) => {
            values((pairs.len() as u64).saturating_mul(2))
        }
        NewObject::LongText(text) => text.len() as u64,
        NewObject::Bytes(bytes) => bytes.len() as u64,
        NewObject::BigInt(int) | NewObject::Timestamp(int) => integer(int),
        NewObject::Function(_) => CALLABLE,
        NewObject::Error { .. } => values(3),
    };
    OBJECT.saturating_add(length)
}

/// A cell (`K-CLO-001`).
pub(crate) const CELL: u64 = OBJECT + VALUE;

/// A closure over `captures` cells.
pub(crate) fn closure(captures: usize) -> u64 {
    OBJECT.saturating_add(values(captures as u64))
}

/// A long text of `bytes` UTF-8 bytes.
pub(crate) fn text(bytes: usize) -> u64 {
    OBJECT.saturating_add(bytes as u64)
}

/// A frame of `unit` whose operands reach `height` above its locals.
pub(crate) fn frame(unit: &Unit, height: u32) -> u64 {
    FRAME.saturating_add(values(u64::from(unit.locals) + u64::from(height)))
}

/// The instance's own program (`K-MACH-001`).
pub(crate) fn program(program: &Program) -> u64 {
    let ops: u64 = program.units.iter().map(|unit| unit.ops.len() as u64).sum();
    OP.saturating_mul(ops)
        .saturating_add(CONSTANT.saturating_mul(program.constants.len() as u64))
}

/// A raw outcome the host delivered, held until its consumption decodes it:
/// its deep size, each scalar one value and each text or byte string its
/// length besides.
pub(crate) fn raw(datum: &Datum) -> u64 {
    let mut total = 0u64;
    let mut work = vec![datum];
    while let Some(datum) = work.pop() {
        total = total.saturating_add(VALUE);
        match datum {
            Datum::Text(text) => total = total.saturating_add(text.len() as u64),
            Datum::Bytes(bytes) => total = total.saturating_add(bytes.as_slice().len() as u64),
            Datum::List(items) | Datum::Tuple(items) | Datum::Set(items) => work.extend(items),
            Datum::Map(pairs) => {
                for (key, value) in pairs {
                    work.push(key);
                    work.push(value);
                }
            }
            Datum::Record(fields) => {
                for (_, value) in fields {
                    total = total.saturating_add(VALUE);
                    work.push(value);
                }
            }
            Datum::Error(error) => {
                total = total
                    .saturating_add(values(2))
                    .saturating_add(error.kind.len() as u64)
                    .saturating_add(error.message.len() as u64);
                work.push(error.raw());
            }
            _ => {}
        }
    }
    total
}
