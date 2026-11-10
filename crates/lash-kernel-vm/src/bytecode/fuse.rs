//! Fusion: runs of a block's ops that dispatch at once on the prepaid
//! path (cutover contract §3).
//!
//! A prepaid block bills no op on its own and tests no slice inside it,
//! so a run of its ops may dispatch together. The pass fuses up to two
//! local or constant pushes, the scalar native call (R8) they feed, and
//! the op that takes its answer. The call's fee cancels the block, and
//! the rest runs exact, so the taking op is fused only where the exact
//! path bills, tests and sees nothing at it. The run's first op becomes
//! `Op::Fused`; the exact path dispatches the first op alone, and the
//! ops after it stay in place.
//!
//! It runs after validation, on validated code.

use lash_kernel_doc::arena::native::{InlineOperation, ScalarDomain};

use super::op::Op;
use super::program::{Callee, Fused, Operand, Program, Then, Unit, Words};

/// Fuses the runs of every unit of `program`.
pub(crate) fn fuse(program: &mut Program) {
    let scalar = |site: u32, unit: &Unit| {
        let call = &unit.calls[site as usize];
        let Callee::Native { native, .. } = call.callee else {
            return false;
        };
        let entry = program.native(native);
        call.argc == 2
            && entry.params == 2
            && matches!(entry.scalar, Some(InlineOperation::Scalar { .. }))
    };
    // A native with no body is answered at its call with no frame, so
    // two word ints it accepts are answered and billed in place.
    let words = |site: u32, unit: &Unit| {
        let Callee::Native { native, charged } = unit.calls[site as usize].callee else {
            return None;
        };
        let entry = program.native(native);
        let Some(InlineOperation::Scalar { operation, domain }) = entry.scalar else {
            return None;
        };
        let fee = match charged {
            true => Some(program.charge(entry.charge).pairs()?),
            false => None,
        };
        (entry.body.is_none()
            && matches!(domain, ScalarDomain::Words | ScalarDomain::WordsOrFloats))
        .then_some(Words { operation, fee })
    };
    let mut runs = Vec::new();
    for (at, unit) in program.units.iter().enumerate() {
        let mut pc = 0;
        while pc < unit.ops.len() {
            match run(unit, pc, &scalar).map(|(mut fused, end)| {
                fused.words = words(fused.site, unit);
                (fused, end)
            }) {
                Some((fused, end)) => {
                    runs.push((at, pc, fused));
                    pc = end;
                }
                None => pc += 1,
            }
        }
    }
    for (at, pc, fused) in runs {
        let unit = &mut program.units[at];
        unit.ops[pc] = Op::Fused(unit.fused.len() as u32);
        unit.fused.push(fused);
    }
}

/// The run that starts at `pc`, and the op after it.
fn run(unit: &Unit, pc: usize, scalar: &impl Fn(u32, &Unit) -> bool) -> Option<(Fused, usize)> {
    let operand = |op: Op| match op {
        Op::Local(n) => Some(Operand::Local(n)),
        Op::Const(n) => Some(Operand::Const(n)),
        _ => None,
    };
    let mut operands = [Operand::Local(0); 2];
    let mut pushes = 0;
    let mut at = pc;
    let site = loop {
        match *unit.ops.get(at)? {
            Op::Call(site) if scalar(site, unit) => break site,
            _ if pushes == 2 => return None,
            op => {
                operands[pushes] = operand(op)?;
                pushes += 1;
                at += 1;
            }
        }
    };
    let call = at;
    // After the fee the rest of the block is exact: the op taking the
    // answer is fused only where the exact path does nothing at it.
    let quiet = |pc: usize| {
        let event = unit.event[pc];
        event.units == 0 && event.atomic == 0 && !unit.step[pc]
    };
    let taking = unit.ops.get(call + 1).copied().filter(|_| quiet(call + 1));
    let (then, end) = match taking {
        Some(Op::SetLocal(n)) => (Then::SetLocal(n), call + 2),
        Some(Op::Pop) => (Then::Pop, call + 2),
        Some(Op::JumpIfFalse(to)) => (Then::Branch { to, looping: false }, call + 2),
        Some(Op::WhileFalse(to)) => (Then::Branch { to, looping: true }, call + 2),
        // A lone call dispatches inline already.
        _ if pushes == 0 => return None,
        _ => (Then::Push, call + 1),
    };
    let fused = Fused {
        first: unit.ops[pc],
        operands,
        pushes: pushes as u8,
        site,
        call: call as u32,
        then,
        words: None,
    };
    Some((fused, end))
}
