//! An array read or method, on a value the front end checks is one:
//!
//! ```text
//! let t = same(kind(x), "list")
//! if t {} else { if invoke ts.array.is(x) {} else { throw ... } }
//! let r = absent
//! if t { set r = <the list's own method> } else {
//!   let m = invoke ts.member.method(x)
//!   ...
//!   set r = let(f = m, let(a = [args], let(c = ts.callable(f), apply c(x, a))))
//! }
//! ```
//!
//! is `x.method(args)`, read from the general arm. Only reading the print
//! back shows the list's arm is the one the front end gives it.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;
use lash_kernel_value::sites::Site;

use super::lift::{Lift, Lifter};
use super::source::Expr;

impl Lifter<'_> {
    /// The array read or method call at the start of `block`, its result
    /// temporary, and how many statements it takes.
    pub(super) fn array_ladder(&mut self, block: &[k::Stmt]) -> Lift<Option<(Name, Expr, usize)>> {
        let [
            k::Stmt::Let {
                name: test,
                value: k::Rhs::Expr(tested),
            },
            k::Stmt::If {
                condition: k::Expr::Variable(checked),
                then_block: passes,
                else_block: checks,
            },
            k::Stmt::Let {
                name: result,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            },
            k::Stmt::If {
                condition: k::Expr::Variable(dispatched),
                else_block: general,
                ..
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let Some(target) = self.kind_test(tested, "list") else {
            return Ok(None);
        };
        if checked != test
            || dispatched != test
            || !passes.is_empty()
            || !self.array_check(checks, target)
        {
            return Ok(None);
        }
        // A typed array read uses this same list/branded dispatch. Its
        // branded arm retains the authored index read; the
        // complete print is verified against both arms by read-back.
        if let [
            k::Stmt::Assign {
                place: k::Place::Variable(assigned),
                value,
                order: k::AssignOrder::ValueFirst,
            },
        ] = general.as_slice()
            && assigned == result
            && let Some(args) = self.helper_read(value, "ts.get_numeric")
            && let [receiver, key] = args.as_slice()
            && receiver == target
        {
            let read = Expr::Index(Box::new(self.expr(target)?), Box::new(self.expr(key)?));
            return Ok(Some((result.clone(), read, 4)));
        }
        // A checked array's length: `ts.get(x, "length")` in the general arm.
        if let [
            k::Stmt::Assign {
                place: k::Place::Variable(assigned),
                value,
                order: k::AssignOrder::ValueFirst,
            },
        ] = general.as_slice()
            && assigned == result
            && let Some(args) = self.helper_read(value, "ts.get")
            && args.as_slice()
                == [
                    target.clone(),
                    k::Expr::Literal(k::Literal::Text("length".into())),
                ]
        {
            let read = Expr::Field(Box::new(self.expr(target)?), "length".into());
            return Ok(Some((result.clone(), read, 4)));
        }
        let [
            k::Stmt::Let {
                name: member,
                value: selected,
            },
            middle @ ..,
            k::Stmt::Assign {
                place: k::Place::Variable(assigned),
                value: k::Rhs::Expr(call),
                ..
            },
        ] = general.as_slice()
        else {
            return Ok(None);
        };
        let act = match selected {
            k::Rhs::Expr(k::Expr::Act(act)) => Some((**act).clone()),
            k::Rhs::Action(action) => Some(super::statements::act_of(action)),
            k::Rhs::Expr(_) => None,
        };
        let method = act.and_then(|act| match &act.kind {
            k::ActKind::Call(k::Callee::Local(id)) if act.args.as_slice() == [target.clone()] => {
                self.library(id)
                    .and_then(|name| name.strip_prefix("ts.member.").map(str::to_owned))
            }
            _ => None,
        });
        let Some(method) = method else {
            return Ok(None);
        };
        if assigned != result {
            return Ok(None);
        }
        let Some(args) = self.method_arguments(member, target, &method, call) else {
            return Ok(None);
        };
        // The general arm's own temporaries: the function values it passes.
        let saved = self.enter();
        let mut bound = Vec::new();
        let mut lifted = Ok(());
        for statement in middle {
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            } = statement
            else {
                lifted = Err("a statement among a method's arguments".to_owned());
                break;
            };
            let value = self.expr(value);
            match value {
                Ok(value) => bound.push((name.clone(), value)),
                Err(why) => {
                    lifted = Err(why);
                    break;
                }
            }
        }
        self.leave(saved);
        lifted?;
        // A local annotation can be erased while its initializer infers a
        // narrower receiver. Retain the kind check this ladder declares.
        let target = Expr::As(
            Box::new(self.expr(target)?),
            super::source::Type::Array(Box::new(super::source::Type::Word("any"))),
        );
        let mut written = Vec::with_capacity(args.len());
        for arg in args {
            let value = match arg {
                k::Expr::Variable(name) => bound
                    .iter()
                    .position(|(bound, _)| bound == name)
                    .map(|at| bound.remove(at).1),
                _ => None,
            };
            written.push(match value {
                Some(value) => value,
                None => self.expr(arg)?,
            });
        }
        if !bound.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            result.clone(),
            Expr::Call(Box::new(Expr::Field(Box::new(target), method)), written),
            4,
        )))
    }

    /// `if invoke ts.array.is(x) {} else { throw ... }`.
    fn array_check(&self, checks: &[k::Stmt], target: &k::Expr) -> bool {
        matches!(checks, [k::Stmt::If { condition: k::Expr::Act(act), then_block, else_block }]
            if then_block.is_empty()
                && matches!(else_block.as_slice(), [k::Stmt::Throw { .. }])
                && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.is(id, "ts.array.is"))
                && act.args.as_slice() == [target.clone()])
    }

    /// `x.method(args)` on a value that is not a list: the member read is a
    /// temporary waiting to be read, and `call` applies it to `x`.
    pub(super) fn method_call_expression(
        &mut self,
        held: &Name,
        value: &k::Expr,
        body: &k::Expr,
    ) -> Lift<Option<Expr>> {
        let k::Expr::Variable(member) = value else {
            return Ok(None);
        };
        let k::Expr::Let {
            body: selecting, ..
        } = body
        else {
            return Ok(None);
        };
        let k::Expr::Let { value: selects, .. } = &**selecting else {
            return Ok(None);
        };
        let k::Expr::Act(select) = &**selects else {
            return Ok(None);
        };
        let [_, target, k::Expr::Literal(k::Literal::Text(method))] = select.args.as_slice() else {
            return Ok(None);
        };
        let call = k::Expr::Let {
            name: held.clone(),
            value: Box::new(value.clone()),
            body: Box::new(body.clone()),
        };
        let Some(args) = self.method_arguments(member, target, method, &call) else {
            return Ok(None);
        };
        if self.pending_at(member).is_none() {
            return Ok(None);
        }
        let Some(read @ Expr::Field(..)) = self.take_pending(member) else {
            return Err("a method read that is not a member read".to_owned());
        };
        if !matches!(&read, Expr::Field(_, field) if field == method) {
            return Err("a method called by another name than it was read by".to_owned());
        }
        let args = args.iter().map(|arg| self.expr(arg)).collect::<Lift<_>>()?;
        Ok(Some(Expr::Call(Box::new(read), args)))
    }

    /// The arguments `call` passes the method `member` holds, with `target`
    /// its receiver: `let(f = member, let(a = [args], let(c =
    /// ts.method_callable(f, target, method), apply c(target, a))))`.
    fn method_arguments<'e>(
        &self,
        member: &Name,
        target: &k::Expr,
        method: &str,
        call: &'e k::Expr,
    ) -> Option<&'e [k::Expr]> {
        let k::Expr::Let {
            name: held,
            value: holds,
            body,
        } = call
        else {
            return None;
        };
        let k::Expr::Let {
            name: list,
            value: args,
            body,
        } = &**body
        else {
            return None;
        };
        let k::Expr::Let {
            name: callable,
            value: selects,
            body: applied,
        } = &**body
        else {
            return None;
        };
        let k::Expr::List(args) = &**args else {
            return None;
        };
        let selects = matches!(&**selects, k::Expr::Act(act)
        if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(function))
            if self.is(function, "ts.method_callable"))
            && act.args.as_slice() == [
                k::Expr::Variable(held.clone()),
                target.clone(),
                k::Expr::Literal(k::Literal::Text(method.to_owned())),
            ]);
        let applies = matches!(&**applied, k::Expr::Act(act)
            if matches!(&act.kind, k::ActKind::Call(k::Callee::Value(called)) if called == callable)
                && act.args.as_slice() == [target.clone(), k::Expr::Variable(list.clone())]);
        (**holds == k::Expr::Variable(member.clone()) && selects && applies)
            .then_some(args.as_slice())
    }
}

