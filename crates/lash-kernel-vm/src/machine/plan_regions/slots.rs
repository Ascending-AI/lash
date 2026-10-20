//! A slots walk's step, taken natively (FIG-6978): the region's body as
//! [`crate::compile::plan_regions::slots`] recognised it, its forms
//! compiled with the region's (FIG-6947).
//!
//! The step opens each statement the region's pass opens, in its order,
//! with its register span, and writes each variable it writes. Its end
//! test, hole test and advance are the region's own compiled forms, and
//! its decisions run here. Its present arm, where it is a map's write or a
//! filter's keep, applies the callback at the steady site, writes its
//! answer as the region's member write does, decides a bool answer's truth
//! as `same` decides it and appends a kept element as the region's member
//! write appends it; an answer no bool, a hole and any other arm run as
//! the region's pass runs them. The callback's operands go to the plan's
//! registers, as the region's form puts them, wherever anything may see
//! them there: where its island returns, nothing does, and the island is
//! handed them in place. So the plan stands, raises and halts exactly
//! where the region's pass would.

use std::mem::ManuallyDrop;

use lash_kernel_doc::Value;
use lash_kernel_doc::plan::{PlanContext, PlanExit};

use super::{Body, IN_PLACE, Operand, Place, Special, Typed, Went, body, operand, special};
use crate::compile::LibId;
use crate::compile::plan_regions::slots::{Append, Apply, Present, Slots};
use crate::compile::plan_regions::{Node, NodeKind};
use crate::machine::islands::Ran;
use crate::machine::plans::PlanRun;

/// A slots walk compiled for the plan's variant.
pub(super) struct Slotted<const S: bool> {
    index: u32,
    length: u32,
    ending: Scalar<S>,
    left: Box<[u32]>,
    /// The hole test's statement span, and its test.
    hole: (u32, Hole<S>),
    holed: Body<S>,
    present: Arm<S>,
    advance: Scalar<S>,
    /// What the present arm's block declares, and the body's.
    declared: (Box<[u32]>, Box<[u32]>),
}

/// A value its statement computes in place: by its typed kernels where it
/// has them, otherwise as the region's operand.
enum Scalar<const S: bool> {
    Typed(Typed),
    Any(Operand<S>),
}

impl<const S: bool> Scalar<S> {
    fn of(node: Node) -> Self {
        match special(&node) {
            Some(Special::Typed(typed)) => Self::Typed(typed),
            _ => Self::Any(operand(node)),
        }
    }

    fn reg(&self) -> u32 {
        match self {
            Self::Typed(typed) => typed.reg,
            Self::Any(operand) => operand.reg,
        }
    }
}

/// The hole test `same(value, ())` by `lib`, its register `reg`.
struct Hole<const S: bool> {
    reg: u32,
    lib: LibId,
    value: Operand<S>,
    unit: Operand<S>,
}

/// The present arm compiled ([`Present`]).
enum Arm<const S: bool> {
    Write {
        span: u32,
        apply: Applied,
        src: u32,
        output: u32,
        target: u32,
        at: u32,
    },
    Keep {
        span: u32,
        answer: u32,
        apply: Applied,
        kept: Kept<S>,
        coerced: Body<S>,
    },
    Block(Body<S>),
}

/// What a filter's keep runs for a true answer: its append, or its block.
enum Kept<const S: bool> {
    Append {
        span: u32,
        item: u32,
        src: u32,
        output: u32,
        target: u32,
        /// `list.len` by `lib` of the list, its register and its operand.
        length: (u32, LibId, Operand<S>),
    },
    Block(Body<S>),
}

/// A steady callback's application ([`Apply`]), its operands read in
/// place, each with its register.
struct Applied {
    reg: u32,
    site: u32,
    callee: u32,
    args: Box<[(u32, Place)]>,
}

fn applied(apply: Apply) -> Option<Applied> {
    if apply.args.len() > IN_PLACE {
        return None;
    }
    Some(Applied {
        reg: apply.reg,
        site: apply.site,
        callee: apply.callee,
        args: apply.args.iter().map(Place::of).collect::<Option<_>>()?,
    })
}

