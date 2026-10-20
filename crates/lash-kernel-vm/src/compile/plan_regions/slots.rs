//! The slots walk of an array callback helper (FIG-6978): a plan region
//! whose loop visits a list's slots by position up to the length taken
//! first, skipping or filling its holes and applying the callback to each
//! element. TypeScript's `ts.array.map_slots`, `filter_slots` and
//! `forEach` walk this way (FIG-6942).
//!
//! The region's machine steps such a loop natively
//! (`crate::machine::plan_regions`), opening each statement the region's
//! pass opens, in its order and with its register span. Its end test,
//! hole test and advance are the region's own compiled forms, and its
//! decisions run as Rust. Where its present arm is a map's write or a
//! filter's keep, the step applies the callback through the region's
//! steady site, writes its answer as the region's member write does,
//! decides a bool answer's truth as `same` decides it and appends a kept
//! element as the region's member write appends it; a coercion of any
//! other answer, a hole and any other arm run as the region's pass runs
//! them. So where it stands, raises or halts, the plan stands as the
//! region would, and nothing it charges differs: the body's forms are
//! uncharged (`K-CHG-007`), the loop's test is charged by the region's
//! walk, and the callback is ordinary charged code wherever it runs.
//!
//! The loop forms a slots walk only when its body is exactly this, its
//! end test and advance putting nothing in a register:
//!
//! ```text
//! if same(index, length) { break }
//! if same(value, ()) { <hole> } else { <present> }
//! set index = num.add(index, 1.0)
//! ```
//!
//! where the present arm is a map's write, a filter's keep, or any other
//! block:
//!
//! ```text
//! set output[index] = apply callback(context, value, index, this)
//!
//! let answer = apply callback(context, value, index, this)
//! if same(answer, true) { <kept> } else {
//!   if same(answer, false) {} else { <coerced> }
//! }
//! ```
//!
//! and the kept block, where it is an append, is
//! `set output[list.len(output)] = value`.

use lash_kernel_doc::Value;

use super::merge::{ADD, Recognise, SAME, assigned, local, nothing, nothing_declared};
use super::{Block, Node, NodeKind, StepKind};

/// A recognised slots walk: the plan variables its index and the length
/// it stops at are, and its forms, the plan's nodes and blocks, which the
/// machine compiles with the region's.
pub(crate) struct Slots {
    pub(crate) index: u32,
    pub(crate) length: u32,
    /// The end test `same(index, length)`.
    pub(crate) ending: Node,
    /// What the `break` empties.
    pub(crate) left: Box<[u32]>,
    /// The hole test's statement: its register span and its test.
    pub(crate) hole: (u32, Node),
    pub(crate) holed: Block,
    pub(crate) present: Present,
    /// The advance `num.add(index, 1.0)`.
    pub(crate) advance: Node,
    /// What the present arm's block declares, and the body's.
    pub(crate) declared: (Box<[u32]>, Box<[u32]>),
}

/// A slots walk's present arm.
pub(crate) enum Present {
    /// `set output[at] = apply ...`: the statement's register span, the
    /// application, whose register its pinned answer stays in, then the
    /// variables the target and the index read and the target's register,
    /// the index's its next.
    Write {
        span: u32,
        apply: Apply,
        src: u32,
        output: u32,
        target: u32,
        at: u32,
    },
    /// `let answer = apply ...` and the test of its truth: the
    /// application's register span, the variable it binds, the
    /// application, the block a true answer runs, as the append it is
    /// where it is one, and the block an answer no bool runs.
    Keep {
        span: u32,
        answer: u32,
        apply: Apply,
        kept: Block,
        append: Option<Append>,
        coerced: Block,
    },
    /// Any other arm, run as the region's pass runs it.
    Block(Block),
}

/// `set output[list.len(output)] = item` of two variables: the item
/// first, pinned, then the list, then its length. The statement's register
/// span, the item's variable and its register, the list's variable and
/// register (the length's its next), and the length's form.
pub(crate) struct Append {
    pub(crate) span: u32,
    pub(crate) item: u32,
    pub(crate) src: u32,
    pub(crate) output: u32,
    pub(crate) target: u32,
    pub(crate) length: Node,
}

/// A steady callback's application to operands read in place: its
/// register, its site, the variable holding the callback, and its
/// operands, each with its register.
pub(crate) struct Apply {
    pub(crate) reg: u32,
    pub(crate) site: u32,
    pub(crate) callee: u32,
    pub(crate) args: Box<[Node]>,
}

