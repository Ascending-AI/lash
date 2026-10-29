//! Invert the Python callable ABI without printing its binder as user code.
use super::{BindingScope, Renderer, k};
use lash_kernel_value::Name;

use crate::constructs::callables::parts as token;
use crate::constructs::callables::{CALLABLE, CATEGORY, NAME, PARAMETERS};
fn category(parts: &[k::Expr]) -> Option<&str> {
    match &parts[CATEGORY] {
        k::Expr::Literal(k::Literal::Text(name)) => Some(name),
        k::Expr::Tuple(fields) => match fields.first()? {
            k::Expr::Literal(k::Literal::Text(name)) => Some(name),
            _ => None,
        },
        _ => None,
    }
}
fn mode(value: &k::Expr) -> Option<i64> {
    let k::Expr::Tuple(parts) = value else {
        return None;
    };
    let k::Expr::Literal(k::Literal::Int(value)) = &parts[1] else {
        return None;
    };
    (0..5).find(|n| value == &k::Integer::from(*n))
}
impl Renderer<'_> {
    /// Class tokens project their source spelling from their own metadata.
    pub(super) fn exception_classes(&self, value: &k::Expr) -> Option<String> {
        let value = self.resolved(value)?;
        if let k::Expr::Let { value, body, .. } = value
            && matches!(value.as_ref(), k::Expr::Closure(_))
        {
            return self.exception_classes(body);
        }
        if let Some(parts) = token(value)
            && category(parts) == Some("py.exception")
            && let k::Expr::Literal(k::Literal::Text(name)) = &parts[NAME]
            && super::identifier(name)
        {
            return Some(name.clone());
        }
        match value {
            k::Expr::Tuple(classes) => {
                let classes = classes
                    .iter()
                    .map(|class| self.exception_classes(class))
                    .collect::<Option<Vec<_>>>()?;
                Some(format!("({},)", classes.join(", ")))
            }
            k::Expr::Variable(name) if super::attribute_identifier(name.as_str()) => {
                Some(name.as_str().to_owned())
            }
            _ => None,
        }
    }

    /// The metadata literal, named local prologue and author body agree.
    fn packed_body(
        &self,
        closure: &k::Closure,
        parts: &[k::Expr],
    ) -> Option<(Vec<Name>, k::Block, lash_kernel_value::FunctionId, usize)> {
        let k::Expr::Tuple(metadata) = &parts[PARAMETERS] else {
            return None;
        };
        let definition = self.document.functions.get(&closure.function)?;
        if definition.signature.params.len() != 3 {
            return None;
        }
        let stored = &definition.body()?.block;
        let positional = &definition.signature.params[0].name;
        let keywords = &definition.signature.params[1].name;
        if let [
            k::Stmt::Return {
                value:
                    k::Expr::Let {
                        name: slots,
                        value,
                        body,
                    },
            },
        ] = stored.as_slice()
            && let k::Expr::If { cond, then, r#else } = value.as_ref()
            && then.as_ref() == &k::Expr::Variable(positional.clone())
            && let k::Expr::Act(binding) = r#else.as_ref()
            && let k::ActKind::Call(target) = &binding.kind
            && super::task::target(target)
                .is_some_and(|target| self.operators.is(&target, "py.bind"))
        {
            let k::Expr::Literal(k::Literal::Text(spelling)) = &parts[NAME] else {
                return None;
            };
            if !self.fixed_condition(cond, positional, keywords, metadata.len())
                || binding.args
                    != vec![
                        parts[NAME].clone(),
                        parts[PARAMETERS].clone(),
                        k::Expr::Variable(positional.clone()),
                        k::Expr::Variable(keywords.clone()),
                        k::Expr::Literal(k::Literal::Bool(false)),
                    ]
                || metadata
                    .iter()
                    .any(|param| !matches!(mode(param), Some(0 | 1)))
            {
                return None;
            }
            let (names, value) = self.packed_inputs(body, slots, metadata, spelling, true)?;
            return self.packed_source(
                names,
                vec![k::Stmt::Return {
                    value: value.clone(),
                }],
                closure.function,
                0,
            );
        }
        let mut expression_fallback = Vec::new();
        let (fast, stored) = match stored.first() {
            Some(k::Stmt::If {
                condition,
                then_block,
                else_block,
            }) if else_block.is_empty() => (Some((condition, then_block.clone())), &stored[1..]),
            Some(k::Stmt::Return {
                value: k::Expr::If { cond, then, r#else },
            }) if stored.len() == 1 => {
                expression_fallback.push(k::Stmt::Return {
                    value: *r#else.clone(),
                });
                (
                    Some((
                        cond.as_ref(),
                        vec![k::Stmt::Return {
                            value: *then.clone(),
                        }],
                    )),
                    expression_fallback.as_slice(),
                )
            }
            _ => (None, stored.as_slice()),
        };
        let expanded = lash_kernel_dialect::builder::expanded_return(stored).unwrap_or_default();
        let folded = expanded.len() > 1;
        let body = if folded { &expanded } else { stored };
        let k::Stmt::Let { name: slots, value } = body.first()? else {
            return None;
        };
        let value: k::Expr = match value {
            k::Rhs::Expr(v) => v.clone(),
            k::Rhs::Action(v) => v.clone().into(),
        };
        let k::Expr::Act(call) = value else {
            return None;
        };
        let k::ActKind::Call(callee) = &call.kind else {
            return None;
        };
        if !super::task::target(callee).is_some_and(|target| self.operators.is(&target, "py.bind"))
            || call.args.get(1) != Some(&parts[PARAMETERS])
        {
            return None;
        }
        let mut names = Vec::new();
        for (i, param) in metadata.iter().enumerate() {
            let k::Expr::Tuple(fields) = param else {
                return None;
            };
            let k::Expr::Literal(k::Literal::Text(spelled)) = &fields[0] else {
                return None;
            };
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Member(member)),
            } = body.get(i + 1)?
            else {
                return None;
            };
            if name.as_str() != spelled
                || !matches!(member.as_ref(), k::Member::Index { target: k::Expr::Variable(read), index: k::Expr::Literal(k::Literal::Int(index)) } if read == slots && index == &k::Integer::from(i64::try_from(i).ok()?))
            {
                return None;
            }
            names.push(name.clone());
        }
        let body = &body[metadata.len() + 1..];
        let mut authored = None;
        if let Some((condition, fast)) = fast.as_ref() {
            let (k::Stmt::Return { value }, prefix) = body.split_last()? else {
                return None;
            };
            let value = lash_kernel_dialect::builder::BlockBuilder::from_parts(
                prefix.to_vec(),
                vec![Default::default(); prefix.len()],
            )
            .expression(value.clone(), 0)?;
            let [k::Stmt::Return { value: fast }] = fast.as_slice() else {
                return None;
            };
            let k::Expr::Literal(k::Literal::Text(spelling)) = &parts[NAME] else {
                return None;
            };
            if !self.fixed_condition(condition, positional, keywords, metadata.len()) {
                return None;
            }
            let (fast_names, actual) = self.packed_inputs(
                fast,
                positional,
                metadata,
                spelling,
                call.args.get(4) == Some(&k::Expr::Literal(k::Literal::Bool(false))),
            )?;
            if fast_names != names {
                return None;
            }
            if actual != &value {
                return None;
            }
            authored = Some(vec![k::Stmt::Return { value }]);
        }
        let body = authored.as_deref().unwrap_or(body);
        self.packed_source(
            names,
            body.to_vec(),
            closure.function,
            if folded {
                0
            } else {
                metadata.len() + 1 + usize::from(fast.is_some())
            },
        )
    }

    fn packed_source(
        &self,
        names: Vec<Name>,
        body: k::Block,
        unit: lash_kernel_value::FunctionId,
        prologue: usize,
    ) -> Option<(Vec<Name>, k::Block, lash_kernel_value::FunctionId, usize)> {
        if let Some(function) = self.direct_body(&body) {
            let raw = self.document.functions.get(&function)?.body()?;
            Some((names, raw.block.clone(), function, 0))
        } else {
            Some((names, body, unit, prologue))
        }
    }

    fn packed_inputs<'a>(
        &self,
        mut value: &'a k::Expr,
        source: &Name,
        metadata: &[k::Expr],
        spelling: &str,
        required: bool,
    ) -> Option<(Vec<Name>, &'a k::Expr)> {
        let mut names = Vec::new();
        for (index, param) in metadata.iter().enumerate() {
            let k::Expr::Tuple(fields) = param else {
                return None;
            };
            let k::Expr::Literal(k::Literal::Text(spelled)) = fields.first()? else {
                return None;
            };
            let k::Expr::Let {
                name,
                value: read,
                body,
            } = value
            else {
                return None;
            };
            if name.as_str() != spelled
                || !matches!(read.as_ref(), k::Expr::Member(member) if matches!(member.as_ref(), k::Member::Index { target: k::Expr::Variable(target), index: k::Expr::Literal(k::Literal::Int(at)) } if target == source && at == &k::Integer::from(i64::try_from(index).ok()?)))
            {
                return None;
            }
            names.push(name.clone());
            value = body;
            if required && fields.get(2) == Some(&k::Expr::Literal(k::Literal::Bool(false))) {
                let k::Expr::If { cond, then, r#else } = value else {
                    return None;
                };
                if !matches!(cond.as_ref(), k::Expr::Call { function, args } if self.operators.is(function, "same") && args == &vec![k::Expr::Variable(name.clone()), k::Expr::Literal(k::Literal::Absent)])
                {
                    return None;
                }
                let k::Expr::Act(failure) = then.as_ref() else {
                    return None;
                };
                if !matches!(&failure.kind, k::ActKind::Call(target) if super::task::target(target).is_some_and(|target| self.operators.is(&target, "py.fail")))
                    || failure.args
                        != vec![
                            k::Expr::Literal(k::Literal::Text("TypeError".into())),
                            k::Expr::Literal(k::Literal::Text(format!(
                                "{spelling}() missing a required positional argument"
                            ))),
                        ]
                {
                    return None;
                }
                value = r#else;
            }
        }
        Some((names, value))
    }

    fn fixed_condition(
        &self,
        value: &k::Expr,
        positional: &Name,
        keywords: &Name,
        count: usize,
    ) -> bool {
        matches!(value, k::Expr::If { cond, then, r#else }
            if **r#else == k::Expr::Literal(k::Literal::Bool(false))
                && self.exact_count(cond, "list.len", positional, count)
                && self.exact_count(then, "map.len", keywords, 0))
    }

    fn exact_count(&self, value: &k::Expr, length: &str, parameter: &Name, count: usize) -> bool {
        matches!(value, k::Expr::Call { function, args } if self.operators.is(function, "same") && matches!(args.as_slice(), [k::Expr::Call { function, args }, k::Expr::Literal(k::Literal::Int(actual))] if self.operators.is(function, length) && args == &vec![k::Expr::Variable(parameter.clone())] && actual == &k::Integer::from(i64::try_from(count).unwrap_or(i64::MAX))))
    }

    pub(super) fn packed_definition(&mut self, statements: &[k::Stmt]) -> usize {
        let binding = |stmt: &k::Stmt| match stmt {
            k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(value),
                ..
            }
            | k::Stmt::Let {
                name,
                value: k::Rhs::Expr(value),
            } => Some((name.clone(), value.clone())),
            _ => None,
        };
        let Some((name, value)) = statements.first().and_then(binding) else {
            return 0;
        };
        let k::Expr::Let {
            name: temporary,
            value,
            body,
        } = &value
        else {
            return 0;
        };
        let k::Expr::Closure(closure) = value.as_ref() else {
            return 0;
        };
        let closure = (**closure).clone();
        let Some(parts) = token(body) else {
            return 0;
        };
        if parts[CALLABLE] != k::Expr::Variable(temporary.clone()) {
            return 0;
        }
        if category(parts) != Some("py.callable")
            || !matches!(&parts[NAME], k::Expr::Literal(k::Literal::Text(spelling)) if spelling == name.as_str())
        {
            return 0;
        }
        let Some((names, body, body_unit, prologue)) = self.packed_body(&closure, parts) else {
            return 0;
        };
        let asynchronous = matches!(&parts[CATEGORY], k::Expr::Tuple(fields) if fields.get(1) == Some(&k::Expr::Literal(k::Literal::Bool(true))));
        self.caption_next();
        self.padding();
        self.out.begin_node(None); // packed closure binding
        self.out.begin_node(None);
        self.out
            .write(if asynchronous { "async def " } else { "def " });
        self.name(&name);
        self.out.begin_node(None); // closure
        self.packed_parameters(parts, closure.presentation.as_ref());
        if let Some(annotation) = closure
            .presentation
            .as_ref()
            .and_then(|start| super::authored::annotation(&start.signature.result))
        {
            self.out.write(" -> ");
            self.out.write(&annotation);
        }
        self.out.write(":\n");
        if asynchronous {
            self.asynchronous.insert(name.clone());
        } else {
            self.sync.insert(name.clone());
        }
        let parent = std::mem::replace(&mut self.scope, BindingScope::new(&names));
        let main = self.main;
        self.main = false;
        self.out
            .begin_node(Some(&lash_kernel_value::sites::Unit::Library(body_unit)));
        self.indent += 1;
        for _ in 0..prologue {
            self.implicit();
        }
        self.nonlocals(&body, &names);
        if body.is_empty() {
            self.empty();
        } else {
            self.authored_statements(&body);
        }
        self.indent -= 1;
        self.out.end_node();
        self.main = main;
        self.scope = parent;
        self.out.end_node();
        self.implicit();
        self.out.end_node();
        self.out.end_node();
        1
    }

    fn packed_parameters(&mut self, parts: &[k::Expr], start: Option<&k::CallablePresentation>) {
        let k::Expr::Tuple(metadata) = &parts[PARAMETERS] else {
            return;
        };
        self.out.write("(");
        let mut first = true;
        let mut starred = false;
        for group in [0, 1, 3, 2, 4] {
            for (i, param) in metadata
                .iter()
                .enumerate()
                .filter(|(_, p)| mode(p) == Some(group))
            {
                if group == 2 && !starred {
                    if !first {
                        self.out.write(", ");
                    }
                    self.out.write("*");
                    first = false;
                    starred = true;
                }
                if !first {
                    self.out.write(", ");
                }
                first = false;
                let k::Expr::Tuple(fields) = param else {
                    continue;
                };
                let k::Expr::Literal(k::Literal::Text(name)) = &fields[0] else {
                    continue;
                };
                if group == 3 {
                    self.out.write("*");
                    starred = true;
                }
                if group == 4 {
                    self.out.write("**");
                }
                self.out.write(name);
                if let Some(annotation) = start
                    .and_then(|s| s.signature.params.get(i))
                    .and_then(|p| super::authored::annotation(&p.ty))
                {
                    self.out.write(": ");
                    self.out.write(&annotation);
                }
                // Defaults stay explicit until their binding has an exact
                // authored projection; refusing is preferable to changing it.
                if fields.get(2) == Some(&k::Expr::Literal(k::Literal::Bool(true))) {
                    self.out.refuse(super::PrintError::Invalid(
                        "defaulted callable needs its definition-site expression".into(),
                    ));
                }
            }
            if group == 0 && metadata.iter().any(|p| mode(p) == Some(0)) {
                self.out.write(", /");
            }
        }
        self.out.write(")");
    }

    pub(super) fn packed_value(&mut self, value: &k::Expr) -> bool {
        let Some(parts) = token(value) else {
            return false;
        };
        match category(parts) {
            Some("py.builtin" | "py.type" | "py.exception") => {
                if let k::Expr::Literal(k::Literal::Text(name)) = &parts[NAME] {
                    self.out.write(name);
                    return true;
                }
            }
            Some("py.callable") => {
                let Some(k::Expr::Closure(closure)) = self.resolved(&parts[CALLABLE]).cloned()
                else {
                    return false;
                };
                let Some((_, body, _, _)) = self.packed_body(&closure, parts) else {
                    return false;
                };
                let [k::Stmt::Return { value }] = body.as_slice() else {
                    return false;
                };
                self.out.write("(lambda ");
                let k::Expr::Tuple(metadata) = &parts[PARAMETERS] else {
                    return false;
                };
                for (i, param) in metadata.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    let k::Expr::Tuple(fields) = param else {
                        return false;
                    };
                    let k::Expr::Literal(k::Literal::Text(name)) = &fields[0] else {
                        return false;
                    };
                    self.out.write(name);
                }
                self.out.write(": ");
                self.inline_expr(value);
                self.out.write(")");
                return true;
            }
            _ => {}
        }
        false
    }

    /// A known token's projected packed closure still spells a source call.
    pub(super) fn projected_call(
        &mut self,
        callee: &k::Callee,
        args: &[k::Expr],
        awaited: bool,
    ) -> bool {
        let k::Callee::Value(name) = callee else {
            return false;
        };
        let value = k::Expr::Variable(name.clone());
        let Some(k::Expr::Member(member)) = self.resolved(&value).cloned() else {
            return false;
        };
        let k::Member::Index {
            target,
            index: k::Expr::Literal(k::Literal::Int(index)),
        } = *member
        else {
            return false;
        };
        if index != k::Integer::from(1) || args.len() != 3 {
            return false;
        }
        self.packed_call(&[target, args[0].clone(), args[1].clone()], awaited)
    }

    pub(super) fn packed_call(&mut self, args: &[k::Expr], awaited: bool) -> bool {
        let [callee, positional, keywords, ..] = args else {
            return false;
        };
        let Some(k::Expr::List(positional)) = self.resolved(positional).cloned() else {
            return false;
        };
        let Some(k::Expr::Map(keywords)) = self.resolved(keywords).cloned() else {
            return false;
        };
        if awaited
            && matches!(self.resolved(callee), Some(k::Expr::Variable(name)) if self.asynchronous.contains(name))
        {
            self.out.write("await ");
        }
        self.inline_expr(callee);
        self.out.write("(");
        for (i, value) in positional.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.inline_expr(value);
        }
        for (i, entry) in keywords.iter().enumerate() {
            if !positional.is_empty() || i > 0 {
                self.out.write(", ");
            }
            let k::Expr::Literal(k::Literal::Text(name)) = &entry.key else {
                return false;
            };
            self.out.write(name);
            self.out.write("=");
            self.inline_expr(&entry.value);
        }
        self.out.write(")");
        true
    }
}

impl Renderer<'_> {
    pub(super) fn scope_provider(&self, value: &k::Rhs) -> bool {
        let k::Rhs::Expr(k::Expr::Closure(closure)) = value else {
            return false;
        };
        if closure.presentation.is_some() {
            return false;
        }
        let Some(definition) = self.document.functions.get(&closure.function) else {
            return false;
        };
        if !definition.signature.params.is_empty() {
            return false;
        }
        let Some(body) = definition.body() else {
            return false;
        };
        let [
            k::Stmt::Return {
                value: k::Expr::Act(call),
            },
        ] = body.block.as_slice()
        else {
            return false;
        };
        let k::ActKind::Call(callee) = &call.kind else {
            return false;
        };
        super::task::target(callee).is_some_and(|target| self.operators.is(&target, "py.namespace"))
    }
}
