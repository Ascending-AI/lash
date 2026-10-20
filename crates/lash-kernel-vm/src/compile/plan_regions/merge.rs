//! The merge pass of a comparator sort (FIG-6948): a plan region whose
//! loop is one pass of a stable bottom-up merge, placing one element per
//! step and applying the comparator where the left and right runs both
//! have elements. TypeScript's `ts.array.sort` merges this way
//! (FIG-6886).
//!
//! The region's machine walks such a loop natively
//! (`crate::machine::plan_regions`): its integer bookkeeping and its
//! decisions run as Rust on the plan's own variables, and each other form
//! (the item reads and writes, the comparator's application at its site,
//! the coercion of its answer offered at its site) runs through the
//! region's own form, in the plan's order on the plan's registers. So
//! where it stands, raises or halts, the plan stands as the region would,
//! and nothing it charges differs: the body's forms are uncharged
//! (`K-CHG-007`), the loop's test is charged by the region's walk, and the
//! comparator is ordinary charged code wherever it runs.
//!
//! The loop forms a merge only when its body is exactly this, every block
//! but the comparison's declaring nothing and every statement but the
//! application and the coercion putting nothing in a register:
//!
//! ```text
//! if same(dest, end) {
//!   set mid = num.min(num.add(dest, width), n)
//!   set end = num.min(num.add(mid, width), n)
//!   set left = dest
//!   set right = mid
//! }
//! let from_right = false
//! if num.lt(left, mid) {
//!   if num.lt(right, end) {
//!     let first = list.get(sorted, left)
//!     let second = list.get(sorted, right)
//!     let output = apply callback(context, first, second)
//!     let order = invoke coerce(output)
//!     if num.lt(0, order) { set from_right = true }
//!   }
//! } else { set from_right = true }
//! if from_right {
//!   set out[dest] = list.get(sorted, right)
//!   set right = num.add(right, 1)
//! } else {
//!   set out[dest] = list.get(sorted, left)
//!   set left = num.add(left, 1)
//! }
//! set dest = num.add(dest, 1)
//! ```

use lash_kernel_doc::Value;

use super::{Block, Node, NodeKind, Step, StepKind};
use crate::compile::{Lib, LibId};

/// A recognised merge pass: the plan variables it reads and writes, and
/// the forms it runs as the region's own, each a value `N` and each
/// element write a `P`: as recognised, the plan's nodes and statements;
/// as the machine walks it, their compiled forms ([`Self::map`]).
pub(crate) struct Merge<N = Node, P = Step> {
    pub(crate) sorted: u32,
    pub(crate) out: u32,
    pub(crate) n: u32,
    pub(crate) width: u32,
    pub(crate) mid: u32,
    pub(crate) end: u32,
    pub(crate) left: u32,
    pub(crate) right: u32,
    pub(crate) dest: u32,
    pub(crate) from_right: u32,
    /// The comparator's operands, the left item and the right.
    pub(crate) operands: [u32; 2],
    /// The two item reads of the comparison, left then right.
    pub(crate) items: [N; 2],
    /// The statement applying the comparator: its register span, the
    /// variable it binds and its `Apply` node.
    pub(crate) apply: (u32, u32, N),
    /// The statement coercing the comparator's answer: its register span,
    /// the variable it binds and its `Invoke` node.
    pub(crate) coerce: (u32, u32, N),
    /// The test of the coerced order: `num.lt(0, order)`.
    pub(crate) positive: N,
    /// The two element writes into `out`: from the right run, then from
    /// the left.
    pub(crate) places: [P; 2],
    /// What the comparison's block declares, and the body's.
    pub(crate) declared: (Box<[u32]>, Box<[u32]>),
}

