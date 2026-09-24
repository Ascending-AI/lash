//! Mutable captures: the read path of ADR 0062 deviation register entry 5.
//!
//! A closure captures by value: it copies every binding it closes over at the
//! moment it is created. That is exact for a binding nothing assigns after the
//! copy, and a silent stale read for one that something does, because
//! ECMA-262 closes over the binding itself and reads its current value. Until
//! durable lexical cells exist, the dialect therefore refuses both halves of a
//! mutable capture. The write half (assigning to a captured binding from
//! inside the closure) refuses where the assignment is lowered. The read half
//! needs the whole frame, because the assignment that makes a copy stale may
//! come after the closure in source order, so this ledger collects the facts
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
//! - it is a `globalThis.name` write inside a function, which runs whenever
//!   that function is called.
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

use std::collections::BTreeMap;

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

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

struct Declared {
    name: String,
    /// The loops the binding is declared inside. A loop in this list makes a
    /// fresh binding each iteration; a loop outside it is one the binding
    /// outlives.
    loops: Vec<usize>,
}

struct Capture {
    binding: BindingId,
    site: Site,
    span: Option<SourceSpan>,
}

enum Write {
    At(Site),
    /// A write inside a function body to a root binding, through
    /// `globalThis`: it runs whenever the function is called.
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
    pub(super) fn declare(&mut self, name: &str, loops: Vec<usize>) -> BindingId {
        let id = BindingId(self.next_binding);
        self.next_binding += 1;
        self.declared.insert(
            id,
            Declared {
                name: name.to_string(),
                loops,
            },
        );
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

    pub(super) fn capture(&mut self, binding: BindingId, site: Site, span: Option<SourceSpan>) {
        self.captures.push(Capture {
            binding,
            site,
            span,
        });
    }

    pub(super) fn write(&mut self, binding: BindingId, site: Site) {
        self.writes.push((binding, Write::At(site)));
    }

    pub(super) fn write_anytime(&mut self, binding: BindingId) {
        self.writes.push((binding, Write::Anytime));
    }

    /// Refuses the first capture, in evaluation order, that an assignment may
    /// reach after the closure holding it was created.
    pub(super) fn refuse_stale_reads(&self) -> Result<(), Diagnostic> {
        let mut captures = self.captures.iter().collect::<Vec<_>>();
        captures.sort_by_key(|capture| capture.site.point);
        for capture in captures {
            let Some(declared) = self.declared.get(&capture.binding) else {
                continue;
            };
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
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::MutableCaptureUnsupported,
                    format!(
                        "a closure reads `{}`, which is assigned after the closure is created; a closure copies what it captures until durable lexical cells exist, so it would read a stale value",
                        declared.name
                    ),
                    format!(
                        "copy the value into a `const` before creating the closure and read that, or pass `{}` into the function as a parameter",
                        declared.name
                    ),
                    capture.span,
                ));
            }
        }
        Ok(())
    }
}

/// How many enclosing loops two sites of one frame share, outermost first.
fn shared_loops(left: &[usize], right: &[usize]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}
