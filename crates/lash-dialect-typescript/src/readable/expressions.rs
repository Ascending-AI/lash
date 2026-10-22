//! Expressions: operators from the library calls they lower to, waits from
//! their actions, and the scoped forms of `&&`, `||`, `??` and calls.

use lash_kernel_doc::{self as k};

use lash_vocabulary::Name;

use super::lift::{Lift, Lifter, refuse};
use super::source::{self, Expr, Part};

impl Lifter<'_> {
    pub(super) fn expr(&mut self, expr: &k::Expr) -> Lift<Expr> {
        match expr {
            k::Expr::Literal(literal) => literal_of(literal),
            k::Expr::Variable(name) => self.variable(name),
            k::Expr::If { cond, then, r#else } => {
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
                    Ok(Expr::Index(Box::new(target), Box::new(index)))
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

    fn native(&mut self, function: &lash_vocabulary::FunctionId, args: &[k::Expr]) -> Lift<Expr> {
        let Some(name) = self.library(function) else {
            return refuse("a call of a function the manifest does not list");
        };
        match (name.as_str(), args) {
            ("num.to_float", [value]) => self.expr(value),
            ("num.neg", [value]) => Ok(Expr::Unary("-", Box::new(self.expr(value)?))),
            ("bool.not", [value]) => Ok(Expr::Unary("!", Box::new(self.condition(value)?))),
            (
                "text.join",
                [
                    k::Expr::List(parts),
                    k::Expr::Literal(k::Literal::Text(separator)),
                ],
            ) if separator.is_empty() => self.template(parts),
            (name, [left, right]) => match binary_of(name) {
                Some(op) => self.binary(op, left, right),
                None => refuse(format!("`{name}` has no operator")),
            },
            (name, _) => refuse(format!("`{name}` has no source spelling")),
        }
    }

    /// `left op right`. Pinned operands the source evaluated right to left
    /// are a comparison written the other way round.
    fn binary(&mut self, op: &'static str, left: &k::Expr, right: &k::Expr) -> Lift<Expr> {
        if let (k::Expr::Variable(first), k::Expr::Variable(second)) = (left, right)
            && let (Some(first), Some(second)) = (self.pending_at(first), self.pending_at(second))
            && second < first
            && let Some(mirrored) = mirror(op)
        {
            let right = self.expr(right)?;
            let left = self.expr(left)?;
            return Ok(Expr::Binary(mirrored, Box::new(right), Box::new(left)));
        }
        let left = self.expr(left)?;
        let right = self.expr(right)?;
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
        let bound = k::Expr::Variable(name.clone());
        // `await sleep(ms)` gives undefined.
        if let k::Expr::Act(act) = value
            && let k::ActKind::Perform {
                slot,
                then_yield: true,
                ..
            } = act.kind
            && self.document.imports.get(&slot).is_some_and(|import| {
                import.name == lash_kernel_dialect::service_name("typescript", "sleep")
            })
            && let [duration] = act.args.as_slice()
            && matches!(body, k::Expr::Literal(k::Literal::Absent))
        {
            let duration = self.expr(duration)?;
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
        refuse("a scoped value the authored dialect does not lower to")
    }

    /// `if(same(name, null), true, same(name, absent))`.
    fn nullish(&self, test: &k::Expr, name: &Name) -> bool {
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
        let selects = match &**selected {
            k::Expr::Member(member) => matches!(&**member, k::Member::Index { target, index }
            if *target == held
                && *index == k::Expr::Literal(k::Literal::Float(k::Float::new(
                    f64::from(crate::FUNCTION_CALLABLE),
                )))),
            k::Expr::Call { function, args } => {
                self.is(function, "ts.callable") && args.as_slice() == [held.clone()]
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
        Ok(Some(Expr::Call(Box::new(callee), args)))
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
            k::ActKind::Call(k::Callee::Import(id)) => {
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
        if let k::Expr::Act(boundary) = arg
            && let k::ActKind::Call(k::Callee::Import(id)) = &boundary.kind
            && self.is(id, "ts.boundary")
            && let [value] = boundary.args.as_slice()
        {
            return self.expr(value);
        }
        self.expr(arg)
    }

    /// The dotted path a call of `effect` is written as.
    fn operation(&self, slot: &lash_vocabulary::ImportSlot) -> Lift<Expr> {
        let name = &self
            .document
            .imports
            .get(slot)
            .ok_or_else(|| "missing operation import".to_owned())?
            .name;
        let operation = lash_vocabulary::QualifiedOperation {
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
        match (name, args) {
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
                if !matches!(&boundary.kind, k::ActKind::Call(k::Callee::Import(id)) if self.is(id, "ts.boundary"))
                {
                    return refuse("pending tool wrapper boundary");
                }
                let [k::Expr::Member(member)] = boundary.args.as_slice() else {
                    return refuse("pending tool wrapper operand");
                };
                if !matches!(&**member, k::Member::Index { target: k::Expr::Variable(name), index: k::Expr::Literal(k::Literal::Float(index)) } if *name == params.name && index.get() == 0.0)
                    || inputs.len() != 1
                {
                    return refuse("pending tool wrapper argument position");
                }
                let callee = self.operation(&slot)?;
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
            ("ts.strict_equals", [left, right]) => self.binary("===", left, right),
            ("ts.strict_not_equals", [left, right]) => self.binary("!==", left, right),
            ("ts.not", [value]) => Ok(Expr::Unary("!", Box::new(self.expr(value)?))),
            ("ts.typeof", [value]) => Ok(Expr::Unary("typeof ", Box::new(self.expr(value)?))),
            (name, [target]) if name.starts_with("ts.property.") => {
                let property = &name["ts.property.".len()..];
                let target = self.expr(target)?;
                Ok(Expr::Field(Box::new(target), property.to_owned()))
            }
            ("ts.get_numeric", [target, key]) => {
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
                if let Some(class) = method.strip_prefix("new_") {
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
            (name, _) => refuse(format!("`{name}` has no source spelling")),
        }
    }
}

/// The global a static helper's namespace stands for.
fn global(namespace: &str) -> Option<&'static str> {
    Some(match namespace {
        "json" => "JSON",
        "math" => "Math",
        "object" => "Object",
        "array" => "Array",
        "number" => "Number",
        "string" => "String",
        "promise" => "Promise",
        _ => return None,
    })
}

fn binary_of(name: &str) -> Option<&'static str> {
    Some(match name {
        "num.add" | "ts.add" | "text.concat" => "+",
        "num.sub" | "ts.sub" => "-",
        "num.mul" | "ts.mul" => "*",
        "num.div" | "ts.div" => "/",
        "num.lt" | "ts.lt" => "<",
        "num.le" | "ts.le" => "<=",
        "ts.gt" => ">",
        "ts.ge" => ">=",
        "eq" => "===",
        _ => return None,
    })
}

/// The comparison `b op' a` that is `a op b`.
fn mirror(op: &str) -> Option<&'static str> {
    Some(match op {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        _ => return None,
    })
}

pub(super) fn literal_of(literal: &k::Literal) -> Lift<Expr> {
    Ok(match literal {
        k::Literal::Null => Expr::Null,
        k::Literal::Absent => Expr::Undefined,
        k::Literal::Bool(value) => Expr::Bool(*value),
        k::Literal::Text(text) => Expr::Text(text.clone()),
        k::Literal::Float(value) => match source::number(value.get()) {
            Some(text) => Expr::Number(text),
            None => return refuse("a number no literal writes"),
        },
        _ => return refuse("a literal the authored dialect does not lower to"),
    })
}