impl<N, P> Merge<N, P> {
    /// The same merge, its values by `value` and its element writes by
    /// `place`; `None` where `place` declines one.
    pub(crate) fn map<M, Q>(
        self,
        mut value: impl FnMut(N) -> M,
        mut place: impl FnMut(P) -> Option<Q>,
    ) -> Option<Merge<M, Q>> {
        let [right, left] = self.places;
        let [first, second] = self.items;
        let (apply_span, output, applied) = self.apply;
        let (coerce_span, order, invoked) = self.coerce;
        Some(Merge {
            sorted: self.sorted,
            out: self.out,
            n: self.n,
            width: self.width,
            mid: self.mid,
            end: self.end,
            left: self.left,
            right: self.right,
            dest: self.dest,
            from_right: self.from_right,
            operands: self.operands,
            items: [value(first), value(second)],
            apply: (apply_span, output, value(applied)),
            coerce: (coerce_span, order, value(invoked)),
            positive: value(self.positive),
            places: [place(right)?, place(left)?],
            declared: self.declared,
        })
    }
}

/// The kernel library function names a merge's integer forms call.
pub(super) const SAME: &str = "same";
pub(super) const ADD: &str = "num.add";
const MIN: &str = "num.min";
const LT: &str = "num.lt";

/// What recognising a loop's body reads: each library function by its
/// table index.
pub(super) struct Recognise<'a, 'l> {
    pub(super) lib: &'a dyn Fn(LibId) -> Option<&'l Lib>,
}

impl Recognise<'_, '_> {
    /// The merge pass `body` is, if it is one.
    pub(super) fn merge(&self, body: &Block) -> Option<Merge> {
        let [head, start, compare, place, advance] = &body.steps[..] else {
            return None;
        };
        // if same(dest, end) { set mid = ... set end = ... set left = dest set right = mid }
        let (cond, then, otherwise) = self.branch(head)?;
        let [dest, end] = self.call_locals(cond, SAME)?;
        nothing(otherwise)?;
        let [set_mid, set_end, set_left, set_right] = self.plain(then)?;
        let (mid, [dest2, width, n]) = self.bound_sum(set_mid)?;
        let (end2, [mid2, width2, n2]) = self.bound_sum(set_end)?;
        let (left, from) = assigned(set_left)?;
        let (right, from2) = assigned(set_right)?;
        (dest2 == dest
            && end2 == end
            && mid2 == mid
            && width2 == width
            && n2 == n
            && local(from)? == dest
            && local(from2)? == mid)
            .then_some(())?;
        // let from_right = false
        let StepKind::Bind {
            slot: from_right,
            value,
        } = &start.kind
        else {
            return None;
        };
        (start.span == 0 && matches!(value.kind, NodeKind::Const(Value::Bool(false))))
            .then_some(())?;
        // if num.lt(left, mid) { if num.lt(right, end) { <comparison> } } else { set from_right = true }
        let (cond, then, otherwise) = self.branch(compare)?;
        (self.call_locals(cond, LT)? == [left, mid]).then_some(())?;
        self.sets_true(otherwise, *from_right)?;
        let [inner] = self.plain(then)?;
        let (cond, comparison, otherwise) = self.branch(inner)?;
        (self.call_locals(cond, LT)? == [right, end]).then_some(())?;
        nothing(otherwise)?;
        let compared = self.comparison(comparison, *from_right, [left, right])?;
        // if from_right { set out[dest] = ...right... } else { ...left... }
        let (cond, then, otherwise) = self.branch(place)?;
        (local(cond)? == *from_right).then_some(())?;
        let [take_right, step_right] = self.plain(then)?;
        let [take_left, step_left] = self.plain(otherwise)?;
        let sorted = compared.sorted;
        let out = self.placed(take_right, [dest, sorted, right])?;
        (self.placed(take_left, [dest, sorted, left])? == out
            && self.increment(step_right)? == right
            && self.increment(step_left)? == left
            // set dest = num.add(dest, 1)
            && self.increment(advance)? == dest)
            .then_some(())?;
        // The loop's bookkeeping is its own: no two roles share a variable.
        let mut roles = vec![
            sorted,
            out,
            n,
            width,
            mid,
            end,
            left,
            right,
            dest,
            *from_right,
        ];
        roles.extend(compared.operands);
        roles.extend([compared.apply.1, compared.coerce.1]);
        let count = roles.len();
        roles.sort_unstable();
        roles.dedup();
        (roles.len() == count).then_some(())?;
        Some(Merge {
            sorted,
            out,
            n,
            width,
            mid,
            end,
            left,
            right,
            dest,
            from_right: *from_right,
            operands: compared.operands,
            items: compared.items,
            apply: compared.apply,
            coerce: compared.coerce,
            positive: compared.positive,
            places: [take_right.clone(), take_left.clone()],
            declared: (compared.declared, body.declares.clone()),
        })
    }

