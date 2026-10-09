//! What a test proves about the names it reads.
//!
//! `typeof x === "number"`, `x === null`, `x !== undefined`, `x == null`,
//! a comparison with a literal and a bare `x` each say something about `x`
//! on one side of the test. The lowerer applies it to the code that runs
//! only on that side, and only to a name [`super::Facts::narrowable`]
//! allows: one binding, never assigned, so what the test saw is what a
//! later read sees.

use super::Ty;
use crate::adapter::{BinaryOp, Expr, LogicalOp, UnaryOp};

/// What holds of some names when a test is true, and when it is false.
/// A later entry for a name replaces an earlier one.
#[derive(Clone, Debug, Default)]
pub(crate) struct Narrowing {
    pub(crate) when_true: Vec<(String, Ty)>,
    pub(crate) when_false: Vec<(String, Ty)>,
}

impl Narrowing {
    fn swapped(self) -> Self {
        Self {
            when_true: self.when_false,
            when_false: self.when_true,
        }
    }
}

/// What `test` proves. `current` gives the type a narrowable name has
/// where the test stands, and `None` for any other name; `undefined` says
/// whether the name `undefined` is the global, which no binding hides.
pub(crate) fn narrow(
    test: &Expr,
    current: &dyn Fn(&str) -> Option<Ty>,
    undefined: bool,
) -> Narrowing {
    match test {
        Expr::Unary {
            op: UnaryOp::Not,
            value,
        } => narrow(value, current, undefined).swapped(),
        Expr::Logical {
            left,
            op: LogicalOp::And,
            right,
        } => {
            let left = narrow(left, current, undefined);
            // The right operand is tested where the left one held.
            let seen = |name: &str| overlay(&left.when_true, name).or_else(|| current(name));
            let right = narrow(right, &seen, undefined);
            let mut when_true = left.when_true;
            when_true.extend(right.when_true);
            Narrowing {
                when_true,
                when_false: Vec::new(),
            }
        }
        Expr::Logical {
            left,
            op: LogicalOp::Or,
            right,
        } => {
            let left = narrow(left, current, undefined);
            let seen = |name: &str| overlay(&left.when_false, name).or_else(|| current(name));
            let right = narrow(right, &seen, undefined);
            let mut when_false = left.when_false;
            when_false.extend(right.when_false);
            Narrowing {
                when_true: Vec::new(),
                when_false,
            }
        }
        Expr::Ident(name, _) => {
            // A truthy value is neither `null` nor `undefined`.
            let Some(ty) = current(name) else {
                return Narrowing::default();
            };
            Narrowing {
                when_true: vec![(name.clone(), ty.without(&[Ty::Null, Ty::Undefined]))],
                when_false: Vec::new(),
            }
        }
        Expr::Binary {
            left, op, right, ..
        } => {
            let (strict, negated) = match op {
                BinaryOp::StrictEqual => (true, false),
                BinaryOp::StrictNotEqual => (true, true),
                BinaryOp::LooseEqual => (false, false),
                BinaryOp::LooseNotEqual => (false, true),
                _ => return Narrowing::default(),
            };
            let narrowing = equality(left, right, strict, current, undefined)
                .or_else(|| equality(right, left, strict, current, undefined))
                .unwrap_or_default();
            if negated {
                narrowing.swapped()
            } else {
                narrowing
            }
        }
        _ => Narrowing::default(),
    }
}

fn overlay(entries: &[(String, Ty)], name: &str) -> Option<Ty> {
    entries
        .iter()
        .rev()
        .find(|(entry, _)| entry == name)
        .map(|(_, ty)| ty.clone())
}

/// What `subject == other` (or `===`) proves, when `subject` is the side
/// that names the value.
fn equality(
    subject: &Expr,
    other: &Expr,
    strict: bool,
    current: &dyn Fn(&str) -> Option<Ty>,
    undefined: bool,
) -> Option<Narrowing> {
    // `typeof x == "..."`: `typeof` always gives a text, so `==` is `===`.
    if let Expr::Unary {
        op: UnaryOp::TypeOf,
        value,
    } = subject
        && let Expr::Ident(name, _) = value.as_ref()
        && let Expr::String(tag) = other
    {
        let ty = current(name)?;
        let shown = match tag.as_str() {
            "number" => Ty::Number,
            "string" => Ty::Text,
            "boolean" => Ty::Bool,
            "undefined" => Ty::Undefined,
            _ => return None,
        };
        return Some(Narrowing {
            when_true: vec![(name.clone(), ty.shown(&shown))],
            when_false: vec![(name.clone(), ty.without(&[shown]))],
        });
    }
    let Expr::Ident(name, _) = subject else {
        return None;
    };
    let ty = current(name)?;
    let shown = match other {
        Expr::Null => Ty::Null,
        Expr::Ident(name, _) if undefined && name == "undefined" => Ty::Undefined,
        Expr::String(_) if strict => Ty::Text,
        Expr::Number(_) if strict => Ty::Number,
        Expr::Bool(_) if strict => Ty::Bool,
        _ => return None,
    };
    let nullish = matches!(shown, Ty::Null | Ty::Undefined);
    Some(match (strict, nullish) {
        (true, true) => Narrowing {
            when_true: vec![(name.clone(), shown.clone())],
            when_false: vec![(name.clone(), ty.without(&[shown]))],
        },
        // `x == null` holds of both `null` and `undefined`.
        (false, _) => Narrowing {
            when_true: Vec::new(),
            when_false: vec![(name.clone(), ty.without(&[Ty::Null, Ty::Undefined]))],
        },
        // Equal to a literal: its type. Unequal: nothing.
        (true, false) => Narrowing {
            when_true: vec![(name.clone(), ty.shown(&shown))],
            when_false: Vec::new(),
        },
    })
}