impl Lifter<'_> {
    /// Invert call_array_callback's positional ABI. The final read-back
    /// still proves the complete closure, temporary ordering and Imports.
    pub(super) fn array_callback_expression(
        &mut self,
        receiver: &Name,
        value: &k::Expr,
        body: &k::Expr,
    ) -> Lift<Option<Expr>> {
        let k::Expr::Let {
            name,
            value: callback,
            body,
        } = body
        else {
            return Ok(None);
        };
        let k::Expr::Closure(closure) = &**callback else {
            return Ok(None);
        };
        if closure.presentation.is_some() {
            return Ok(None);
        }
        let k::Expr::Act(call) = &**body else {
            return Ok(None);
        };
        let k::ActKind::Call(k::Callee::Local(slot)) = &call.kind else {
            return Ok(None);
        };
        let Some(helper) = self.library(slot) else {
            return Ok(None);
        };
        let Some(method) = lash_ext_ts_helpers::builtins::array_callback_method(&helper) else {
            return Ok(None);
        };
        let (context, callable) = match call.args.as_slice() {
            [target, k::Expr::Act(length), context, callable]
                if *target == k::Expr::Variable(receiver.clone())
                    && matches!(&length.kind, k::ActKind::Call(k::Callee::Local(slot)) if self.is(slot, "ts.array.length"))
                    && length.args.as_slice() == [target.clone()] =>
            {
                (Some(context), callable)
            }
            [target, k::Expr::Act(length), callable]
                if method == "sort"
                    && *target == k::Expr::Variable(receiver.clone())
                    && matches!(&length.kind, k::ActKind::Call(k::Callee::Local(slot)) if self.is(slot, "ts.array.length"))
                    && length.args.as_slice() == [target.clone()] =>
            {
                (None, callable)
            }
            _ => return Ok(None),
        };
        if *callable != k::Expr::Variable(name.clone()) {
            return Ok(None);
        }
        let definition = self
            .document
            .functions
            .get(&closure.function)
            .ok_or_else(|| "missing positional callback".to_owned())?;
        let body = &definition
            .body()
            .ok_or_else(|| "positional callback has no body".to_owned())?
            .block;
        let [this, params @ ..] = definition.signature.params.as_slice() else {
            return Ok(None);
        };
        // An arrow is faithful only when the callback does not use a dynamic
        // receiver. Retain all positional slots, including unused ones: the
        // read-back must reconstruct their exact binding identities.
        let mut pending = vec![k::Node::Block(body)];
        while let Some(node) = pending.pop() {
            if matches!(node, k::Node::Expr(k::Expr::Variable(name)) if *name == this.name) {
                return Ok(None);
            }
            pending.extend(node.children());
        }
        let mut inner = self.in_function(closure.function);
        let parameters = params
            .iter()
            .map(|param| {
                Ok(super::source::Param {
                    caption: None,
                    name: inner.binding(&param.name)?,
                    optional: false,
                    ty: None,
                })
            })
            .collect::<Lift<Vec<_>>>()?;
        let at = Site::new(
            lash_kernel_value::sites::Unit::Library(closure.function),
            Vec::new(),
        );
        let body = match body.as_slice() {
            [k::Stmt::Return { value }] => super::source::Body::Expr(inner.expr(value)?),
            body => super::source::Body::Block(inner.block(body, &at)?),
        };
        let mut args = vec![Expr::arrow(super::source::Function {
            result: None,
            is_async: false,
            params: parameters,
            body,
        })];
        if let Some(context) =
            context.filter(|value| !matches!(value, k::Expr::Literal(k::Literal::Absent)))
        {
            args.push(self.expr(context)?);
        }
        Ok(Some(Expr::Call(
            Box::new(Expr::Field(Box::new(self.expr(value)?), method.to_owned())),
            args,
        )))
    }
}