    /// The comparison's block: the two item reads, the application, the
    /// coercion and the test of the order, reading `sorted` at `left` and
    /// `right`.
    fn comparison(&self, block: &Block, from_right: u32, at: [u32; 2]) -> Option<Compared> {
        let [first, second, apply, coerce, test] = &block.steps[..] else {
            return None;
        };
        // The two item reads, by the same list, at `left` then `right`.
        let mut sorted = None;
        let mut items = Vec::new();
        let mut operands = Vec::new();
        for (step, position) in [first, second].into_iter().zip(at) {
            let StepKind::Bind { slot, value: item } = &step.kind else {
                return None;
            };
            (step.span == 0).then_some(())?;
            let [list, read] = self.item(item)?;
            (*sorted.get_or_insert(list) == list && read == position).then_some(())?;
            items.push(item.clone());
            operands.push(*slot);
        }
        // let output = apply callback(context, <operands>)
        let StepKind::Bind {
            slot: output,
            value: applied,
        } = &apply.kind
        else {
            return None;
        };
        let NodeKind::Apply { args, steady, .. } = &applied.kind else {
            return None;
        };
        let ([context, a, b], [first, second]) = (&args[..], &operands[..]) else {
            return None;
        };
        (matches!(context.kind, NodeKind::Const(_)) && local(a)? == *first && local(b)? == *second)
            .then_some(())?;
        // The callback is the loop's for its whole walk.
        steady.then_some(())?;
        // let order = invoke coerce(output)
        let StepKind::Bind {
            slot: order,
            value: invoked,
        } = &coerce.kind
        else {
            return None;
        };
        let NodeKind::Invoke {
            args: coerced_args, ..
        } = &invoked.kind
        else {
            return None;
        };
        let [coerced_arg] = &coerced_args[..] else {
            return None;
        };
        (local(coerced_arg)? == *output).then_some(())?;
        // if num.lt(0, order) { set from_right = true }
        let (positive, then, otherwise) = self.branch(test)?;
        let [zero, ordered] = self.call_args(positive, LT)?;
        (matches!(zero.kind, NodeKind::Const(Value::Int(0))) && local(ordered)? == *order)
            .then_some(())?;
        self.sets_true(then, from_right)?;
        nothing(otherwise)?;
        Some(Compared {
            sorted: sorted?,
            operands: [*first, *second],
            items: [items[0].clone(), items[1].clone()],
            apply: (apply.span, *output, applied.clone()),
            coerce: (coerce.span, *order, invoked.clone()),
            positive: positive.clone(),
            declared: block.declares.clone(),
        })
    }

