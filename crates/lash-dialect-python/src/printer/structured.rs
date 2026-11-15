//! Invert statement regions whose syntax owns guards, temporaries and cleanup.
use super::{Renderer, k};
use lash_kernel_value::Name;

impl Renderer<'_> {
    fn syntax_call<'a>(&self, value: &'a k::Expr, helper: &str) -> Option<&'a [k::Expr]> {
        match value {
            k::Expr::Call { function, args } if self.operators.is(function, helper) => Some(args),
            k::Expr::Act(act) if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, helper)) => {
                Some(&act.args)
            }
            _ => None,
        }
    }

    pub(super) fn structured_statements(&mut self, statements: &[k::Stmt]) -> usize {
        let mut statements = statements.to_vec();
        let mut pending = vec![k::NodeMut::Block(&mut statements)];
        while let Some(mut node) = pending.pop() {
            if let k::NodeMut::Stmt(k::Stmt::Let { value, .. } | k::Stmt::Assign { value, .. }) =
                &mut node
                && let k::Rhs::Action(action) = value
            {
                *value = k::Rhs::Expr(action.clone().into());
            }
            pending.extend(node.children());
        }
        self.match_statements(&statements)
            .or_else(|| self.range_statements(&statements))
            .or_else(|| self.with_statements(&statements))
            .unwrap_or(0)
    }

    fn range_statements(&mut self, statements: &[k::Stmt]) -> Option<usize> {
        let guard = statements.iter().take(4).position(|statement| {
            let k::Stmt::If { condition, then_block, .. } = statement else { return false; };
            self.range_bound(condition).is_some() && matches!(then_block.first(), Some(k::Stmt::Do { value }) if self.syntax_call(value, "py.fail").is_some())
        })?;
        let [first, second, third, _, tail @ ..] = &statements[guard..] else {
            return None;
        };
        let bounds: Vec<_> = [first, second, third]
            .into_iter()
            .map(|statement| {
                let k::Stmt::If { condition, .. } = statement else {
                    return None;
                };
                self.range_bound(condition).cloned()
            })
            .collect::<Option<_>>()?;
        let while_at = tail
            .iter()
            .take(6)
            .position(|statement| matches!(statement, k::Stmt::While { .. }))?;
        let k::Stmt::While {
            body, else_block, ..
        } = &tail[while_at]
        else {
            return None;
        };
        let [
            k::Stmt::Assign {
                place: k::Place::Variable(binding),
                ..
            },
            k::Stmt::Assign { .. },
            body @ ..,
        ] = body.as_slice()
        else {
            return None;
        };
        let saved = self.substitutions.clone();
        for statement in &statements[..guard] {
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            } = statement
            else {
                return None;
            };
            if !super::authored::temporary_name(name) {
                return None;
            }
            self.substitutions.insert(name.clone(), value.clone());
        }
        let zero = k::Expr::Literal(k::Literal::Int(0.into()));
        let one = k::Expr::Literal(k::Literal::Int(1.into()));
        let bounds = if bounds[2] == one {
            if bounds[0] == zero {
                &bounds[1..2]
            } else {
                &bounds[..2]
            }
        } else {
            bounds.as_slice()
        };
        self.padding();
        self.out.write("for ");
        self.name(binding);
        self.scope.introduce(binding);
        self.out.write(" in range(");
        for (index, bound) in bounds.iter().enumerate() {
            if index > 0 {
                self.out.write(", ");
            }
            self.inline_expr(bound);
        }
        self.out.write("):\n");
        self.readable_block(&body.to_vec());
        if !else_block.is_empty() {
            self.padding();
            self.out.write("else:\n");
            self.readable_block(else_block);
        }
        self.substitutions = saved;
        Some(guard + 4 + while_at + 1)
    }

    fn range_bound<'a>(&self, condition: &'a k::Expr) -> Option<&'a k::Expr> {
        let [test] = self.syntax_call(condition, "bool.not")? else {
            return None;
        };
        let [kind, k::Expr::Literal(k::Literal::Text(integer))] = self.syntax_call(test, "same")?
        else {
            return None;
        };
        if integer != "integer" {
            return None;
        }
        let [bound] = self.syntax_call(kind, "kind")? else {
            return None;
        };
        Some(bound)
    }

    fn with_statements(&mut self, statements: &[k::Stmt]) -> Option<usize> {
        let k::Stmt::Let {
            name: manager,
            value: k::Rhs::Expr(value),
        } = statements.first()?
        else {
            return None;
        };
        let mut enters = None;
        let mut exits = None;
        let mut entered = None;
        let mut asynchronous = false;
        for (index, statement) in statements.iter().enumerate().skip(1).take(18) {
            if let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(value),
                ..
            } = statement
            {
                if let Some(
                    [
                        k::Expr::Variable(target),
                        k::Expr::Literal(k::Literal::Text(member)),
                    ],
                ) = self.syntax_call(value, "py.context_member")
                {
                    if target != manager {
                        return None;
                    }
                    match member.as_str() {
                        "__enter__" => enters = Some(name),
                        "__exit__" => exits = Some(name),
                        "__aenter__" => {
                            enters = Some(name);
                            asynchronous = true;
                        }
                        "__aexit__" => {
                            exits = Some(name);
                            asynchronous = true;
                        }
                        _ => return None,
                    }
                }
                if let Some([k::Expr::Variable(callee), ..]) = self.syntax_call(value, "py.call")
                    && Some(callee) == enters
                {
                    entered = Some(name);
                }
            }
            if let k::Stmt::Try(scope) = statement {
                if exits.is_none()
                    || entered.is_none()
                    || scope.catch.is_none()
                    || scope.finally.is_none()
                {
                    return None;
                }
                let (binding, body) = match scope.body.first() {
                    Some(k::Stmt::Assign {
                        place: k::Place::Variable(binding),
                        value: k::Rhs::Expr(k::Expr::Variable(input)),
                        ..
                    }) if Some(input) == entered => (Some(binding), &scope.body[1..]),
                    _ => (None, scope.body.as_slice()),
                };
                self.padding();
                self.out
                    .write(if asynchronous { "async with " } else { "with " });
                self.inline_expr(value);
                if let Some(binding) = binding {
                    self.out.write(" as ");
                    self.name(binding);
                    self.scope.introduce(binding);
                }
                self.out.write(":\n");
                self.readable_block(&body.to_vec());
                return Some(index + 1);
            }
        }
        None
    }
}

