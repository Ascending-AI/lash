//! The ABI digest: one hash over everything compiled code and the machine
//! must agree on.
//!
//! [`EXEC_ABI_DIGEST`] is the blake3 hash of [`abi_description`]: the
//! ordered op list, the ordered runtime functions with their groups and
//! signatures, the exit and status codes, the `RawSlot`, `ExitRecord` and
//! `RtContext` layouts, and the map encoding version. Every artifact key
//! carries it, so code built against another ABI is never run.

use std::fmt::Write as _;
use std::mem::{align_of, offset_of, size_of};
use std::sync::LazyLock;

use crate::abi::{ExitKind, ExitRecord, RawSlot, RtContext, RtFn, RtStatus};
use crate::kir::OpKind;
use crate::maps::MAP_ENCODING_VERSION;

/// The hash of [`abi_description`].
pub static EXEC_ABI_DIGEST: LazyLock<[u8; 32]> =
    LazyLock::new(|| *blake3::hash(abi_description().as_bytes()).as_bytes());

/// The canonical text the ABI digest hashes, one fact per line.
pub fn abi_description() -> String {
    let mut out = String::from("lash-kernel-exec abi\n");
    for op in OpKind::ALL {
        line(&mut out, format_args!("op {op:?}"));
    }
    for f in RtFn::ALL {
        let sig = f.signature();
        line(
            &mut out,
            format_args!("rt {f:?} {:?} {:?} -> {:?}", f.group(), sig.params, sig.ret),
        );
    }
    for exit in ExitKind::ALL {
        line(&mut out, format_args!("exit {exit:?}={}", *exit as u32));
    }
    for status in RtStatus::ALL {
        line(
            &mut out,
            format_args!("status {status:?}={}", *status as u32),
        );
    }
    layout::<RawSlot>(
        &mut out,
        "RawSlot",
        &[
            ("tag", offset_of!(RawSlot, tag)),
            ("payload", offset_of!(RawSlot, payload)),
        ],
    );
    layout::<ExitRecord>(
        &mut out,
        "ExitRecord",
        &[
            ("kind", offset_of!(ExitRecord, kind)),
            ("boundary", offset_of!(ExitRecord, boundary)),
            ("map", offset_of!(ExitRecord, map)),
            ("value_reg", offset_of!(ExitRecord, value_reg)),
            ("aux", offset_of!(ExitRecord, aux)),
        ],
    );
    layout::<RtContext>(
        &mut out,
        "RtContext",
        &[
            ("machine", offset_of!(RtContext, machine)),
            ("rt", offset_of!(RtContext, rt)),
            ("regs", offset_of!(RtContext, regs)),
            ("charged", offset_of!(RtContext, charged)),
            ("charge_bound", offset_of!(RtContext, charge_bound)),
            ("slice_at", offset_of!(RtContext, slice_at)),
            ("charging", offset_of!(RtContext, charging)),
            ("interrupt", offset_of!(RtContext, interrupt)),
            ("exit", offset_of!(RtContext, exit)),
        ],
    );
    line(&mut out, format_args!("maps v{MAP_ENCODING_VERSION}"));
    out
}

fn line(out: &mut String, fact: std::fmt::Arguments<'_>) {
    // Writing to a `String` cannot fail.
    let _ = out.write_fmt(fact);
    out.push('\n');
}

fn layout<T>(out: &mut String, name: &str, fields: &[(&str, usize)]) {
    line(
        out,
        format_args!(
            "layout {name} size={} align={}",
            size_of::<T>(),
            align_of::<T>()
        ),
    );
    for (field, offset) in fields {
        line(out, format_args!("layout {name}.{field}@{offset}"));
    }
}
