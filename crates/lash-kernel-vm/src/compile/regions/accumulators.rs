//! Linear text accumulators (FIG-6795, `docs/design/kernel-regions.md`
//! §3.5): the proof that no statement of a loop plan observes the value a
//! write of an accumulator's slot replaces.

use super::super::expr::{Expr, Kind};
use super::super::{Atom, Callee, Var};
use super::nodes::Libs;
use super::{Body, Head, LoopPlan, RegionSlot, Role, Step, Summary, root_call};

/// Whether `summary`, a write of frame slot `at` in an allocating `plan`,
/// is a linear text accumulator (FIG-6795): its value is a library
/// function's call whose first operand reads the slot, the plan's only
/// read of it, and it makes no guest call. No statement of the plan then
/// observes the value the write replaces, so where that value is a long
/// text the statement's own concatenation made, the next concatenation of
/// it may take its storage (`super::Cx::answered`).
pub(super) fn accumulates(libs: &Libs<'_>, plan: &LoopPlan, summary: &Summary, at: u32) -> bool {
    fn bare(expr: &Expr) -> &Expr {
        match &expr.kind {
            Kind::Pin(value) => bare(value),
            _ => expr,
        }
    }
    if !libs.general || summary.guest.is_some() {
        return false;
    }
    let first = match (summary.action, &*summary.roots) {
        (Some(id), []) => match root_call(&libs.exe.stmt(id).form) {
            Some((_, Callee::Library(_), args)) => matches!(
                args.first(),
                Some(Atom::Var(Var::Local(local))) if local.at == at && !local.shared
            ),
            _ => false,
        },
        (None, [(Role::Value, root)]) => match &bare(root).kind {
            Kind::Operator { args, .. } | Kind::Native { args, .. } => matches!(
                args.first().map(|first| &bare(first).kind),
                Some(Kind::Local { local, scoped: false, .. }) if local.at == at
            ),
            _ => false,
        },
        _ => false,
    };
    first && plan_reads(plan, RegionSlot::Frame(at)) == 1
}

/// How many times `plan`'s statements read `slot`: its head's and every
/// block's.
fn plan_reads(plan: &LoopPlan, slot: RegionSlot) -> usize {
    fn count(summary: &Summary, slot: RegionSlot) -> usize {
        summary.reads.iter().filter(|read| **read == slot).count()
    }
    fn body(block: &Body, slot: RegionSlot) -> usize {
        block
            .steps
            .iter()
            .map(|step| match step {
                Step::Write(summary)
                | Step::Bind(summary)
                | Step::Continue(summary)
                | Step::Store(summary)
                | Step::Discard(summary) => count(summary, slot),
                Step::If {
                    test,
                    then,
                    otherwise,
                } => count(test, slot) + body(then, slot) + body(otherwise, slot),
                Step::Leave => 0,
            })
            .sum()
    }
    let head = match &plan.head {
        Head::While(test) => count(test, slot),
        Head::For {
            charge, guarded, ..
        } => count(charge, slot) + guarded.as_deref().map_or(0, |guarded| count(guarded, slot)),
    };
    head + body(&plan.body, slot)
}
