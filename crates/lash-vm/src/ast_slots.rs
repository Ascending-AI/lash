//! The typed child slots of the semantic IR.
//!
//! [`Expr::children`] is the positional walk every pass shares. This module
//! names each of those positions: a child is reached through an [`ExprSlot`]
//! that says which role it plays in its parent, so an editor, a validator or a
//! site address never depends on a bare child index.
//!
//! The slot walk is exhaustive by construction. [`Expr::slots`] and
//! [`Expr::slots_mut`] match every [`Expr`] variant with no wildcard arm, so a
//! new IR variant does not compile until it names the slots of its children:
//! an executable construct cannot exist without an addressable, editable
//! representation.
//!
//! # Semantic contract
//!
//! The slots of a variant are listed in evaluation order, which is also the
//! order of [`Expr::children`]:
//!
//! - `Block`: `Item(i)` for each statement, run in order; the block evaluates
//!   to its last statement. `List`: `Item(i)` for each element, left to right.
//! - `Record`: `Entry(i)` for the value of the `i`-th entry, in entry order.
//! - `LabelAnnotated` and every [`StructuralRole`] but the JSON display role:
//!   `Inner`. A role never changes what runs. The JSON display role over a
//!   block exposes the block's statements directly as `Item(i)`.
//! - `Assign`: `AssignIndex(i)` for the `i`-th dynamic index of the target
//!   path, in path order, then `Value`. A simple target binds or rebinds its
//!   root in the enclosing function scope.
//! - `If`: `Condition`, `Then`, `Else`. Exactly one branch runs.
//! - `For`: `Iterable`, then per element `Bind` (when present) and `Body`. The
//!   element binding and the names `Bind` assigns are scoped to one iteration.
//! - `While`: `Condition`, `Body`, repeated while the condition holds.
//! - `HostDescriptorConstructor`: `Input`.
//! - `ReceiverCall`: `Receiver`, then `Arg(i)` left to right.
//! - `Await`, `SleepFor`, `ResultUnwrap`, `Print`, `Finish`, `Fail`, `Throw`,
//!   `FunctionReturn`, `CoercingUnary`: `Operand`.
//! - `BuiltinCall`, `FunctionCall`: `Arg(i)` left to right.
//! - `Function`, `ProcessLiteral`: `Body`, which runs in its own scope when
//!   the function is called or the process starts, never where it is written.
//! - `Call`: `Callee`, then `Arg(i)`. `MethodCall`: `Receiver`, `MethodKey`
//!   for a computed member, then `Arg(i)`. `ThisCall`: `This`, `Callee`, then
//!   `Arg(i)`.
//! - `Map`: `Items`, `Function`.
//! - `Try`: `Body`, `Catch` (its binding is scoped to the catch body), then
//!   `Finally`, which runs on every exit from the other two.
//! - `Field`: `Target`. `Index`: `Target`, `Index`.
//! - `CoercingBinary`, `OperandLogical`: `Left`, `Right`; a logical operator
//!   evaluates `Right` only when its condition selects it.
//!
//! Leaves (`Null`, `Absent`, `Bool`, `Number`, `String`, `Variable`, `Break`,
//! `Continue`, `ProcessRef`, `ResourceRef`) have no slots.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AssignPathStep, Expr, MethodKey, StructuralRole};

/// The role one child expression plays in its parent [`Expr`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExprSlot {
    /// A statement of a block or an element of a list.
    Item(u32),
    /// The value of a record entry, by entry position.
    Entry(u32),
    /// The expression a label or a structural role wraps.
    Inner,
    /// A dynamic index of an assignment target's path, by position.
    AssignIndex(u32),
    /// The value an assignment stores.
    Value,
    Condition,
    Then,
    Else,
    Iterable,
    /// The generated statements that bind a loop element to authored names.
    Bind,
    /// The body of a loop, a function, a process literal or a `try`.
    Body,
    /// The input of a host descriptor constructor.
    Input,
    Receiver,
    /// A call argument, by position.
    Arg(u32),
    /// The single operand of a unary form.
    Operand,
    Callee,
    /// The computed member a method call reads its callee from.
    MethodKey,
    This,
    /// The collection a map intrinsic reads.
    Items,
    /// The callback a map intrinsic applies.
    Function,
    Catch,
    Finally,
    Target,
    Index,
    Left,
    Right,
}

