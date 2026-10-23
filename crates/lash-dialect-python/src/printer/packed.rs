//! Invert the Python callable ABI without printing its binder as user code.
use super::{BindingScope, Renderer, k};
use lash_vocabulary::Name;

fn token(value: &k::Expr) -> Option<&[k::Expr]> {
    let k::Expr::Tuple(parts) = value else {
        return None;
    };
    (parts.len() == 5
        && matches!(&parts[0], k::Expr::Literal(k::Literal::Text(tag)) if tag == "py.function"))
    .then_some(parts)
}
fn category(parts: &[k::Expr]) -> Option<&str> {
    match &parts[4] {
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
    /// The metadata literal, named local prologue and author body agree.
    fn packed_body(
        &self,
        closure: &k::Closure,
        parts: &[k::Expr],
    ) -> Option<(Vec<Name>, k::Block)> {
        let k::Expr::Tuple(metadata) = &parts[2] else {
            return None;
        };
        let definition = self.document.functions.get(&closure.function)?;
        if definition.signature.params.len() != 3 {
            return None;
        }
        let body = &definition.body()?.block;
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
            || call.args.get(1) != Some(&parts[2])
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
        Some((names, body[metadata.len() + 1..].to_vec()))
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
        if parts[1] != k::Expr::Variable(temporary.clone()) {
            return 0;
        }
        if category(parts) != Some("py.callable") {
            return 0;
        }
        let Some((names, body)) = self.packed_body(&closure, parts) else {
            return 0;
        };
        let asynchronous = matches!(&parts[4], k::Expr::Tuple(fields) if fields.get(1) == Some(&k::Expr::Literal(k::Literal::Bool(true))));
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
        let parent = std::mem::replace(&mut self.scope, BindingScope::new(&names));
        let main = self.main;
        self.main = false;
        self.out
            .begin_node(Some(&lash_vocabulary::sites::Unit::Library(
                closure.function,
            )));
        self.indent += 1;
        for _ in 0..=names.len() {
            self.implicit();
        }
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
        if asynchronous {
            self.asynchronous.insert(name.clone());
        } else {
            self.sync.insert(name.clone());
        }
        1
    }

    fn packed_parameters(&mut self, parts: &[k::Expr], start: Option<&k::CallablePresentation>) {
        let k::Expr::Tuple(metadata) = &parts[2] else {
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
                if let k::Expr::Literal(k::Literal::Text(name)) = &parts[3] {
                    self.out.write(name);
                    return true;
                }
            }
            Some("py.callable") => {
                let Some(k::Expr::Closure(closure)) = self.resolved(&parts[1]).cloned() else {
                    return false;
                };
                let Some((_, body)) = self.packed_body(&closure, parts) else {
                    return false;
                };
                let [k::Stmt::Return { value }] = body.as_slice() else {
                    return false;
                };
                self.out.write("(lambda ");
                let k::Expr::Tuple(metadata) = &parts[2] else {
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
