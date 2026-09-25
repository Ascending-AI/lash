//! Mutable captures: which bindings live in a binding cell (FIG-3707).
//!
//! A closure captures by value: it copies every binding it closes over at the
//! moment it is created. That is exact for a binding nothing assigns after the
//! copy, and it is the fast path every other binding keeps. ECMA-262 closes
//! over the binding itself, so a binding that an assignment can reach after a
//! closure copied it, or that a closure assigns, is shared instead: it lives
//! in one binding cell that the frame owning it and every closure over it
//! reference. This ledger decides which bindings those are. The judgement
//! needs the whole frame, because the assignment that makes a copy stale may
//! come after the closure in source order, so the ledger collects the facts
//! while the frame lowers and judges them once lowering is done.
//!
//! Per binding, it records where each capturing closure is created and where
//! each assignment happens, as a point in the owning frame's evaluation order
//! plus the loops of that frame that enclose it. An assignment makes a capture
//! stale when it may run after the closure exists:
//!
//! - its point is later than the closure's, or
//! - both sit inside a loop the binding outlives, so a later iteration's
//!   assignment follows an earlier iteration's closure, or
//! - it runs whenever a function is called: an assignment inside a closure,
//!   or a `globalThis.name` write inside a function.
//!
//! A binding declared inside a loop is fresh on every iteration, so that loop
//! does not count against it. A classic `for` binding is one of those, and its
//! `i++` is lowered before the body, so the increment precedes the body's
//! closures: ECMA-262's CreatePerIterationEnvironment copies the binding
//! before the increment, so a closure made in the body keeps its own
//! iteration's value.
//!
//! A hoisted function declaration is created at the head of its block, before
//! any statement of that block runs, so its capture point is taken there.
//!
//! Nothing a later cell does can make a copy stale: a closure never outlives
//! the cell that made it (a binding whose value reaches a function does not
//! survive its cell), so every assignment that could follow a capture is in
//! the same program as the capture.
//!
//! The answer is per slot, not per binding: [`CaptureLedger::cell_slots`]
//! names the (frame, internal name) pairs to box. Every binding that shares a
//! slot boxes with it (sibling scopes share internal names, and a classic
//! `for` head and its per-iteration copies share one slot), so a slot never
//! holds a cell on one path and a raw value on another.

use std::collections::{BTreeMap, BTreeSet};

/// One binding's identity for the ledger. Internal names are unique only
/// where a binding is visibly shadowed, so sibling scopes can share one; the
/// ledger needs a key that two bindings never share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BindingId(usize);

/// A place in one frame's evaluation order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Site {
    point: usize,
    /// The loops of the frame that enclose the site, outermost first.
    loops: Vec<usize>,
}

/// The slot a binding lives in: its owning frame's function id and its
/// internal name. Boxing is decided per slot.
pub(super) type SlotKey = (usize, String);

struct Declared {
    slot: SlotKey,
    /// The loops the binding is declared inside. A loop in this list makes a
    /// fresh binding each iteration; a loop outside it is one the binding
    /// outlives.
    loops: Vec<usize>,
}

struct Capture {
    binding: BindingId,
    site: Site,
}

enum Write {
    At(Site),
    /// A write that runs whenever a function is called: an assignment inside
    /// a closure, or a `globalThis` write inside a function body.
    Anytime,
}

#[derive(Default)]
pub(super) struct CaptureLedger {
    next_binding: usize,
    next_point: usize,
    next_loop: usize,
    declared: BTreeMap<BindingId, Declared>,
    captures: Vec<Capture>,
    writes: Vec<(BindingId, Write)>,
}

impl CaptureLedger {
    pub(super) fn declare(&mut self, slot: SlotKey, loops: Vec<usize>) -> BindingId {
        let id = BindingId(self.next_binding);
        self.next_binding += 1;
        self.declared.insert(id, Declared { slot, loops });
        id
    }

    pub(super) fn site(&mut self, loops: &[usize]) -> Site {
        let point = self.next_point;
        self.next_point += 1;
        Site {
            point,
            loops: loops.to_vec(),
        }
    }

    pub(super) fn open_loop(&mut self) -> usize {
        let id = self.next_loop;
        self.next_loop += 1;
        id
    }

    pub(super) fn capture(&mut self, binding: BindingId, site: Site) {
        self.captures.push(Capture { binding, site });
    }

    pub(super) fn write(&mut self, binding: BindingId, site: Site) {
        self.writes.push((binding, Write::At(site)));
    }

    pub(super) fn write_anytime(&mut self, binding: BindingId) {
        self.writes.push((binding, Write::Anytime));
    }

    /// The slots that live in a binding cell: every slot one of whose
    /// captures an assignment may reach after the closure holding it was
    /// created.
    pub(super) fn cell_slots(&self) -> BTreeSet<SlotKey> {
        let mut slots = BTreeSet::new();
        for capture in &self.captures {
            let Some(declared) = self.declared.get(&capture.binding) else {
                continue;
            };
            if slots.contains(&declared.slot) {
                continue;
            }
            let stale = self
                .writes
                .iter()
                .filter(|(binding, _)| *binding == capture.binding)
                .any(|(_, write)| match write {
                    Write::Anytime => true,
                    Write::At(site) => {
                        site.point > capture.site.point
                            || shared_loops(&site.loops, &capture.site.loops) > declared.loops.len()
                    }
                });
            if stale {
                slots.insert(declared.slot.clone());
            }
        }
        slots
    }
}

/// How many enclosing loops two sites of one frame share, outermost first.
fn shared_loops(left: &[usize], right: &[usize]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}