impl std::fmt::Display for ExprSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Item(index) => return write!(formatter, "item[{index}]"),
            Self::Entry(index) => return write!(formatter, "entry[{index}]"),
            Self::AssignIndex(index) => return write!(formatter, "assign_index[{index}]"),
            Self::Arg(index) => return write!(formatter, "arg[{index}]"),
            Self::Inner => "inner",
            Self::Value => "value",
            Self::Condition => "condition",
            Self::Then => "then",
            Self::Else => "else",
            Self::Iterable => "iterable",
            Self::Bind => "bind",
            Self::Body => "body",
            Self::Input => "input",
            Self::Receiver => "receiver",
            Self::Operand => "operand",
            Self::Callee => "callee",
            Self::MethodKey => "method_key",
            Self::This => "this",
            Self::Items => "items",
            Self::Function => "function",
            Self::Catch => "catch",
            Self::Finally => "finally",
            Self::Target => "target",
            Self::Index => "index",
            Self::Left => "left",
            Self::Right => "right",
        };
        formatter.write_str(name)
    }
}

fn slot_index(position: usize) -> u32 {
    u32::try_from(position).unwrap_or(u32::MAX)
}

/// Generates the shared and the mutable slot walk from one exhaustive match,
/// so the two can never disagree about a variant's slots or their order.
macro_rules! slot_walk {
    ($name:ident, $($mutability:tt)?) => {
        fn $name(expr: &$($mutability)? Expr) -> Vec<(ExprSlot, &$($mutability)? Expr)> {
            let mut slots = Vec::new();
            match expr {
                Expr::Null
                | Expr::Absent
                | Expr::Bool(_)
                | Expr::Number(_)
                | Expr::String(_)
                | Expr::Variable(_)
                | Expr::Break
                | Expr::Continue
                | Expr::ProcessRef { process: _ }
                | Expr::ResourceRef(_) => {}
                Expr::Block(items) | Expr::List(items) => {
                    for (position, item) in IntoIterator::into_iter(items).enumerate() {
                        slots.push((ExprSlot::Item(slot_index(position)), item));
                    }
                }
                Expr::LabelAnnotated { label: _, expr } => slots.push((ExprSlot::Inner, &$($mutability)? **expr)),
                Expr::Record(entries) => {
                    for (position, (_, value)) in IntoIterator::into_iter(entries).enumerate() {
                        slots.push((ExprSlot::Entry(slot_index(position)), value));
                    }
                }
                Expr::Assign { target, expr } => {
                    let mut position = 0;
                    for step in & $($mutability)? target.steps {
                        if let AssignPathStep::Index(step) = step {
                            slots.push((ExprSlot::AssignIndex(position), step));
                            position += 1;
                        }
                    }
                    slots.push((ExprSlot::Value, &$($mutability)? **expr));
                }
                Expr::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    slots.push((ExprSlot::Condition, &$($mutability)? **condition));
                    slots.push((ExprSlot::Then, &$($mutability)? **then_block));
                    slots.push((ExprSlot::Else, &$($mutability)? **else_block));
                }
                Expr::For {
                    binding: _,
                    authored_binding: _,
                    iterable,
                    bind,
                    body,
                } => {
                    slots.push((ExprSlot::Iterable, &$($mutability)? **iterable));
                    if let Some(bind) = bind {
                        slots.push((ExprSlot::Bind, &$($mutability)? **bind));
                    }
                    slots.push((ExprSlot::Body, &$($mutability)? **body));
                }
                Expr::While { condition, body } => {
                    slots.push((ExprSlot::Condition, &$($mutability)? **condition));
                    slots.push((ExprSlot::Body, &$($mutability)? **body));
                }
                Expr::Role { role, expr } => match (role, &$($mutability)? **expr) {
                    (StructuralRole::JsonTraversal, Expr::Block(items)) => {
                        for (position, item) in IntoIterator::into_iter(items).enumerate() {
                            slots.push((ExprSlot::Item(slot_index(position)), item));
                        }
                    }
                    (
                        StructuralRole::JsonTraversal
                        | StructuralRole::Scope
                        | StructuralRole::Completion
                        | StructuralRole::AttributeAssign
                        | StructuralRole::CollectionTransform { operation: _ }
                        | StructuralRole::ProcessWrapper,
                        inner,
                    ) => slots.push((ExprSlot::Inner, inner)),
                },
                Expr::HostDescriptorConstructor { type_name: _, input } => {
                    slots.push((ExprSlot::Input, &$($mutability)? **input));
                }
                Expr::ReceiverCall {
                    receiver,
                    operation: _,
                    args,
                } => {
                    slots.push((ExprSlot::Receiver, &$($mutability)? **receiver));
                    for (position, arg) in IntoIterator::into_iter(args).enumerate() {
                        slots.push((ExprSlot::Arg(slot_index(position)), arg));
                    }
                }
                Expr::Await(operand)
                | Expr::SleepFor(operand)
                | Expr::ResultUnwrap(operand)
                | Expr::Print(operand)
                | Expr::Finish(operand)
                | Expr::Fail(operand)
                | Expr::Throw(operand)
                | Expr::FunctionReturn(operand)
                | Expr::CoercingUnary { op: _, expr: operand } => {
                    slots.push((ExprSlot::Operand, &$($mutability)? **operand));
                }
                Expr::BuiltinCall { name: _, args } | Expr::FunctionCall { function: _, args } => {
                    for (position, arg) in IntoIterator::into_iter(args).enumerate() {
                        slots.push((ExprSlot::Arg(slot_index(position)), arg));
                    }
                }
                Expr::Function(function) => {
                    slots.push((ExprSlot::Body, &$($mutability)? *function.body));
                }
                Expr::ProcessLiteral(literal) => {
                    slots.push((ExprSlot::Body, &$($mutability)? *literal.body));
                }
                Expr::Call { function, args } => {
                    slots.push((ExprSlot::Callee, &$($mutability)? **function));
                    for (position, arg) in IntoIterator::into_iter(args).enumerate() {
                        slots.push((ExprSlot::Arg(slot_index(position)), arg));
                    }
                }
                Expr::MethodCall {
                    receiver,
                    method,
                    args,
                } => {
                    slots.push((ExprSlot::Receiver, &$($mutability)? **receiver));
                    match method {
                        MethodKey::Field(_) => {}
                        MethodKey::Index(key) => {
                            slots.push((ExprSlot::MethodKey, &$($mutability)? **key));
                        }
                    }
                    for (position, arg) in IntoIterator::into_iter(args).enumerate() {
                        slots.push((ExprSlot::Arg(slot_index(position)), arg));
                    }
                }
                Expr::ThisCall {
                    this,
                    function,
                    args,
                } => {
                    slots.push((ExprSlot::This, &$($mutability)? **this));
                    slots.push((ExprSlot::Callee, &$($mutability)? **function));
                    for (position, arg) in IntoIterator::into_iter(args).enumerate() {
                        slots.push((ExprSlot::Arg(slot_index(position)), arg));
                    }
                }
                Expr::Map { items, function } => {
                    slots.push((ExprSlot::Items, &$($mutability)? **items));
                    slots.push((ExprSlot::Function, &$($mutability)? **function));
                }
                Expr::Try(scope) => {
                    let scope = &$($mutability)? **scope;
                    slots.push((ExprSlot::Body, &$($mutability)? *scope.body));
                    if let Some(catch) = &$($mutability)? scope.catch {
                        slots.push((ExprSlot::Catch, &$($mutability)? *catch.body));
                    }
                    if let Some(finally) = &$($mutability)? scope.finally {
                        slots.push((ExprSlot::Finally, &$($mutability)? **finally));
                    }
                }
                Expr::Field { target, field: _ } => {
                    slots.push((ExprSlot::Target, &$($mutability)? **target));
                }
                Expr::Index { target, index } => {
                    slots.push((ExprSlot::Target, &$($mutability)? **target));
                    slots.push((ExprSlot::Index, &$($mutability)? **index));
                }
                Expr::CoercingBinary { left, op: _, right }
                | Expr::OperandLogical { left, op: _, right } => {
                    slots.push((ExprSlot::Left, &$($mutability)? **left));
                    slots.push((ExprSlot::Right, &$($mutability)? **right));
                }
            }
            slots
        }
    };
}

