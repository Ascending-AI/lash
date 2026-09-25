//! Binding cells: how a captured binding that something assigns is shared
//! (FIG-3707).
//!
//! The capture ledger ([`super::captures`]) names the slots whose captures an
//! assignment can reach after the closure copied them. The lowering pass that
//! knows those slots mints a cell wherever such a binding comes into being:
//! `x = cell_new(value)` at every declaration, hoisted `var`, parameter,
//! `catch` binding and per-iteration copy, so each binding instance is its own
//! cell. [`box_captured_bindings`] then rewrites every other read and write of
//! the slot, in the frame that owns it and in every closure that captures it,
//! into a read or write of the cell. A closure captures the slot, which holds
//! the cell reference, so the owning frame and every closure over the binding
//! share one cell.
//!
//! A top-level session slot is never a cell. It already has one storage
//! location every frame can reach live, the session slot itself (the
//! `globalThis.name` path), so a closure over one reads and writes that slot
//! instead of capturing a copy, and a cell never has to cross a cell
//! boundary.

use std::collections::BTreeSet;

use lashlang::{AssignTarget, Expr as LashExpr};

use super::{Binding, GENERATED_BINDING_PREFIX, Lowerer};

const CELL_NEW: &str = "__typescript_cell_new";
const CELL_GET: &str = "__typescript_cell_get";
const CELL_SET: &str = "__typescript_cell_set";
const GLOBAL_GET: &str = "__typescript_global_get";
const GLOBAL_SET: &str = "__typescript_global_set";

fn builtin(name: &str, args: Vec<LashExpr>) -> LashExpr {
    LashExpr::BuiltinCall {
        name: name.into(),
        args,
    }
}

fn variable(name: &str) -> LashExpr {
    LashExpr::Variable(name.into())
}

impl Lowerer {
    /// The value `binding` is initialized with: a fresh cell holding `value`
    /// when the binding lives in one, `value` itself otherwise.
    pub(super) fn binding_initial_value(&self, binding: &Binding, value: LashExpr) -> LashExpr {
        if self.is_cell(binding) {
            builtin(CELL_NEW, vec![value])
        } else {
            value
        }
    }

    /// The per-iteration copy of a boxed classic-`for` binding
    /// (CreatePerIterationEnvironment): a fresh cell holding the current value,
    /// so a closure an earlier iteration made keeps that iteration's cell.
    pub(super) fn per_iteration_copy(&self, binding: &Binding) -> Option<LashExpr> {
        self.is_cell(binding).then(|| LashExpr::Assign {
            target: AssignTarget::variable(binding.internal.as_str().into()),
            expr: Box::new(builtin(CELL_NEW, vec![variable(&binding.internal)])),
        })
    }

    /// A fresh slot for a value that a cell is about to be minted from (a
    /// parameter or a caught exception). Drawn from its own counter so the
    /// generated names the rest of the lowering mints do not move.
    pub(super) fn cell_temporary(&mut self) -> String {
        let id = self.cell_temporaries;
        self.cell_temporaries += 1;
        let name = format!("{GENERATED_BINDING_PREFIX}cell_{id}");
        self.private_bindings.insert(name.clone());
        name
    }

    /// Mints the cell a boxed binding starts in from the value in `slot`.
    pub(super) fn cell_from_slot(internal: &str, slot: &str) -> LashExpr {
        LashExpr::Assign {
            target: AssignTarget::variable(internal.into()),
            expr: Box::new(builtin(CELL_NEW, vec![variable(slot)])),
        }
    }
}

/// Rewrites every read and write of a boxed binding in `main` (the cell's
/// root frame) and in each closure below it; see the module docs.
///
/// `session_slots` are the top-level session slots the ledger boxed: a
/// closure reaches each one live through the session slot.
pub(super) fn box_captured_bindings(
    main: &mut LashExpr,
    session_slots: &BTreeSet<String>,
    lowerer: &mut Lowerer,
) {
    let cells = declared_cells(main);
    rewrite(
        main,
        &Frame {
            cells: &cells,
            live: &BTreeSet::new(),
            session_slots,
        },
        lowerer,
    );
}

/// What one frame knows about the names it reads and writes.
struct Frame<'a> {
    /// Slots of this frame that hold a cell: the ones it declares as cells,
    /// and the captures of cells its parent holds.
    cells: &'a BTreeSet<String>,
    /// Captured top-level session slots this frame reaches live.
    live: &'a BTreeSet<String>,
    /// The top-level session slots the ledger boxed; a closure the root frame
    /// creates reaches any of them live.
    session_slots: &'a BTreeSet<String>,
}

