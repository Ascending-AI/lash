//! Primitive text methods (`crate::lower::primitive_methods`): a method
//! over constant bounds lowers to natives in place of its helper. Each rule
//! here gives back the method call; reading it lowers the same natives again,
//! including the receiver check the region begins with.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;

use super::expressions::literal_of;
use super::lift::{Lift, Lifter, temporary};
use super::source::Expr;
use crate::lower::primitive_methods::TRIM_SET;

/// The `split` limit the front end passes when the source gave none.
const SPLIT_LIMIT: i64 = 4_294_967_295;

impl Lifter<'_> {
    /// The receiver check a primitive text method begins with
    /// (`do text.len(receiver)`). The method read lowers it again.
    pub(super) fn receiver_check(&self, statement: &k::Stmt) -> bool {
        matches!(statement, k::Stmt::Do { value: k::Expr::Call { function, args } }
            if self.is(function, "text.len") && args.len() == 1)
    }

    /// `let start = …; let end = …;` ahead of the `ts.string.slice_units`
    /// call that reads them: the bounds of `receiver.slice(start, end)`,
    /// written where that call reads them.
    pub(super) fn slice_bounds(&self, block: &[k::Stmt]) -> Option<[(Name, Expr); 2]> {
        let [
            k::Stmt::Let {
                name: start,
                value: k::Rhs::Expr(lower),
            },
            k::Stmt::Let {
                name: end,
                value: k::Rhs::Expr(upper),
            },
            consumer,
            ..,
        ] = block
        else {
            return None;
        };
        if !temporary(start) || !temporary(end) {
            return None;
        }
        let receiver = self.slice_receiver(consumer, start, end)?;
        let lower = match self.position(lower, &receiver)? {
            Some(lower) => lower,
            None => Expr::Ident("Infinity".into()),
        };
        // The whole length is an omitted end; `helper` drops it.
        let upper = self.position(upper, &receiver)?.unwrap_or(Expr::Undefined);
        Some([(start.clone(), lower), (end.clone(), upper)])
    }

    /// The receiver of the `ts.string.slice_units(receiver, start, end)` call
    /// in `statement`, whether its own action or nested in an expression.
    fn slice_receiver(&self, statement: &k::Stmt, start: &Name, end: &Name) -> Option<k::Expr> {
        let reads = |args: &[k::Expr]| match args {
            [
                receiver,
                k::Expr::Variable(first),
                k::Expr::Variable(second),
            ] if first == start && second == end => Some(receiver.clone()),
            _ => None,
        };
        let mut nodes = vec![k::Node::Stmt(statement)];
        while let Some(node) = nodes.pop() {
            let found = match node {
                k::Node::Action(action @ k::Action::Call { .. }) => {
                    Some(super::statements::act_of(action))
                }
                k::Node::Expr(k::Expr::Act(act)) => Some((**act).clone()),
                _ => None,
            };
            if let Some(act) = found
                && let k::ActKind::Call(k::Callee::Local(id)) = &act.kind
                && self.is(id, "ts.string.slice_units")
                && let Some(receiver) = reads(&act.args)
            {
                return Some(receiver);
            }
            nodes.extend(node.children());
        }
        None
    }

    /// `endsWith`'s region: its start, its result, and the test that sets
    /// the result, read as `receiver.endsWith(needle)`.
    pub(super) fn ends_with(&mut self, block: &[k::Stmt]) -> Lift<Option<(Name, Expr)>> {
        let [
            k::Stmt::Let {
                name: start,
                value:
                    k::Rhs::Expr(k::Expr::Call {
                        function: sub,
                        args: from,
                    }),
            },
            k::Stmt::Let {
                name: result,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
            },
            k::Stmt::If {
                condition:
                    k::Expr::Call {
                        function: lt,
                        args: negative,
                    },
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let [position, width] = from.as_slice() else {
            return Ok(None);
        };
        let [
            k::Stmt::Assign {
                place: k::Place::Variable(set),
                value: k::Rhs::Expr(matched),
                ..
            },
        ] = else_block.as_slice()
        else {
            return Ok(None);
        };
        let Some([needle]) = self.native_args(width, "text.utf16_len") else {
            return Ok(None);
        };
        let Some([found, at]) = self.native_args(matched, "eq") else {
            return Ok(None);
        };
        let Some([receiver, searched, from]) = self.native_args(found, "text.utf16_find") else {
            return Ok(None);
        };
        let starts_here = |expr: &k::Expr| matches!(expr, k::Expr::Variable(name) if name == start);
        if !temporary(start)
            || !temporary(result)
            || set != result
            || !self.is(sub, "num.sub")
            || !self.is(lt, "num.lt")
            || negative.as_slice()
                != [
                    k::Expr::Variable(start.clone()),
                    k::Expr::Literal(k::Literal::Int(k::Integer::from(0_i64))),
                ]
            || !then_block.is_empty()
            || !matches!(needle, k::Expr::Literal(k::Literal::Text(_)))
            || searched != needle
            || !starts_here(from)
            || !starts_here(at)
        {
            return Ok(None);
        }
        // The end defaults to the whole length.
        let end = if self.length(position, receiver) {
            None
        } else {
            match self.absolute(position, receiver) {
                Some(Some(end)) => Some(end),
                _ => return Ok(None),
            }
        };
        let receiver = self.expr(receiver)?;
        let mut args = vec![literal(needle)?];
        args.extend(end);
        Ok(Some((
            result.clone(),
            Expr::Call(
                Box::new(Expr::Field(Box::new(receiver), "endsWith".into())),
                args,
            ),
        )))
    }

    /// A method a primitive text region lowered to natives, as its call.
    pub(super) fn primitive_method(&mut self, name: &str, args: &[k::Expr]) -> Lift<Option<Expr>> {
        let (receiver, method, args): (&k::Expr, &str, Vec<Expr>) = match (name, args) {
            (
                "text.trim_set",
                [
                    receiver,
                    k::Expr::Literal(k::Literal::Text(set)),
                    k::Expr::Literal(k::Literal::Bool(start)),
                    k::Expr::Literal(k::Literal::Bool(end)),
                ],
            ) if set == TRIM_SET => {
                let method = match (start, end) {
                    (true, true) => "trim",
                    (true, false) => "trimStart",
                    (false, true) => "trimEnd",
                    (false, false) => return Ok(None),
                };
                (receiver, method, Vec::new())
            }
            (
                "text.split",
                [
                    receiver,
                    needle @ k::Expr::Literal(k::Literal::Text(_)),
                    k::Expr::Literal(k::Literal::Int(limit)),
                ],
            ) if *limit == k::Integer::from(SPLIT_LIMIT) => {
                (receiver, "split", vec![literal(needle)?])
            }
            ("num.to_float", [found]) => match self.search(found, None) {
                Some((receiver, args)) => (receiver, "indexOf", args?),
                None => return Ok(None),
            },
            ("num.le", [k::Expr::Literal(k::Literal::Int(zero)), found])
                if *zero == k::Integer::from(0_i64) =>
            {
                match self.search(found, None) {
                    Some((receiver, args)) => (receiver, "includes", args?),
                    None => return Ok(None),
                }
            }
            ("eq", [found, at]) => match self.search(found, Some(at)) {
                Some((receiver, args)) => (receiver, "startsWith", args?),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        let receiver = self.expr(receiver)?;
        Ok(Some(Expr::Call(
            Box::new(Expr::Field(Box::new(receiver), method.into())),
            args,
        )))
    }

    /// `text.utf16_find(receiver, needle, position)`: its receiver, and the
    /// needle and position as the method's arguments. `at`, when given, is
    /// the position the match must be found at (`startsWith`).
    fn search<'e>(
        &self,
        found: &'e k::Expr,
        at: Option<&k::Expr>,
    ) -> Option<(&'e k::Expr, Lift<Vec<Expr>>)> {
        let k::Expr::Call { function, args } = found else {
            return None;
        };
        let [
            receiver,
            needle @ k::Expr::Literal(k::Literal::Text(_)),
            position,
        ] = args.as_slice()
        else {
            return None;
        };
        if !self.is(function, "text.utf16_find") || at.is_some_and(|at| at != position) {
            return None;
        }
        let position = self.absolute(position, receiver)?;
        let args = literal(needle).map(|needle| match position {
            // Zero is the default position.
            Some(Expr::Number(zero)) if zero == "0" => vec![needle],
            Some(position) => vec![needle, position],
            None => vec![needle, Expr::Ident("Infinity".into())],
        });
        Some((receiver, args))
    }

    /// A relative bound (`slice`) of `receiver`, as the source wrote it:
    /// `None` for the whole length.
    fn position(&self, bound: &k::Expr, receiver: &k::Expr) -> Option<Option<Expr>> {
        self.bound(bound, receiver, true)
    }

    /// An absolute search position of `receiver`.
    fn absolute(&self, position: &k::Expr, receiver: &k::Expr) -> Option<Option<Expr>> {
        self.bound(position, receiver, false)
    }

    /// `crate::lower::primitive_methods::primitive_position`, inverted.
    fn bound(&self, bound: &k::Expr, receiver: &k::Expr, relative: bool) -> Option<Option<Expr>> {
        if *bound == k::Expr::Literal(k::Literal::Int(k::Integer::from(0_i64))) {
            return Some(Some(Expr::Number("0".into())));
        }
        if self.length(bound, receiver) {
            return Some(None);
        }
        // float.to_int(num.to_float(num.min(num.max(value, 0), length)))
        let [clamped] = self.native_args(bound, "float.to_int")? else {
            return None;
        };
        let [clamped] = self.native_args(clamped, "num.to_float")? else {
            return None;
        };
        let [clamped, length] = self.native_args(clamped, "num.min")? else {
            return None;
        };
        let [value, zero] = self.native_args(clamped, "num.max")? else {
            return None;
        };
        if !self.length(length, receiver)
            || *zero != k::Expr::Literal(k::Literal::Int(k::Integer::from(0_i64)))
        {
            return None;
        }
        let value = match value {
            k::Expr::Literal(k::Literal::Float(value)) if value.get() > 0.0 => value.get(),
            // A negative relative bound counts back from the length.
            value if relative => {
                let [length, k::Expr::Literal(k::Literal::Float(value))] =
                    self.native_args(value, "num.add")?
                else {
                    return None;
                };
                if !self.length(length, receiver) || value.get() >= 0.0 {
                    return None;
                }
                value.get()
            }
            _ => return None,
        };
        super::source::number(value).map(|number| Some(Expr::Number(number)))
    }

    fn length(&self, expr: &k::Expr, receiver: &k::Expr) -> bool {
        self.native_args(expr, "text.utf16_len")
            .is_some_and(|args| matches!(args, [of] if of == receiver))
    }

    fn native_args<'e>(&self, expr: &'e k::Expr, name: &str) -> Option<&'e [k::Expr]> {
        match expr {
            k::Expr::Call { function, args } if self.is(function, name) => Some(args),
            _ => None,
        }
    }
}

fn literal(expr: &k::Expr) -> Lift<Expr> {
    match expr {
        k::Expr::Literal(literal) => literal_of(literal),
        _ => super::lift::refuse("a literal argument"),
    }
}