impl Recognise<'_, '_> {
    /// The slots walk `body`, binding each element to `binding`, is, if it
    /// is one.
    pub(super) fn slots(&self, body: &Block, binding: u32) -> Option<Slots> {
        let [ending, holed, advance] = &body.steps[..] else {
            return None;
        };
        // if same(index, length) { break }
        let (cond, then, otherwise) = self.branch(ending)?;
        let [index, length] = self.call_locals(cond, SAME)?;
        nothing(otherwise)?;
        nothing_declared(then)?;
        let [stop] = &then.steps[..] else {
            return None;
        };
        let StepKind::Break(left) = &stop.kind else {
            return None;
        };
        // if same(value, ()) { <hole> } else { <present> }
        let StepKind::If {
            cond: hole,
            then: holes,
            otherwise: present,
        } = &holed.kind
        else {
            return None;
        };
        let NodeKind::IsUnit { args, .. } = &hole.kind else {
            return None;
        };
        (args.first().and_then(local)? == binding).then_some(())?;
        // set index = num.add(index, 1.0)
        let (advanced, sum) = assigned(advance)?;
        let [from, one] = self.call_args(sum, ADD)?;
        (advanced == index
            && local(from)? == index
            && matches!(one.kind, NodeKind::Const(Value::Float(one)) if one.get() == 1.0))
        .then_some(())?;
        // The walk's bookkeeping is its own.
        (index != length && index != binding && length != binding).then_some(())?;
        let arm = self
            .write(present, index)
            .or_else(|| self.keep(present))
            .unwrap_or_else(|| Present::Block(present.clone()));
        Some(Slots {
            index,
            length,
            ending: cond.clone(),
            left: left.clone(),
            hole: (holed.span, hole.clone()),
            holed: holes.clone(),
            present: arm,
            advance: sum.clone(),
            declared: (present.declares.clone(), body.declares.clone()),
        })
    }

    /// A map's write, `set output[index] = apply ...`, of the walk's
    /// `index`: the answer first, pinned, then the target and the index,
    /// none of which stands the plan at a site.
    fn write(&self, block: &Block, index: u32) -> Option<Present> {
        nothing_declared(block)?;
        let [step] = &block.steps[..] else {
            return None;
        };
        let StepKind::Store {
            order: [value, target, at],
            target: reg,
            src,
            placed: false,
        } = &step.kind
        else {
            return None;
        };
        let NodeKind::Pin(applied) = &value.kind else {
            return None;
        };
        (value.reg == *src
            && applied.reg == *src
            && target.reg == *reg
            && at.reg == reg + 1
            && local(at)? == index)
            .then_some(())?;
        Some(Present::Write {
            span: step.span,
            apply: steady(applied)?,
            src: *src,
            output: local(target)?,
            target: *reg,
            at: index,
        })
    }

    /// A filter's keep: `let answer = apply ...`, then the test of its
    /// truth.
    fn keep(&self, block: &Block) -> Option<Present> {
        let [bind, test] = &block.steps[..] else {
            return None;
        };
        let StepKind::Bind {
            slot: answer,
            value: applied,
        } = &bind.kind
        else {
            return None;
        };
        (block.declares[..] == [*answer]).then_some(())?;
        // if same(answer, true) { <kept> } else { if same(answer, false) {} else { <coerced> } }
        let (cond, kept, otherwise) = self.branch(test)?;
        self.answered(cond, *answer, true)?;
        nothing_declared(otherwise)?;
        let [inner] = &otherwise.steps[..] else {
            return None;
        };
        let (cond, falsy, coerced) = self.branch(inner)?;
        self.answered(cond, *answer, false)?;
        nothing(falsy)?;
        Some(Present::Keep {
            span: bind.span,
            answer: *answer,
            apply: steady(applied)?,
            kept: kept.clone(),
            append: append(kept),
            coerced: coerced.clone(),
        })
    }

    /// `same(answer, truth)`.
    fn answered(&self, node: &Node, answer: u32, truth: bool) -> Option<()> {
        let [tested, constant] = self.call_args(node, SAME)?;
        (local(tested)? == answer
            && matches!(constant.kind, NodeKind::Const(Value::Bool(held)) if held == truth))
        .then_some(())
    }
}

/// The append `block` is, if it is one, declaring nothing.
fn append(block: &Block) -> Option<Append> {
    nothing_declared(block)?;
    let [step] = &block.steps[..] else {
        return None;
    };
    let StepKind::Store {
        order: [item, target, at],
        target: reg,
        src,
        placed: false,
    } = &step.kind
    else {
        return None;
    };
    let NodeKind::Pin(pinned) = &item.kind else {
        return None;
    };
    let NodeKind::Length { list, .. } = &at.kind else {
        return None;
    };
    let output = local(target)?;
    (item.reg == *src && target.reg == *reg && at.reg == reg + 1 && local(list)? == output)
        .then_some(())?;
    Some(Append {
        span: step.span,
        item: local(pinned)?,
        src: *src,
        output,
        target: *reg,
        length: at.clone(),
    })
}

/// The application `node` of a steady callback to operands read in place.
fn steady(node: &Node) -> Option<Apply> {
    let NodeKind::Apply {
        site,
        callee,
        args,
        steady: true,
    } = &node.kind
    else {
        return None;
    };
    args.iter()
        .all(|arg| matches!(arg.kind, NodeKind::Local(_) | NodeKind::Const(_)))
        .then_some(())?;
    Some(Apply {
        reg: node.reg,
        site: *site,
        callee: *callee,
        args: args.clone(),
    })
}