slot_walk!(slots_of,);
slot_walk!(slots_of_mut, mut);

impl Expr {
    /// Every direct child with the slot it occupies, in [`Expr::children`]
    /// order.
    pub fn slots(&self) -> Vec<(ExprSlot, &Expr)> {
        slots_of(self)
    }

    /// The mutable twin of [`Expr::slots`]: same slots, same order.
    pub fn slots_mut(&mut self) -> Vec<(ExprSlot, &mut Expr)> {
        slots_of_mut(self)
    }

    /// The child in `slot`, if this expression has one.
    pub fn slot(&self, slot: ExprSlot) -> Option<&Expr> {
        self.slots()
            .into_iter()
            .find_map(|(candidate, child)| (candidate == slot).then_some(child))
    }

    /// The mutable child in `slot`, if this expression has one.
    pub fn slot_mut(&mut self, slot: ExprSlot) -> Option<&mut Expr> {
        self.slots_mut()
            .into_iter()
            .find_map(|(candidate, child)| (candidate == slot).then_some(child))
    }

    /// The expression a slot path reaches from this one.
    pub fn at_slots(&self, path: &[ExprSlot]) -> Option<&Expr> {
        path.iter()
            .try_fold(self, |expression, slot| expression.slot(*slot))
    }

    /// The mutable expression a slot path reaches from this one: the one
    /// place a typed edit replaces a subtree.
    pub fn at_slots_mut(&mut self, path: &[ExprSlot]) -> Option<&mut Expr> {
        path.iter()
            .try_fold(self, |expression, slot| expression.slot_mut(*slot))
    }

