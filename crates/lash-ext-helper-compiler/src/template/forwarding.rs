//! Forwarding facts are derived from the ordered symbolic source path.

use super::*;

pub(super) fn identity(pass: &mut Pass, names: &[&str], results: &[(State, Option<Symbol>)]) {
    let [(state, Some(value))] = results else {
        return;
    };
    if names.len() != 1
        || value.code != "args[0]"
        || !state.steps.is_empty()
        || state.charging.is_some()
        || state.identity_unproven
    {
        return;
    }
    let Some(kind @ (ValueKind::Int | ValueKind::Float | ValueKind::Bool)) = value.kind else {
        return;
    };
    pass.identity_kinds |= 1 << kind as u32;
    for body in &state.identity_bodies {
        if !pass.identity_bodies.contains(body) {
            pass.identity_bodies.push(*body);
        }
    }
    for requirement in &state.identity_requires {
        if !pass.identity_requires.contains(requirement) {
            pass.identity_requires.push(*requirement);
        }
    }
}

pub(super) fn identity_descriptor(pass: &Pass) -> String {
    if pass.identity_kinds == 0 {
        return "None".to_owned();
    }
    let mut descriptor = format!(
        "Some(&p::IdentityPath {{ kinds: {}, bodies: &[",
        pass.identity_kinds
    );
    for body in &pass.identity_bodies {
        let _ = write!(descriptor, "p::function({:?}),", body.to_string());
    }
    descriptor.push_str("], requires: &[");
    for (function, fact) in &pass.identity_requires {
        let _ = write!(
            descriptor,
            "p::NativeRequirement {{ function: p::function({:?}), fact: p::NativeFact::Symbolic(lash_kernel_doc::SymbolicNative::{fact:?}) }},",
            function.to_string()
        );
    }
    descriptor.push_str("] })");
    descriptor
}

impl Evaluator<'_> {
    /// A path returning one native's unchanged arguments. Integer floor
    /// kernels also cover a preceding nonzero-divisor guard: they decline
    /// zero (and word overflow), leaving the template's error path intact.
    /// Float paths retain their guard and never use this exception.
    pub(super) fn forwarded(&self, kinds: &[&str], steps: &[Step], value: &Symbol) -> Option<u8> {
        use lash_kernel_doc::scalar::ScalarOp;
        let (Step::Entry { name, native, args }, prefix) = steps.split_last()? else {
            return None;
        };
        if value.code != *name || args != "args[0], args[1]" {
            return None;
        }
        match prefix {
            [] => {}
            [
                Step::Entry {
                    name: zero,
                    native: equal,
                    args,
                },
                Step::Guard { test, holds: false },
            ] if kinds == ["Int(_)", "Int(_)"]
                && test == zero
                && args == "args[1], Value::Int(0)" =>
            {
                let pair = |native: usize| {
                    self.library
                        .registry
                        .get(self.callees.get(native)?)?
                        .native()?
                        .prepare()?
                        .pair
                };
                if pair(*equal)?.ints != Some(ScalarOp::Eq)
                    || !matches!(
                        pair(*native)?.ints,
                        Some(ScalarOp::RemFloor | ScalarOp::FloorDiv)
                    )
                {
                    return None;
                }
            }
            _ => return None,
        }
        u8::try_from(*native).ok()
    }
}

/// A text path consists of exactly one native call of its unchanged args.
pub(super) fn text(steps: &[Step], value: &Symbol) -> Option<u8> {
    let [step] = steps else {
        return None;
    };
    let (Step::Entry { name, native, args }
    | Step::Fallback {
        name, native, args, ..
    }) = step
    else {
        return None;
    };
    (value.code == *name && args == "args[0], args[1]")
        .then(|| u8::try_from(*native).ok())
        .flatten()
}

/// All four representation pairs must forward to the same native.
pub(super) fn texts(scalar: &Pass, heap: Option<&Pass>) -> Option<u8> {
    let native = *scalar
        .text_forwards
        .get(["Text(_)", "Text(_)"].as_slice())?;
    let heap = heap?;
    [
        ("Text(_)", "LongText(_)"),
        ("LongText(_)", "Text(_)"),
        ("LongText(_)", "LongText(_)"),
    ]
    .iter()
    .all(|(a, b)| heap.text_forwards.get([*a, *b].as_slice()) == Some(&native))
    .then_some(native)
}