    /// An `if` that puts nothing in a register: its test and its arms.
    pub(super) fn branch<'s>(&self, step: &'s Step) -> Option<(&'s Node, &'s Block, &'s Block)> {
        match &step.kind {
            StepKind::If {
                cond,
                then,
                otherwise,
            } if step.span == 0 => Some((cond, then, otherwise)),
            _ => None,
        }
    }

    /// A block that declares nothing and whose `N` statements put nothing
    /// in a register.
    fn plain<'s, const N: usize>(&self, block: &'s Block) -> Option<&'s [Step; N]> {
        nothing_declared(block)?;
        let steps: &[Step; N] = block.steps[..].try_into().ok()?;
        steps.iter().all(|step| step.span == 0).then_some(steps)
    }

    /// A block that is `set slot = true`.
    fn sets_true(&self, block: &Block, slot: u32) -> Option<()> {
        let [step] = self.plain(block)?;
        let (written, value) = assigned(step)?;
        (written == slot && matches!(value.kind, NodeKind::Const(Value::Bool(true)))).then_some(())
    }

    /// `set slot = num.min(num.add(a, b), c)`: the slot and `[a, b, c]`.
    fn bound_sum(&self, step: &Step) -> Option<(u32, [u32; 3])> {
        let (slot, value) = assigned(step)?;
        let [sum, bound] = self.call_args(value, MIN)?;
        let [a, b] = self.call_locals(sum, ADD)?;
        Some((slot, [a, b, local(bound)?]))
    }

    /// `set slot = num.add(slot, 1)`: the slot.
    fn increment(&self, step: &Step) -> Option<u32> {
        let (slot, value) = assigned(step)?;
        let [from, one] = self.call_args(value, ADD)?;
        (local(from)? == slot && matches!(one.kind, NodeKind::Const(Value::Int(1)))).then_some(slot)
    }

    /// `set out[dest] = list.get(sorted, at)` with `[dest, sorted, at]`
    /// as given: `out`.
    fn placed(&self, step: &Step, [dest, sorted, at]: [u32; 3]) -> Option<u32> {
        let [list, index, item] = stored(step)?;
        (local(index)? == dest && self.item(item)? == [sorted, at]).then(|| local(list))?
    }

    /// `list.get(list, at)` of two variables: `[list, at]`.
    fn item(&self, node: &Node) -> Option<[u32; 2]> {
        let node = match &node.kind {
            NodeKind::Pin(inner) => inner,
            _ => node,
        };
        let NodeKind::Item { lib, args } = &node.kind else {
            return None;
        };
        (self.lib)(*lib)?.item_read().then_some(())?;
        let [list, at] = &args[..] else {
            return None;
        };
        Some([local(list)?, local(at)?])
    }

    /// A fused call of the kernel's native `name` on two operands.
    pub(super) fn call_args<'s>(&self, node: &'s Node, name: &str) -> Option<[&'s Node; 2]> {
        let NodeKind::Call {
            lib,
            args,
            direct: Some(_),
        } = &node.kind
        else {
            return None;
        };
        let function = (self.lib)(*lib)?;
        (function.header.name.as_str() == name
            && function.arity == 2
            && !function.operation()
            && function.limit.is_none()
            && matches!(function.run, crate::compile::LibRun::Native { .. }))
        .then_some(())?;
        let [a, b] = &args[..] else {
            return None;
        };
        Some([a, b])
    }

    /// A fused call of the kernel's native `name` on two variables.
    pub(super) fn call_locals(&self, node: &Node, name: &str) -> Option<[u32; 2]> {
        let [a, b] = self.call_args(node, name)?;
        Some([local(a)?, local(b)?])
    }
}

/// What a merge's comparison block gives.
struct Compared {
    sorted: u32,
    operands: [u32; 2],
    items: [Node; 2],
    apply: (u32, u32, Node),
    coerce: (u32, u32, Node),
    positive: Node,
    declared: Box<[u32]>,
}

/// A member write that puts nothing in a register before its write: its
/// target, index and value, whatever order the plan evaluates them in.
fn stored(step: &Step) -> Option<[&Node; 3]> {
    let StepKind::Store {
        order,
        target,
        src,
        placed: false,
    } = &step.kind
    else {
        return None;
    };
    let find = |reg: u32| order.iter().find(|node| node.reg == reg);
    Some([find(*target)?, find(target + 1)?, find(*src)?])
}

/// The plan variable a node reads.
pub(super) fn local(node: &Node) -> Option<u32> {
    match node.kind {
        NodeKind::Local(slot) => Some(slot),
        _ => None,
    }
}

/// `set slot = value`, putting nothing in a register.
pub(super) fn assigned(step: &Step) -> Option<(u32, &Node)> {
    match &step.kind {
        StepKind::Assign { slot, value } if step.span == 0 => Some((*slot, value)),
        _ => None,
    }
}

pub(super) fn nothing_declared(block: &Block) -> Option<()> {
    block.declares.is_empty().then_some(())
}

/// An empty block that declares nothing.
pub(super) fn nothing(block: &Block) -> Option<()> {
    nothing_declared(block)?;
    block.steps.is_empty().then_some(())
}