    /// The typed spelling of an [`Expr::children`] index path from this
    /// expression.
    pub fn slot_path(&self, steps: &[u32]) -> Option<Vec<ExprSlot>> {
        let mut expression = self;
        let mut path = Vec::with_capacity(steps.len());
        for step in steps {
            let (slot, child) = expression.slots().into_iter().nth(*step as usize)?;
            path.push(slot);
            expression = child;
        }
        Some(path)
    }

    /// The [`Expr::children`] index path a slot path spells from this
    /// expression.
    pub fn child_steps(&self, path: &[ExprSlot]) -> Option<Vec<u32>> {
        let mut expression = self;
        let mut steps = Vec::with_capacity(path.len());
        for slot in path {
            let (position, child) = expression.slots().into_iter().enumerate().find_map(
                |(position, (candidate, child))| (candidate == *slot).then_some((position, child)),
            )?;
            steps.push(slot_index(position));
            expression = child;
        }
        Some(steps)
    }
}

/// A visitor over the typed slots of an expression tree.
pub trait ExprSlotVisitor {
    /// Called for each child with the slot path from the walk's root.
    fn visit_slot(&mut self, path: &[ExprSlot], expr: &Expr);
}

/// Visits every descendant of `expr` depth first, in evaluation order.
pub fn walk_expr_slots<V>(visitor: &mut V, expr: &Expr)
where
    V: ExprSlotVisitor + ?Sized,
{
    fn walk<V>(visitor: &mut V, expr: &Expr, path: &mut Vec<ExprSlot>)
    where
        V: ExprSlotVisitor + ?Sized,
    {
        for (slot, child) in expr.slots() {
            path.push(slot);
            visitor.visit_slot(path, child);
            walk(visitor, child, path);
            path.pop();
        }
    }
    walk(visitor, expr, &mut Vec::new());
}
