//! Primitive text methods (`crate::constructs::methods`): a method
//! over constant bounds lowers to natives in place of its helper. Each rule
//! here gives back the method call; reading it lowers the same natives again,
//! including the receiver check the region begins with.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;

use super::expressions::literal_of;
use super::lift::{Lift, Lifter, temporary};
use super::source::Expr;
use crate::constructs::methods::{self, Kind};

impl Lifter<'_> {
    /// The receiver check a primitive text method begins with
    /// (`do text.len(receiver)`). The method read lowers it again.
    pub(super) fn receiver_check(&self, statement: &k::Stmt) -> bool {
        matches!(statement, k::Stmt::Do { value } if methods::TEXT_RECEIVER_CHECK.recognize(value, &|id, name| self.is(id, name)).is_some())
            || matches!(statement, k::Stmt::If { condition, then_block, else_block } if methods::SLICE_TOO_LARGE.recognize(condition, &|id, name| self.is(id, name)).is_some() && else_block.is_empty() && matches!(then_block.as_slice(), [k::Stmt::Throw { value }] if methods::SLICE_ERROR.recognize(value, &|id, name| self.is(id, name)).is_some()))
    }

    /// The kind check a method on a declared number or boolean receiver
    /// begins with (`do num.to_float(r)`, `do bool.not(r)`), where the next
    /// statement calls a built-in's helper on that receiver: the method call
    /// lowers it again.
    pub(super) fn typed_receiver_check(&self, block: &[k::Stmt]) -> bool {
        let [
            k::Stmt::Do {
                value: k::Expr::Call { function, args },
            },
            next,
            ..,
        ] = block
        else {
            return false;
        };
        let [receiver @ k::Expr::Variable(_)] = args.as_slice() else {
            return false;
        };
        if !self.is(function, "num.to_float") && !self.is(function, "bool.not") {
            return false;
        }
        let table = lash_ext_ts_helpers::builtins::table();
        let mut nodes = vec![k::Node::Stmt(next)];
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
                && act.args.first() == Some(receiver)
                && table
                    .functions
                    .iter()
                    .any(|(_, helper)| self.is(id, helper))
            {
                return true;
            }
            nodes.extend(node.children());
        }
        false
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
                && self.is(id, methods::SLICE_UNITS)
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
        let Some(found) = methods::ENDS_WITH.recognize(block, &|id, name| self.is(id, name)) else {
            return Ok(None);
        };
        let methods::EndsWith {
            start,
            result,
            receiver,
            needle,
            position,
        } = found;
        if !temporary(start) || !temporary(result) {
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
        let mut args = vec![literal(needle)?];
        args.extend(end);
        let call = self.own_method(
            receiver,
            "value.method",
            methods::source_method(Kind::EndsWith),
            args,
        )?;
        Ok(Some((result.clone(), call)))
    }

    /// A method a primitive text region lowered to natives, as its call.
    pub(super) fn primitive_method(
        &mut self,
        function: &lash_kernel_value::FunctionId,
        args: &[k::Expr],
    ) -> Lift<Option<Expr>> {
        let value = k::Expr::Call {
            function: *function,
            args: args.to_vec(),
        };
        if let Some(values) = methods::LIST_SLICE.recognize(&value, &|id, name| self.is(id, name))
            && let [receiver, start, end] = values.as_slice()
            && let Some(Some(start)) = self.position(start, receiver)
            && let Some(end) = self.position(end, receiver)
        {
            let mut args = vec![start];
            args.extend(end);
            return self
                .own_method(receiver, "list.method", "slice", args)
                .map(Some);
        }
        for declaration in methods::METHODS {
            let Some(recipe) = declaration.expression() else {
                continue;
            };
            let Some(values) = recipe.recognize(&value, &|id, name| self.is(id, name)) else {
                continue;
            };
            let (receiver, args) = match (declaration.kind(), values.as_slice()) {
                (Kind::Trim, [receiver, k::Expr::Literal(k::Literal::Text(set))])
                    if set == methods::trim_set() =>
                {
                    (*receiver, Vec::new())
                }
                (Kind::Split, [receiver, needle @ k::Expr::Literal(k::Literal::Text(_))]) => {
                    (*receiver, vec![literal(needle)?])
                }
                (
                    Kind::Search,
                    [
                        receiver,
                        needle @ k::Expr::Literal(k::Literal::Text(_)),
                        position,
                    ],
                ) => {
                    let Some(position) = self.absolute(position, receiver) else {
                        continue;
                    };
                    let needle = literal(needle)?;
                    let args = match position {
                        Some(Expr::Number(zero)) if zero == "0" => vec![needle],
                        Some(position) => vec![needle, position],
                        None => vec![needle, Expr::Ident("Infinity".into())],
                    };
                    (*receiver, args)
                }
                _ => continue,
            };
            return self
                .own_method(receiver, "value.method", declaration.name, args)
                .map(Some);
        }
        Ok(None)
    }

    /// `receiver.method(args)`, or under [`super::style::Style::exact`] on a
    /// receiver binding the kind's own row, `spelling(receiver, "method",
    /// args)`: a capture cell is of no known kind.
    fn own_method(
        &mut self,
        receiver: &k::Expr,
        spelling: &str,
        method: &str,
        args: Vec<Expr>,
    ) -> Lift<Expr> {
        let exact = self.style.exact && self.reads_binding(receiver);
        let receiver = self.expr(receiver)?;
        Ok(if exact {
            super::expressions::constructor(
                spelling,
                [receiver, Expr::Text(method.to_owned())]
                    .into_iter()
                    .chain(args)
                    .collect(),
            )
        } else {
            Expr::Call(
                Box::new(Expr::Field(Box::new(receiver), method.to_owned())),
                args,
            )
        })
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

    fn bound(&self, bound: &k::Expr, receiver: &k::Expr, relative: bool) -> Option<Option<Expr>> {
        methods::bound(bound, receiver, relative, &|id, name| self.is(id, name))?
            .map_or(Some(None), |value| {
                super::source::number(value).map(|number| Some(Expr::Number(number)))
            })
    }

    fn length(&self, expr: &k::Expr, receiver: &k::Expr) -> bool {
        methods::TEXT_LENGTH
            .recognize(expr, &|id, name| self.is(id, name))
            .is_some_and(|args| args.as_slice() == [receiver])
    }
}

fn literal(expr: &k::Expr) -> Lift<Expr> {
    match expr {
        k::Expr::Literal(literal) => literal_of(literal),
        _ => super::lift::refuse("a literal argument"),
    }
}