/// `slots`'s forms compiled: `None` where its hole test is not one.
pub(super) fn compiled<const S: bool>(slots: Slots) -> Option<Slotted<S>> {
    let (span, hole) = slots.hole;
    let NodeKind::IsUnit { lib, args } = hole.kind else {
        return None;
    };
    let Ok([value, unit]) = <[Node; 2]>::try_from(args.into_vec()) else {
        return None;
    };
    let present = match slots.present {
        Present::Write {
            span,
            apply,
            src,
            output,
            target,
            at,
        } => Arm::Write {
            span,
            apply: applied(apply)?,
            src,
            output,
            target,
            at,
        },
        Present::Keep {
            span,
            answer,
            apply,
            kept,
            append,
            coerced,
        } => Arm::Keep {
            span,
            answer,
            apply: applied(apply)?,
            kept: match append {
                Some(Append {
                    span,
                    item,
                    src,
                    output,
                    target,
                    length:
                        Node {
                            reg,
                            kind: NodeKind::Length { lib, list },
                        },
                }) => Kept::Append {
                    span,
                    item,
                    src,
                    output,
                    target,
                    length: (reg, lib, operand(*list)),
                },
                _ => Kept::Block(body(kept)),
            },
            coerced: body(coerced),
        },
        Present::Block(arm) => Arm::Block(body(arm)),
    };
    Some(Slotted {
        index: slots.index,
        length: slots.length,
        ending: Scalar::of(slots.ending),
        left: slots.left,
        hole: (
            span,
            Hole {
                reg: hole.reg,
                lib,
                value: operand(value),
                unit: operand(unit),
            },
        ),
        holed: body(slots.holed),
        present,
        advance: Scalar::of(slots.advance),
        declared: slots.declared,
    })
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
    /// One step of `walk`'s loop, the element already bound.
    pub(super) fn slots_step(&mut self, walk: &Slotted<SHARED>) -> Result<Went, PlanExit> {
        // if same(index, length) { break }: `same` of two floats is
        // whether their bits are.
        self.open_statement_of(0)?;
        let regs = &self.machine.regs;
        let ended = match (
            regs[self.slots + walk.index as usize],
            regs[self.slots + walk.length as usize],
        ) {
            (Value::Float(index), Value::Float(length)) => {
                index.get().to_bits() == length.get().to_bits()
            }
            _ => self.truth(&walk.ending)?,
        };
        if ended {
            self.open_statement_of(0)?;
            for slot in &walk.left {
                self.unbind(*slot);
            }
            return Ok(Went::Broke);
        }
        // if same(value, ()) { <hole> } else { <present> }
        let (span, hole) = &walk.hole;
        self.open_statement_of(*span)?;
        let went = match self.is_unit(hole.reg, hole.lib, (&hole.value, &hole.unit))? {
            Value::Bool(true) => self.pass(&walk.holed)?,
            Value::Bool(false) => self.present(&walk.present, &walk.declared.0)?,
            other => return Err(self.not_bool(hole.reg, other)),
        };
        if let Went::Broke = went {
            return Ok(Went::Broke);
        }
        // set index = num.add(index, 1.0)
        self.open_statement_of(0)?;
        let next = self.scalar(&walk.advance)?;
        self.machine.put(self.slots + walk.index as usize, next);
        for slot in walk.declared.1.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Went::Through)
    }

    /// The present arm `arm`, whose block declares `declared`.
    fn present(&mut self, arm: &Arm<SHARED>, declared: &[u32]) -> Result<Went, PlanExit> {
        match arm {
            Arm::Block(body) => return self.pass(body),
            Arm::Write {
                span,
                apply,
                src,
                output,
                target,
                at,
            } => {
                // set output[at] = apply ...: the answer, pinned, then the
                // target and the index, which the region's member write
                // writes from in place.
                self.open_statement_of(*span)?;
                let answer = self.applied(apply)?;
                let pinned = self.machine.pin_value(&answer);
                self.held(pinned)?;
                let list = self.machine.regs[self.slots + *output as usize];
                let index = self.machine.regs[self.slots + *at as usize];
                if self.write_list(list, index, answer)? {
                    self.machine.take(self.base + *src as usize);
                } else {
                    // Any other write is the plan's member write, from the
                    // registers the region's form leaves its operands in.
                    self.widen();
                    self.machine.put(self.base + *src as usize, answer);
                    self.machine.put(self.base + *target as usize, list);
                    self.machine.put(self.base + *target as usize + 1, index);
                    self.set_index(*target, *src)?;
                }
            }
            Arm::Keep {
                span,
                answer,
                apply,
                kept,
                coerced,
            } => {
                // let answer = apply ...
                self.open_statement_of(*span)?;
                let value = self.applied(apply)?;
                self.bind_value(*answer, value)?;
                // if same(answer, true) { <kept> } else {
                //   if same(answer, false) {} else { <coerced> }
                // }
                self.open_statement_of(0)?;
                let went = match value {
                    Value::Bool(true) => self.kept(kept)?,
                    Value::Bool(false) => {
                        self.open_statement_of(0)?;
                        Went::Through
                    }
                    _ => {
                        self.open_statement_of(0)?;
                        self.pass(coerced)?
                    }
                };
                if let Went::Broke = went {
                    return Ok(Went::Broke);
                }
            }
        }
        for slot in declared.iter().rev() {
            self.unbind(*slot);
        }
        Ok(Went::Through)
    }

    /// What a filter's keep runs for a true answer: its append
    /// `set output[list.len(output)] = item`, as the region's member write
    /// writes it, the item pinned and the length read as the region's
    /// form reads it; or its block.
    fn kept(&mut self, kept: &Kept<SHARED>) -> Result<Went, PlanExit> {
        let (span, item, src, output, target, (reg, lib, list)) = match kept {
            Kept::Block(body) => return self.pass(body),
            Kept::Append {
                span,
                item,
                src,
                output,
                target,
                length,
            } => (span, item, src, output, target, length),
        };
        self.open_statement_of(*span)?;
        let value = self.machine.regs[self.slots + *item as usize];
        let pinned = self.machine.pin_value(&value);
        self.held(pinned)?;
        let held = self.machine.regs[self.slots + *output as usize];
        let at = self.length(*reg, *lib, list)?;
        if self.write_list(held, at, value)? {
            self.machine.take(self.base + *src as usize);
        } else {
            self.widen();
            self.machine.put(self.base + *src as usize, value);
            self.machine.put(self.base + *target as usize, held);
            self.machine.put(self.base + *target as usize + 1, at);
            self.set_index(*target, *src)?;
        }
        Ok(Went::Through)
    }

    /// The steady callback's application `apply`, as the region's form
    /// applies it: the callback read, then its operands, then applied at
    /// its site. Its answer. Where the callback's island returns, nothing
    /// sees its operands in their registers before the site takes them,
    /// and its island is handed them in place; anywhere else they go to
    /// their registers first, in order, as the region's form puts them.
    #[inline(always)]
    fn applied(&mut self, apply: &Applied) -> Result<Value, PlanExit> {
        let at = self.base + apply.reg as usize;
        let callee = self.machine.regs[self.slots + apply.callee as usize];
        let count = apply.args.len();
        let mut values = [Value::Absent; IN_PLACE];
        let variables = &self.machine.regs[self.slots..];
        for (value, (_, place)) in values.iter_mut().zip(&apply.args) {
            *value = match place {
                Place::Local(slot) => variables[*slot as usize],
                Place::Const(constant) => *constant,
            };
        }
        let steadied = matches!(callee, Value::Closure(closure) if self.steadied(closure));
        let ran = ManuallyDrop::new(if steadied {
            self.steady_entry(&values[..count])
        } else {
            Ok(None)
        });
        // A returned call's outcome owns nothing: it is left undropped
        // rather than paying its drop's call for each callback.
        if let Ok(Some(Ran::Returned(value))) = &*ran {
            let value = *value;
            self.returned(at, value);
            return Ok(value);
        }
        let ran = ManuallyDrop::into_inner(ran);
        for ((reg, _), value) in apply.args.iter().zip(values) {
            self.machine.put(self.base + *reg as usize, value);
        }
        if steadied {
            self.steady_ran(apply.site, (at, count), ran)?;
        } else {
            self.apply_steady(apply.site, at, count, callee)?;
        }
        Ok(self.machine.regs[at])
    }

    #[inline(always)]
    fn scalar(&mut self, scalar: &Scalar<SHARED>) -> Result<Value, PlanExit> {
        match scalar {
            Scalar::Typed(typed) => self.typed(typed),
            Scalar::Any(operand) => operand.get(self),
        }
    }

    /// The test `scalar`, which an `if` takes.
    #[inline(always)]
    fn truth(&mut self, scalar: &Scalar<SHARED>) -> Result<bool, PlanExit> {
        match self.scalar(scalar)? {
            Value::Bool(holds) => Ok(holds),
            other => Err(self.not_bool(scalar.reg(), other)),
        }
    }
}