#[derive(Clone)]
enum Pattern {
    Wildcard,
    Capture(Name),
    Value(k::Expr),
    Sequence(Vec<(bool, Pattern)>),
    Mapping(Vec<(k::Expr, Pattern)>, Option<Name>),
}

impl Renderer<'_> {
    fn match_statements(&mut self, statements: &[k::Stmt]) -> Option<usize> {
        let [
            k::Stmt::Let {
                name: subject,
                value: k::Rhs::Expr(value),
            },
            k::Stmt::Let {
                name: done,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
            },
            tail @ ..,
        ] = statements
        else {
            return None;
        };
        let mut taken = 2;
        let mut cases = Vec::new();
        let mut rest = tail;
        loop {
            let captures = rest
                .iter()
                .take_while(|statement| {
                    matches!(
                        statement,
                        k::Stmt::Let {
                            value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
                            ..
                        }
                    )
                })
                .count();
            let Some(k::Stmt::If {
                condition,
                then_block: attempt,
                else_block,
            }) = rest.get(captures)
            else {
                break;
            };
            if !else_block.is_empty()
                || self.syntax_call(condition, "bool.not")? != [k::Expr::Variable(done.clone())]
            {
                break;
            }
            let [
                k::Stmt::Let { name: ok, .. },
                k::Stmt::If {
                    then_block: pattern,
                    ..
                },
                k::Stmt::If {
                    then_block: matched,
                    ..
                },
            ] = attempt.as_slice()
            else {
                return None;
            };
            let mut bindings = std::collections::BTreeMap::new();
            let mut offset = 0;
            for statement in matched {
                let k::Stmt::Assign {
                    place: k::Place::Variable(binding),
                    value: k::Rhs::Expr(k::Expr::Variable(slot)),
                    ..
                } = statement
                else {
                    break;
                };
                bindings.insert(slot.clone(), binding.clone());
                offset += 1;
            }
            let pattern =
                self.match_pattern(pattern, &k::Expr::Variable(subject.clone()), ok, &bindings)?;
            let body = matched.get(offset..)?;
            let (guard, body) = match body {
                [
                    k::Stmt::If {
                        condition,
                        then_block,
                        else_block,
                    },
                ] if else_block.is_empty() => (Some(condition), then_block.as_slice()),
                body => (None, body),
            };
            let [
                k::Stmt::Assign {
                    place: k::Place::Variable(completed),
                    ..
                },
                body @ ..,
            ] = body
            else {
                return None;
            };
            if completed != done {
                return None;
            }
            cases.push((pattern, guard, body));
            let count = captures + 1;
            taken += count;
            rest = &rest[count..];
        }
        if cases.is_empty() {
            return None;
        }
        self.padding();
        self.out.write("match ");
        self.inline_expr(value);
        self.out.write(":\n");
        self.indent += 1;
        for (pattern, guard, body) in cases {
            self.padding();
            self.out.write("case ");
            self.match_pattern_source(&pattern);
            if let Some(guard) = guard {
                self.out.write(" if ");
                self.inline_expr(guard);
            }
            self.out.write(":\n");
            self.readable_block(&body.to_vec());
        }
        self.indent -= 1;
        Some(taken)
    }

    fn match_pattern(
        &self,
        block: &[k::Stmt],
        value: &k::Expr,
        ok: &Name,
        bindings: &std::collections::BTreeMap<Name, Name>,
    ) -> Option<Pattern> {
        if let [
            k::Stmt::If {
                condition: k::Expr::Variable(test),
                then_block,
                else_block,
            },
        ] = block
            && test == ok
            && else_block.is_empty()
        {
            return self.match_pattern(then_block, value, ok, bindings);
        }
        if let [
            k::Stmt::Assign {
                place: k::Place::Variable(slot),
                value: k::Rhs::Expr(read),
                ..
            },
        ] = block
            && read == value
            && let Some(binding) = bindings.get(slot)
        {
            return Some(Pattern::Capture(binding.clone()));
        }
        if block.is_empty() {
            return Some(Pattern::Wildcard);
        }
        let Some(k::Stmt::Assign {
            value: k::Rhs::Expr(test),
            ..
        }) = block.first()
        else {
            return None;
        };
        if let Some([input, _count, _star]) = self.syntax_call(test, "py.match_sequence") {
            if input != value {
                return None;
            }
            let mut items = Vec::new();
            for statement in &block[1..] {
                let k::Stmt::If { then_block, .. } = statement else {
                    return None;
                };
                let [
                    k::Stmt::Let {
                        name,
                        value: k::Rhs::Expr(item),
                    },
                    nested @ ..,
                ] = then_block.as_slice()
                else {
                    return None;
                };
                let star = self.syntax_call(item, "py.to_list").is_some();
                items.push((
                    star,
                    self.match_pattern(nested, &k::Expr::Variable(name.clone()), ok, bindings)?,
                ));
            }
            return Some(Pattern::Sequence(items));
        }
        if self.syntax_call(test, "py.match_mapping") == Some(std::slice::from_ref(value)) {
            let mut fields = Vec::new();
            let mut rest = None;
            for statement in &block[2..] {
                let k::Stmt::If { then_block, .. } = statement else {
                    return None;
                };
                if let [
                    k::Stmt::Assign {
                        place: k::Place::Variable(slot),
                        value: k::Rhs::Expr(value),
                        ..
                    },
                ] = then_block.as_slice()
                    && self.syntax_call(value, "py.match_rest").is_some()
                {
                    rest = Some(bindings.get(slot)?.clone());
                    continue;
                }
                let Some(k::Stmt::If {
                    then_block: read, ..
                }) = then_block.last()
                else {
                    return None;
                };
                let [
                    k::Stmt::Let {
                        name,
                        value: k::Rhs::Expr(item),
                    },
                    nested @ ..,
                ] = read.as_slice()
                else {
                    return None;
                };
                let [_, key] = self.syntax_call(item, "py.getitem")? else {
                    return None;
                };
                fields.push((
                    key.clone(),
                    self.match_pattern(nested, &k::Expr::Variable(name.clone()), ok, bindings)?,
                ));
            }
            return Some(Pattern::Mapping(fields, rest));
        }
        if let Some([input, expected]) = self
            .syntax_call(test, "eq")
            .or_else(|| self.syntax_call(test, "same"))
            && input == value
        {
            return Some(Pattern::Value(expected.clone()));
        }
        None
    }

    fn match_pattern_source(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard => self.out.write("_"),
            Pattern::Capture(name) => {
                self.name(name);
                self.scope.introduce(name);
            }
            Pattern::Value(value) => self.inline_expr(value),
            Pattern::Sequence(items) => {
                self.out.write("[");
                for (index, (star, pattern)) in items.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    if *star {
                        self.out.write("*");
                    }
                    self.match_pattern_source(pattern);
                }
                self.out.write("]");
            }
            Pattern::Mapping(fields, rest) => {
                self.out.write("{");
                for (index, (key, pattern)) in fields.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    self.inline_expr(key);
                    self.out.write(": ");
                    self.match_pattern_source(pattern);
                }
                if let Some(rest) = rest {
                    if !fields.is_empty() {
                        self.out.write(", ");
                    }
                    self.out.write("**");
                    self.name(rest);
                    self.scope.introduce(rest);
                }
                self.out.write("}");
            }
        }
    }
}
