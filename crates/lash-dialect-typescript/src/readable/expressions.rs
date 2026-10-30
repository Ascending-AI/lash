//! Expressions: operators from the library calls they lower to, waits from
//! their actions, and the scoped forms of `&&`, `||`, `??` and calls.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;

use super::lift::{Lift, Lifter, refuse};
use super::source::{self, Expr, Part};

impl Lifter<'_> {
    pub(super) fn expr(&mut self, expr: &k::Expr) -> Lift<Expr> {
        if let Some(function) = self.closure(expr)? {
            return Ok(Expr::arrow(function));
        }
        match expr {
            k::Expr::Literal(literal) => literal_of(literal),
            k::Expr::Variable(name) => self.variable(name),
            k::Expr::If { cond, then, r#else } => {
                if let Some(name) = self.process_await_name(then, r#else)
                    && let Some(awaited) = self.take_pending(name)
                {
                    return Ok(awaited);
                }
                let test = self.condition(cond)?;
                let then = self.expr(then)?;
                let otherwise = self.expr(r#else)?;
                Ok(Expr::Conditional(
                    Box::new(test),
                    Box::new(then),
                    Box::new(otherwise),
                ))
            }
            k::Expr::Let { name, value, body } => self.scoped(name, value, body),
            k::Expr::List(items) => Ok(Expr::Array(
                items
                    .iter()
                    .map(|item| self.expr(item))
                    .collect::<Lift<_>>()?,
            )),
            k::Expr::Tuple(items) | k::Expr::Set(items) => {
                if let Some(value) = crate::constructs::numbers::bigint_value(expr) {
                    return Ok(Expr::Number(format!("{value}n")));
                }
                if matches!(expr, k::Expr::Tuple(_))
                    && let Some(value) = builtin_value(items)
                {
                    return Ok(value);
                }
                Ok(constructor(
                    if matches!(expr, k::Expr::Tuple(_)) {
                        "tuple.of"
                    } else {
                        "set.of"
                    },
                    items.iter().map(|v| self.expr(v)).collect::<Lift<_>>()?,
                ))
            }
            k::Expr::Map(entries) => Ok(constructor(
                "map.from_entries",
                entries
                    .iter()
                    .map(|e| Ok(Expr::Array(vec![self.expr(&e.key)?, self.expr(&e.value)?])))
                    .collect::<Lift<_>>()?,
            )),
            k::Expr::Record(entries) => {
                let mut fields = Vec::with_capacity(entries.len());
                for entry in entries {
                    if entry.field == "__proto__" {
                        return refuse("a field an object literal cannot name");
                    }
                    fields.push((entry.field.clone(), self.expr(&entry.value)?));
                }
                Ok(Expr::Object(fields))
            }
            k::Expr::Member(member) => match &**member {
                k::Member::Field { target, field } => {
                    let target = self.expr(target)?;
                    Ok(Expr::Field(Box::new(target), field.clone()))
                }
                k::Member::Index { target, index } => {
                    let target = self.expr(target)?;
                    let index = self.expr(index)?;
                    Ok(constructor("tuple.at", vec![target, index]))
                }
            },
            k::Expr::Call { function, args } => self.native(function, args),
            k::Expr::Act(act) => self.act(act),
            _ => refuse("an expression the authored dialect does not lower to"),
        }
    }

    /// A tested value: the dialect's truthiness conversion is the test.
    pub(super) fn condition(&mut self, expr: &k::Expr) -> Lift<Expr> {
        if let k::Expr::Call { function, args } = expr
            && self.is(function, "ts.to_boolean")
            && let [value] = args.as_slice()
        {
            return self.expr(value);
        }
        self.expr(expr)
    }

    /// A temporary the source wrote in place, or a variable.
    fn variable(&mut self, name: &Name) -> Lift<Expr> {
        if let Some(value) = self.take_pending(name) {
            return Ok(value);
        }
        Ok(Expr::Ident(self.binding(name)?))
    }

    /// `await_process` holds the ordinary await before checking for a process
    /// handle. Keep it pending until that check reads it, in either scope form.
    pub(super) fn process_await_binding(&self, name: &Name, value: &k::Expr) -> bool {
        self.process_awaits.contains(name)
            && matches!(value, k::Expr::Act(act)
            if matches!(&act.kind, k::ActKind::Await(policy) if self.is(policy, "ts.await")))
    }

    /// The process branch is implicit in the original `await`. Full read-back
    /// verification also checks the guard and the boundary's forwarding factory.
    pub(super) fn process_await_name<'a>(
        &self,
        then: &'a k::Expr,
        otherwise: &'a k::Expr,
    ) -> Option<&'a Name> {
        let (k::Expr::Act(act), k::Expr::Variable(name)) = (then, otherwise) else {
            return None;
        };
        let k::ActKind::Perform {
            slot,
            then_yield: true,
            ..
        } = &act.kind
        else {
            return None;
        };
        if self.imports.get(slot)?.name != lash_kernel_dialect::service_name("processes", "await") {
            return None;
        }
        let [k::Expr::Act(boundary)] = act.args.as_slice() else {
            return None;
        };
        if !matches!(&boundary.kind, k::ActKind::Call(k::Callee::Local(id))
            if self.is(id, crate::constructs::conventions::ARGUMENT_COPY))
        {
            return None;
        }
        let [k::Expr::Record(fields), _factory] = boundary.args.as_slice() else {
            return None;
        };
        let [field] = fields.as_slice() else {
            return None;
        };
        if field.field != "handle" || field.value != k::Expr::Variable(name.clone()) {
            return None;
        }
        Some(name)
    }

    fn native(&mut self, function: &lash_kernel_value::FunctionId, args: &[k::Expr]) -> Lift<Expr> {
        let Some(name) = self.library(function) else {
            return refuse("a call of a function the manifest does not list");
        };
        if name == "num.to_float"
            && let [k::Expr::Call { function, args }] = args
            && self.is(function, "list.len")
            && let [target] = args.as_slice()
        {
            return Ok(Expr::Field(
                Box::new(self.expr(target)?),
                "length".to_owned(),
            ));
        }
        if let Some(method) = self.primitive_method(function, args)? {
            return Ok(method);
        }
        if name == "text.concat"
            && args.len() == 2
            && args.iter().any(|part| {
                matches!(part,
                k::Expr::Call { function, .. } if self.is(function, "ts.to_string"))
            })
        {
            return self.template(args);
        }
        if self.style.operators
            && let Some(operator) = self.operator(&name, args)
        {
            return operator;
        }
        if let Some(spelling) =
            lash_kernel_dialect::value_constructors::Constructor::spelling(&name)
        {
            return Ok(constructor(
                spelling,
                args.iter().map(|v| self.expr(v)).collect::<Lift<_>>()?,
            ));
        }
        match (name.as_str(), args) {
            // A materialized capture cell starts as null and is filled after
            // its closures exist. Keep the strict text operation even when
            // rereading cannot infer text from that initializer. Assertions
            // erase without adding checks, calls or operand evaluations.
            ("text.concat", [left, right]) => Ok(Expr::Binary(
                "+",
                Box::new(Expr::As(
                    Box::new(self.expr(left)?),
                    source::Type::Word("string"),
                )),
                Box::new(Expr::As(
                    Box::new(self.expr(right)?),
                    source::Type::Word("string"),
                )),
            )),
            ("num.to_float", [value]) => self.expr(value),
            (name, [value]) => match crate::constructs::operators::unary_inverse(name, true) {
                Some((token, condition)) => Ok(Expr::Unary(
                    token,
                    Box::new(if condition {
                        self.condition(value)?
                    } else {
                        self.expr(value)?
                    }),
                )),
                None => refuse(format!("`{name}` has no source spelling")),
            },
            (
                "text.join",
                [
                    k::Expr::List(parts),
                    k::Expr::Literal(k::Literal::Text(separator)),
                ],
            ) if separator.is_empty() => self.template(parts),
            (name, [left, right]) => match crate::constructs::operators::native_inverse(name) {
                Some(op) => self.binary(op, left, right),
                None => refuse(format!("`{name}` has no operator")),
            },
            (name, _) => refuse(format!("`{name}` has no source spelling")),
        }
    }

    /// The operator a numeric or text native lowers from, over operands
    /// whose `number` checks the operator lowers again.
    fn operator(&mut self, name: &str, args: &[k::Expr]) -> Option<Lift<Expr>> {
        match (name, args) {
            (crate::constructs::operators::NUMBER_GUARD, [value]) => Some(self.expr(value)),
            // Text operands read as text without the assertions.
            ("text.concat", [left, right]) => Some(self.binary("+", left, right)),
            (name, [k::Expr::Call { function, args }])
                if name == crate::constructs::operators::native_not()
                    && self.is(
                        function,
                        crate::types::Direct::NumericEqual { negated: false }.function(),
                    ) =>
            {
                match args.as_slice() {
                    [left, right] => Some(self.binary("!==", left, right)),
                    _ => None,
                }
            }
            (name, [value]) => {
                crate::constructs::operators::unary_inverse(name, true).map(|(token, condition)| {
                    let value = if condition {
                        self.condition(value)
                    } else {
                        self.expr(value)
                    };
                    value.map(|value| Expr::Unary(token, Box::new(value)))
                })
            }
            (name, [left, right]) => crate::constructs::operators::native_inverse(name)
                .map(|operator| self.binary(operator, left, right)),
            _ => None,
        }
    }

    /// `left op right`. Pinned operands the source evaluated right to left
    /// are a comparison written the other way round.
    fn binary(&mut self, op: &'static str, left: &k::Expr, right: &k::Expr) -> Lift<Expr> {
        if let (k::Expr::Variable(first), k::Expr::Variable(second)) = (left, right)
            && let (Some(first), Some(second)) = (self.pending_at(first), self.pending_at(second))
            && second < first
            && let Some(mirrored) = crate::constructs::operators::mirror(op)
        {
            let right = self.expr(right)?;
            let left = self.expr(left)?;
            return Ok(Expr::Binary(mirrored, Box::new(right), Box::new(left)));
        }
        let mut operand = |value: &k::Expr| {
            if !self.style.operators
                && crate::constructs::operators::asserted(op)
                && let k::Expr::Call { function, args } = value
                && self.is(function, crate::constructs::operators::NUMBER_GUARD)
                && let [value] = args.as_slice()
            {
                return Ok(Expr::As(
                    Box::new(self.expr(value)?),
                    source::Type::Word("number"),
                ));
            }
            self.expr(value)
        };
        let left = operand(left)?;
        let right = operand(right)?;
        Ok(Expr::Binary(op, Box::new(left), Box::new(right)))
    }

    fn template(&mut self, parts: &[k::Expr]) -> Lift<Expr> {
        let mut written = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                k::Expr::Literal(k::Literal::Text(text)) => written.push(Part::Text(text.clone())),
                k::Expr::Call { function, args } if self.is(function, "ts.to_string") => {
                    let [value] = args.as_slice() else {
                        return refuse("a conversion of one value");
                    };
                    written.push(Part::Expr(self.expr(value)?));
                }
                value => written.push(Part::Expr(self.expr(value)?)),
            }
        }
        Ok(Expr::Template(written))
    }

    /// `let name = value in body`: the scoped forms the front end writes.
    fn scoped(&mut self, name: &Name, value: &k::Expr, body: &k::Expr) -> Lift<Expr> {
        if self.process_await_binding(name, value) {
            return self.scoped_temporary(name, value, body);
        }
        if let Some(method) = self.array_callback_expression(name, value, body)? {
            return Ok(method);
        }
        if let Some(method) = self.method_call_expression(name, value, body)? {
            return Ok(method);
        }
        let bound = k::Expr::Variable(name.clone());
        // `await sleep(ms)` gives undefined.
        if let k::Expr::Act(act) = value
            && let k::ActKind::Perform {
                slot,
                then_yield: true,
                ..
            } = act.kind
            && self.imports.get(&slot).is_some_and(|import| {
                import.name == lash_kernel_dialect::service_name("typescript", "sleep")
            })
            && let [duration] = act.args.as_slice()
            && lash_dialect_conventions::typescript::SLEEP_RESULT.matches_result(body)
        {
            let k::Expr::Act(conversion) = duration else {
                return refuse("a sleep duration conversion");
            };
            let k::ActKind::Call(k::Callee::Local(function)) = &conversion.kind else {
                return refuse("a sleep duration library reference");
            };
            if !self.is(function, "ts.sleep_duration") {
                return refuse("the selected sleep duration helper");
            }
            let [authored] = conversion.args.as_slice() else {
                return refuse("one authored sleep duration");
            };
            let duration = self.expr(authored)?;
            return Ok(Expr::Unary(
                "await ",
                Box::new(Expr::Call(
                    Box::new(Expr::Ident("sleep".to_owned())),
                    vec![duration],
                )),
            ));
        }
        if let k::Expr::If { cond, then, r#else } = body
            && **cond == bound
        {
            // `a && b` and `a || b` test `a` once.
            if **r#else == bound {
                let left = self.expr(value)?;
                let right = self.expr(then)?;
                return Ok(Expr::Binary("&&", Box::new(left), Box::new(right)));
            }
            if **then == bound {
                let left = self.expr(value)?;
                let right = self.expr(r#else)?;
                return Ok(Expr::Binary("||", Box::new(left), Box::new(right)));
            }
        }
        // `a ?? b`.
        if let k::Expr::If { cond, then, r#else } = body
            && **r#else == bound
            && self.nullish(cond, name)
        {
            let left = self.expr(value)?;
            let right = self.expr(then)?;
            return Ok(Expr::Binary("??", Box::new(left), Box::new(right)));
        }
        if *body == bound {
            return self.expr(value);
        }
        if let Some(call) = self.call(name, value, body)? {
            return Ok(call);
        }
        if super::lift::temporary(name) {
            return self.scoped_temporary(name, value, body);
        }
        refuse("a scoped value the authored dialect does not lower to")
    }

    /// `if(same(name, null), true, same(name, absent))`.
    pub(super) fn nullish(&self, test: &k::Expr, name: &Name) -> bool {
        let same = |expr: &k::Expr, literal: k::Literal| {
            matches!(expr, k::Expr::Call { function, args }
                if self.is(function, "same")
                    && args.as_slice() == [k::Expr::Variable(name.clone()), k::Expr::Literal(literal)])
        };
        matches!(test, k::Expr::If { cond, then, r#else }
            if same(cond, k::Literal::Null)
                && **then == k::Expr::Literal(k::Literal::Bool(true))
                && same(r#else, k::Literal::Absent))
    }

    /// `f(args)` of a function value: `let(f' = f, let(a = [args],
    /// let(c = f'[4], apply c(absent, a))))`.
    fn call(&mut self, callee: &Name, value: &k::Expr, body: &k::Expr) -> Lift<Option<Expr>> {
        let k::Expr::Let {
            name: list,
            value: args,
            body: inner,
        } = body
        else {
            return Ok(None);
        };
        let k::Expr::List(args) = &**args else {
            return Ok(None);
        };
        let k::Expr::Let {
            name: callable,
            value: selected,
            body: applied,
        } = &**inner
        else {
            return Ok(None);
        };
        let held = k::Expr::Variable(callee.clone());
        let direct = matches!(&**selected, k::Expr::Member(_));
        let selects = match &**selected {
            k::Expr::Member(member) => matches!(&**member, k::Member::Index { target, index }
            if *target == held
                && *index == k::Expr::Literal(k::Literal::Float(k::Float::new(
                    f64::from(crate::FUNCTION_CALLABLE),
                )))),
            k::Expr::Call { function, args } => {
                self.is(function, "ts.callable") && args.as_slice() == [held.clone()]
            }
            k::Expr::Act(act) => {
                matches!(&act.kind, k::ActKind::Call(k::Callee::Local(function))
                    if self.is(function, "ts.callable"))
                    && act.args.as_slice() == [held.clone()]
            }
            _ => false,
        };
        let k::Expr::Act(act) = &**applied else {
            return Ok(None);
        };
        let applies = matches!(&act.kind, k::ActKind::Call(k::Callee::Value(value)) if value == callable)
            && act.args.as_slice()
                == [
                    k::Expr::Literal(k::Literal::Absent),
                    k::Expr::Variable(list.clone()),
                ];
        if !selects || !applies {
            return Ok(None);
        }
        let callee = self.expr(value)?;
        let args = args.iter().map(|arg| self.expr(arg)).collect::<Lift<_>>()?;
        let authored =
            matches!(value, k::Expr::Variable(name) if self.module_helpers.contains(name));
        Ok(Some(if direct && !authored {
            constructor(
                "callable.invoke_token",
                std::iter::once(callee).chain(args).collect(),
            )
        } else {
            Expr::Call(Box::new(callee), args)
        }))
    }

    pub(super) fn act(&mut self, act: &k::ActExpr) -> Lift<Expr> {
        match &act.kind {
            k::ActKind::Perform {
                slot,
                then_yield: true,
                ..
            } => {
                let callee = self.operation(slot)?;
                let mut args = Vec::with_capacity(act.args.len());
                for arg in &act.args {
                    args.push(self.crossing(arg)?);
                }
                Ok(Expr::Unary(
                    "await ",
                    Box::new(Expr::Call(Box::new(callee), args)),
                ))
            }
            k::ActKind::Await(policy) if self.is(policy, "ts.await") => {
                let [value] = act.args.as_slice() else {
                    return refuse("an await of one value");
                };
                Ok(Expr::Unary("await ", Box::new(self.expr(value)?)))
            }
            k::ActKind::Call(k::Callee::Import(slot)) => Ok(Expr::Call(
                Box::new(self.operation(slot)?),
                act.args
                    .iter()
                    .map(|value| self.expr(value))
                    .collect::<Lift<_>>()?,
            )),
            k::ActKind::Call(k::Callee::Local(id)) => {
                let Some(name) = self.library(id) else {
                    return refuse("a call of a function the manifest does not list");
                };
                self.helper(&name, &act.args)
            }
            _ => refuse("an action the authored dialect does not lower to"),
        }
    }

    /// An argument crossing to a tool: the boundary copy is implicit.
    fn crossing(&mut self, arg: &k::Expr) -> Lift<Expr> {
        if let Some(value) = crate::constructs::conventions::CONVENTIONS
            .argument
            .unwrap(arg, |id, name| self.is(id, name))
        {
            return self.expr(value);
        }
        self.expr(arg)
    }

    /// The dotted path a call of `effect` is written as.
    fn operation(&self, slot: &lash_kernel_value::ImportSlot) -> Lift<Expr> {
        let name = &self
            .imports
            .get(slot)
            .ok_or_else(|| "missing operation import".to_owned())?
            .name;
        let operation = lash_kernel_value::QualifiedOperation {
            package: name.package.clone(),
            name: name.member.clone(),
        };
        let path = lash_kernel_dialect::spell_operation(
            lash_kernel_dialect::Spelling::TypeScript,
            &operation,
        );
        let mut segments = path.split('.');
        let Some(root) = segments.next().filter(|root| source::identifier(root)) else {
            return refuse("an operation whose root TypeScript cannot spell");
        };
        let mut callee = Expr::Ident(root.to_owned());
        for segment in segments {
            callee = Expr::Field(Box::new(callee), segment.to_owned());
        }
        Ok(callee)
    }

    /// A dialect helper's call, as the source operator or built-in it
    /// implements.
    fn helper(&mut self, name: &str, args: &[k::Expr]) -> Lift<Expr> {
        if let Some(value) = crate::constructs::numbers::BIGINT_INPUT.named(name, args) {
            return self.expr(value);
        }
        match (name, args) {
            ("ts.abort.start", [work, inputs, options]) => {
                let call = self.helper(
                    "ts.promise.start",
                    &[
                        work.clone(),
                        k::Expr::Literal(k::Literal::Absent),
                        inputs.clone(),
                    ],
                )?;
                let Expr::Call(callee, mut args) = call else {
                    return refuse("a direct tool promise");
                };
                args.push(self.expr(options)?);
                Ok(Expr::Call(callee, args))
            }
            ("ts.pow", [left, right]) => self.binary("**", left, right),
            // A slice over constant bounds; an omitted end is the whole length.
            (crate::constructs::methods::SLICE_UNITS, [receiver, start, end]) => {
                let receiver = self.expr(receiver)?;
                let mut bounds = vec![self.expr(start)?, self.expr(end)?];
                if bounds.last() == Some(&Expr::Undefined) {
                    bounds.pop();
                }
                Ok(Expr::Call(
                    Box::new(Expr::Field(
                        Box::new(receiver),
                        crate::constructs::methods::source_method(
                            crate::constructs::methods::Kind::Slice,
                        )
                        .into(),
                    )),
                    bounds,
                ))
            }
            (
                "ts.promise.start",
                [
                    k::Expr::Closure(work),
                    k::Expr::Literal(k::Literal::Absent),
                    k::Expr::List(inputs),
                ],
            ) => {
                let definition = self
                    .document
                    .functions
                    .get(&work.function)
                    .ok_or_else(|| "missing pending tool wrapper".to_owned())?;
                let [_, params] = definition.signature.params.as_slice() else {
                    return refuse("pending tool wrapper parameters");
                };
                let Some(body) = definition.body() else {
                    return refuse("pending tool wrapper body");
                };
                let [
                    k::Stmt::Return {
                        value: k::Expr::Act(performed),
                    },
                ] = body.block.as_slice()
                else {
                    return refuse("pending tool wrapper return");
                };
                let k::ActKind::Perform {
                    slot,
                    then_yield: false,
                    ..
                } = performed.kind
                else {
                    return refuse("pending tool wrapper perform");
                };
                let [k::Expr::Act(boundary)] = performed.args.as_slice() else {
                    return refuse("pending tool wrapper input");
                };
                if !matches!(&boundary.kind, k::ActKind::Call(k::Callee::Local(id)) if self.is(id, crate::constructs::conventions::ARGUMENT_COPY))
                {
                    return refuse("pending tool wrapper boundary");
                }
                let [k::Expr::Member(member), _factory] = boundary.args.as_slice() else {
                    return refuse("pending tool wrapper operand");
                };
                if !matches!(&**member, k::Member::Index { target: k::Expr::Variable(name), index: k::Expr::Literal(k::Literal::Float(index)) } if *name == params.name && index.get() == 0.0)
                    || inputs.len() != 1
                {
                    return refuse("pending tool wrapper argument position");
                }
                let callee = self.in_function(work.function).operation(&slot)?;
                let args = inputs
                    .iter()
                    .map(|input| self.expr(input))
                    .collect::<Lift<_>>()?;
                Ok(Expr::Call(Box::new(callee), args))
            }

            // A captured lexical read checks its TDZ cell before returning
            // the variable. The source read recreates that check on lowering.
            (
                "ts.tdz",
                [
                    k::Expr::Variable(name),
                    k::Expr::Literal(k::Literal::Text(label)),
                ],
            ) if name.as_str() == label => self.variable(name),
            (name, [left, right])
                if crate::constructs::operators::BINARY
                    .iter()
                    .any(|entry| entry.helper == name) =>
            {
                let declaration = crate::constructs::operators::BINARY
                    .iter()
                    .find(|entry| entry.helper == name)
                    .unwrap_or_else(|| unreachable!("declared helper"));
                if declaration.swapped {
                    self.binary(declaration.token, right, left)
                } else {
                    self.binary(declaration.token, left, right)
                }
            }
            (name, [value])
                if crate::constructs::operators::unary_inverse(name, false).is_some() =>
            {
                let (token, _) = crate::constructs::operators::unary_inverse(name, false)
                    .unwrap_or_else(|| unreachable!("a declared unary inverse"));
                Ok(Expr::Unary(token, Box::new(self.expr(value)?)))
            }
            (name, [target]) if name.starts_with("ts.property.") => {
                let property = &name["ts.property.".len()..];
                let target = self.expr(target)?;
                Ok(Expr::Field(Box::new(target), property.to_owned()))
            }
            ("ts.get_numeric" | "ts.get_computed", [target, key]) => {
                let target = self.expr(target)?;
                let key = self.expr(key)?;
                Ok(Expr::Index(Box::new(target), Box::new(key)))
            }
            ("ts.read", [target, k::Expr::Literal(k::Literal::Text(field))]) => {
                let target = self.expr(target)?;
                Ok(Expr::Field(Box::new(target), field.clone()))
            }
            (name, [receiver, k::Expr::List(rest)]) => {
                let Some((namespace, method)) = name
                    .strip_prefix("ts.")
                    .and_then(|name| name.split_once('.'))
                else {
                    return refuse(format!("`{name}` has no source spelling"));
                };
                let args = rest
                    .iter()
                    .map(|arg| self.expr(arg))
                    .collect::<Lift<Vec<_>>>()?;
                let constructor = lash_ext_ts_helpers::builtins::table()
                    .constructors
                    .iter()
                    .find(|(_, helper)| **helper == name)
                    .map(|(class, _)| *class)
                    .or_else(|| method.strip_prefix("new_"));
                if let Some(class) = constructor {
                    if !matches!(receiver, k::Expr::Literal(k::Literal::Absent)) {
                        return refuse("a constructor takes no receiver");
                    }
                    return Ok(Expr::New(Box::new(Expr::Ident(class.to_owned())), args));
                }
                let target = match (receiver, global(namespace)) {
                    (k::Expr::Literal(k::Literal::Absent), Some(global)) => {
                        Expr::Ident(global.to_owned())
                    }
                    (k::Expr::Literal(k::Literal::Absent), None) => {
                        return refuse(format!("`{name}` has no source spelling"));
                    }
                    (receiver, _) => self.expr(receiver)?,
                };
                Ok(Expr::Call(
                    Box::new(Expr::Field(Box::new(target), method.to_owned())),
                    args,
                ))
            }
            (name, _) => {
                let Some((path, _)) = lash_ext_ts_helpers::builtins::table()
                    .positional
                    .iter()
                    .find(|(_, declaration)| {
                        declaration.arity == args.len()
                            && (declaration.helper == name || declaration.float == Some(name))
                    })
                else {
                    return refuse(format!("`{name}` has no source spelling"));
                };
                let callee = builtin_path(path)
                    .ok_or_else(|| "a builtin path TypeScript cannot spell".to_owned())?;
                let args = args
                    .iter()
                    .map(|value| self.expr(value))
                    .collect::<Lift<_>>()?;
                Ok(Expr::Call(Box::new(callee), args))
            }
        }
    }
}

/// The global a static helper's namespace stands for.
/// A builtin token reads back as its owning path. Full lowering verification
/// checks the callable body and metadata rather than trusting just the tag.
fn builtin_value(parts: &[k::Expr]) -> Option<Expr> {
    let [
        k::Expr::Literal(k::Literal::Text(tag)),
        k::Expr::Literal(k::Literal::Text(path)),
        ..,
    ] = parts
    else {
        return None;
    };
    if tag != "ts.function"
        || lash_ext_ts_helpers::builtins::table()
            .builtin(path)
            .is_none()
    {
        return None;
    }
    builtin_path(path)
}

fn builtin_path(path: &str) -> Option<Expr> {
    let mut segments = path.split('.');
    let root = segments.next().filter(|root| source::identifier(root))?;
    let mut value = Expr::Ident(root.to_owned());
    for segment in segments {
        if !source::identifier(segment) {
            return None;
        }
        value = Expr::Field(Box::new(value), segment.to_owned());
    }
    Some(value)
}

fn global(namespace: &str) -> Option<&'static str> {
    let prefix = format!("ts.{namespace}.");
    let mut roots = lash_ext_ts_helpers::builtins::table()
        .functions
        .iter()
        .filter(|(_, helper)| helper.starts_with(&prefix))
        .filter_map(|(path, _)| (*path).split_once('.').map(|(root, _)| root));
    let root = roots.next()?;
    roots.all(|other| other == root).then_some(root)
}

pub(super) fn literal_of(literal: &k::Literal) -> Lift<Expr> {
    Ok(match literal {
        k::Literal::Null => Expr::Null,
        k::Literal::Absent => Expr::Undefined,
        k::Literal::Bool(value) => Expr::Bool(*value),
        k::Literal::Text(text) => Expr::Text(text.clone()),
        k::Literal::Int(value) => {
            constructor("integer.from_decimal", vec![Expr::Text(value.to_string())])
        }
        k::Literal::Bytes(value) => constructor("bytes.from_hex", vec![Expr::Text(value.to_hex())]),
        k::Literal::Float(value) => match source::number(value.get()) {
            Some(text) => Expr::Number(text),
            None => return refuse("a number no literal writes"),
        },
        _ => return refuse("a literal the authored dialect does not lower to"),
    })
}

fn constructor(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call(Box::new(Expr::Ident(name.into())), args)
}