/// The slots `body` declares as cells: every `name = cell_new(..)` in the
/// frame itself, not in a closure inside it.
fn declared_cells(body: &LashExpr) -> BTreeSet<String> {
    fn visit(expr: &LashExpr, cells: &mut BTreeSet<String>) {
        match expr {
            LashExpr::Function(_) | LashExpr::ProcessLiteral(_) => {}
            LashExpr::Assign {
                target,
                expr: value,
            } => {
                if target.steps.is_empty() && is_cell_new(value) {
                    cells.insert(target.root.to_string());
                }
                for child in expr.children() {
                    visit(child, cells);
                }
            }
            expr => {
                for child in expr.children() {
                    visit(child, cells);
                }
            }
        }
    }
    let mut cells = BTreeSet::new();
    visit(body, &mut cells);
    cells
}

fn is_cell_new(expr: &LashExpr) -> bool {
    matches!(expr, LashExpr::BuiltinCall { name, .. } if name.as_str() == CELL_NEW)
}

fn rewrite(expr: &mut LashExpr, frame: &Frame<'_>, lowerer: &mut Lowerer) {
    match expr {
        LashExpr::Variable(name) => {
            if frame.cells.contains(name.as_str()) {
                *expr = builtin(CELL_GET, vec![variable(name.as_str())]);
            } else if frame.live.contains(name.as_str()) {
                *expr = builtin(GLOBAL_GET, vec![LashExpr::String(name.clone())]);
            }
        }
        LashExpr::Assign {
            target,
            expr: value,
        } => {
            let declaration = target.steps.is_empty() && is_cell_new(value);
            for child in expr.children_mut() {
                rewrite(child, frame, lowerer);
            }
            let LashExpr::Assign {
                target,
                expr: value,
            } = expr
            else {
                unreachable!("the node matched an assignment above")
            };
            let root = target.root.to_string();
            let reaches = if frame.cells.contains(&root) {
                Some((CELL_GET, CELL_SET, variable(&root)))
            } else if frame.live.contains(&root) {
                Some((
                    GLOBAL_GET,
                    GLOBAL_SET,
                    LashExpr::String(root.as_str().into()),
                ))
            } else {
                None
            };
            let Some((get, set, handle)) = reaches else {
                return;
            };
            if declaration {
                return;
            }
            let value = std::mem::replace(value.as_mut(), LashExpr::Undefined);
            if target.steps.is_empty() {
                *expr = builtin(set, vec![handle, value]);
            } else {
                // A member write through the binding writes the object the
                // binding holds: pin it, then write through the pin.
                let base = lowerer.cell_temporary();
                let steps = std::mem::take(&mut target.steps);
                *expr = LashExpr::Block(vec![
                    LashExpr::Assign {
                        target: AssignTarget::variable(base.as_str().into()),
                        expr: Box::new(builtin(get, vec![handle])),
                    },
                    LashExpr::Assign {
                        target: AssignTarget {
                            root: base.as_str().into(),
                            steps,
                        },
                        expr: Box::new(value),
                    },
                ]);
            }
        }
        LashExpr::For { binding, bind, .. }
            if frame.cells.contains(binding.as_str()) || frame.live.contains(binding.as_str()) =>
        {
            // The loop assigns the element to its binding; route it through
            // a slot of its own and store it into the binding as `bind`'s
            // first step.
            let slot = lowerer.cell_temporary();
            let target = std::mem::replace(binding, slot.as_str().into());
            let store = LashExpr::Assign {
                target: AssignTarget::variable(target),
                expr: Box::new(variable(&slot)),
            };
            let steps = match bind.take() {
                Some(bind) => vec![store, *bind],
                None => vec![store],
            };
            *bind = Some(Box::new(LashExpr::Block(steps)));
            for child in expr.children_mut() {
                rewrite(child, frame, lowerer);
            }
        }
        LashExpr::Function(function) => {
            let captures = function
                .captures
                .iter()
                .map(|capture| capture.to_string())
                .collect::<BTreeSet<_>>();
            // A capture of a session slot the parent reaches live, or of a
            // boxed session slot the root frame owns, is read live here too.
            let live = captures
                .iter()
                .filter(|capture| {
                    frame.live.contains(*capture) || frame.session_slots.contains(*capture)
                })
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut cells = declared_cells(&function.body);
            cells.extend(
                captures
                    .iter()
                    .filter(|capture| frame.cells.contains(*capture))
                    .cloned(),
            );
            function
                .captures
                .retain(|capture| !live.contains(capture.as_str()));
            rewrite(
                &mut function.body,
                &Frame {
                    cells: &cells,
                    live: &live,
                    session_slots: &BTreeSet::new(),
                },
                lowerer,
            );
        }
        LashExpr::ProcessLiteral(literal) => {
            let cells = declared_cells(&literal.body);
            let none = BTreeSet::new();
            rewrite(
                &mut literal.body,
                &Frame {
                    cells: &cells,
                    live: &none,
                    session_slots: &none,
                },
                lowerer,
            );
        }
        expr => {
            for child in expr.children_mut() {
                rewrite(child, frame, lowerer);
            }
        }
    }
}
