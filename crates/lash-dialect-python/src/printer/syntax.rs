//! Literal and mutation forms emitted by syntax lowering.
use super::{Renderer, k};

impl Renderer<'_> {
    pub(super) fn syntax_expression(&mut self, expr: &k::Expr) -> bool {
        if let k::Expr::Call { function, args } = expr
            && self.operators.is(function, "bool.not")
            && let [value] = args.as_slice()
            && let Some(k::Expr::Act(act)) = self.resolved(value).cloned()
            && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, "py.contains"))
            && let [container, value] = act.args.as_slice()
        {
            self.out.write("(");
            self.inline_expr(value);
            self.out.write(" not in ");
            self.inline_expr(container);
            self.out.write(")");
            return true;
        }
        match expr {
            k::Expr::Call { function, args }
                if self.operators.is(function, "text.join")
                    && matches!(args.as_slice(), [k::Expr::List(_), k::Expr::Literal(k::Literal::Text(s))] if s.is_empty()) =>
            {
                let k::Expr::List(parts) = &args[0] else {
                    return false;
                };
                self.out.write("f\"");
                for part in parts {
                    self.fstring_part(part, false);
                }
                self.out.write("\"");
                true
            }
            k::Expr::Call { function, args }
                if self.operators.is(function, "text.concat") && args.len() == 2 =>
            {
                self.out.write("f\"");
                for part in args {
                    self.fstring_part(part, false);
                }
                self.out.write("\"");
                true
            }
            k::Expr::Call { function, args }
                if self.operators.is(function, "num.to_text") && args.len() == 1 =>
            {
                self.out.write("f\"{");
                self.inline_expr(&args[0]);
                self.out.write("}\"");
                true
            }
            k::Expr::Act(act) => {
                let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
                    return false;
                };
                if self.operators.is(id, "py.contains")
                    && let [container, value] = act.args.as_slice()
                {
                    self.out.write("(");
                    self.inline_expr(value);
                    self.out.write(" in ");
                    self.inline_expr(container);
                    self.out.write(")");
                    return true;
                }
                if self.operators.is(id, "py.exc.new")
                    && let [k::Expr::Literal(k::Literal::Text(class)), arguments, _, _] =
                        act.args.as_slice()
                    && let Some(k::Expr::Tuple(arguments)) = self.resolved(arguments).cloned()
                {
                    self.out.write(class);
                    self.out.write("(");
                    for (index, argument) in arguments.iter().enumerate() {
                        if index > 0 {
                            self.out.write(", ");
                        }
                        self.inline_expr(argument);
                    }
                    self.out.write(")");
                    return true;
                }
                if self.operators.is(id, "py.ellipsis") && act.args.is_empty() {
                    self.out.write("...");
                    return true;
                }
                if ["py.str", "py.ascii", "py.format_spec"]
                    .iter()
                    .any(|name| self.operators.is(id, name))
                {
                    self.out.write("f\"");
                    self.fstring_part(expr, false);
                    self.out.write("\"");
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn fstring_part(&mut self, expr: &k::Expr, specification: bool) {
        let Some(value) = self.resolved(expr).cloned() else {
            return;
        };
        if let k::Expr::Literal(k::Literal::Text(text)) = &value {
            let quoted = serde_json::Value::String(text.clone()).to_string();
            let text = quoted[1..quoted.len() - 1]
                .replace('{', "{{")
                .replace('}', "}}");
            self.out.write(&text);
            return;
        }
        if specification
            && let k::Expr::Call { function, args } = &value
            && self.operators.is(function, "text.concat")
        {
            for arg in args {
                self.fstring_part(arg, true);
            }
            return;
        }
        self.out.write("{");
        if let k::Expr::Act(act) = &value
            && let k::ActKind::Call(k::Callee::Local(id)) = &act.kind
        {
            match act.args.as_slice() {
                [input]
                    if ["py.str", "py.repr", "py.ascii"]
                        .iter()
                        .any(|name| self.operators.is(id, name)) =>
                {
                    self.inline_expr(input);
                    if self.operators.is(id, "py.repr") {
                        self.out.write("!r");
                    }
                    if self.operators.is(id, "py.ascii") {
                        self.out.write("!a");
                    }
                }
                [input, spec, k::Expr::Literal(k::Literal::Text(conversion))]
                    if self.operators.is(id, "py.format_spec") =>
                {
                    self.inline_expr(input);
                    if !conversion.is_empty() {
                        self.out.write("!");
                        self.out.write(conversion);
                    }
                    self.out.write(":");
                    self.fstring_part(spec, true);
                }
                _ => self.inline_expr(&value),
            }
        } else if let k::Expr::Call { function, args } = &value
            && self.operators.is(function, "num.to_text")
            && args.len() == 1
        {
            self.inline_expr(&args[0]);
        } else {
            self.inline_expr(&value);
        }
        self.out.write("}");
    }

    pub(super) fn syntax_statement(&mut self, stmt: &k::Stmt) -> bool {
        if let k::Stmt::If {
            condition: k::Expr::Call { function, args },
            then_block,
            else_block,
        } = stmt
            && self.operators.is(function, "bool.not")
            && else_block.is_empty()
            && args.len() == 1
            && let [
                k::Stmt::Throw {
                    value: k::Expr::Act(act),
                },
            ] = then_block.as_slice()
            && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, "py.exc.new"))
            && let [
                k::Expr::Literal(k::Literal::Text(class)),
                k::Expr::Tuple(arguments),
                _,
                _,
            ] = act.args.as_slice()
            && class == "AssertionError"
        {
            self.out.write("assert ");
            if let k::Expr::Act(truth) = &args[0]
                && matches!(&truth.kind, k::ActKind::Call(k::Callee::Local(id)) if self.operators.is(id, "py.truth"))
                && let [value] = truth.args.as_slice()
            {
                self.inline_expr(value);
            } else {
                self.inline_expr(&args[0]);
            }
            if let Some(message) = arguments.first() {
                self.out.write(", ");
                self.inline_expr(message);
            }
            self.out.write("\n");
            return true;
        }
        let k::Stmt::Do {
            value: k::Expr::Act(act),
        } = stmt
        else {
            return false;
        };
        let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
            return false;
        };
        let deletion = self.operators.is(id, "py.delitem") || self.operators.is(id, "py.delslice");
        let slice = self.operators.is(id, "py.delslice") || self.operators.is(id, "py.setslice");
        if !deletion && !self.operators.is(id, "py.setslice") {
            return false;
        }
        let Some((target, args)) = act.args.split_first() else {
            return false;
        };
        if (slice && args.len() != if deletion { 3 } else { 4 }) || (!slice && args.len() != 1) {
            return false;
        }
        self.out.begin_node(None);
        if deletion {
            self.out.write("del ");
        }
        self.inline_expr(target);
        self.out.write("[");
        if slice {
            for (index, bound) in args[..3].iter().enumerate() {
                if index > 0 {
                    self.out.write(":");
                }
                if !matches!(bound, k::Expr::Literal(k::Literal::Null)) {
                    self.inline_expr(bound);
                } else {
                    self.implicit();
                }
            }
        } else {
            self.inline_expr(&args[0]);
        }
        self.out.write("]");
        if !deletion {
            self.out.write(" = ");
            self.inline_expr(&args[3]);
        }
        self.out.end_node();
        self.out.write("\n");
        true
    }
}

#[derive(Clone)]
pub(super) enum Display {
    List(Vec<(bool, k::Expr)>),
    Dict(Vec<(Option<k::Expr>, k::Expr)>),
}

impl Renderer<'_> {
    pub(super) fn lift_display(
        &self,
        statements: &[k::Stmt],
    ) -> Option<(lash_kernel_value::Name, Display, usize)> {
        let k::Stmt::Let {
            name,
            value: k::Rhs::Expr(value),
        } = statements.first()?
        else {
            return None;
        };
        let mut display = match value {
            k::Expr::List(items) if items.is_empty() => Display::List(Vec::new()),
            k::Expr::Map(items) if items.is_empty() => Display::Dict(Vec::new()),
            _ => return None,
        };
        let mut taken = 1;
        for statement in &statements[1..] {
            let k::Stmt::Do {
                value: k::Expr::Act(act),
            } = statement
            else {
                break;
            };
            let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
                break;
            };
            let Some((k::Expr::Variable(target), args)) = act.args.split_first() else {
                break;
            };
            if target != name {
                break;
            }
            match (&mut display, args) {
                (Display::List(items), [value]) if self.operators.is(id, "py.args.append") => {
                    items.push((false, value.clone()))
                }
                (Display::List(items), [value]) if self.operators.is(id, "py.args.extend") => {
                    items.push((true, value.clone()))
                }
                (Display::Dict(items), [key, value]) if self.operators.is(id, "py.setitem") => {
                    items.push((Some(key.clone()), value.clone()))
                }
                (Display::Dict(items), [value])
                    if (self.operators.is(id, "py.dict.extend")
                        || self.operators.is(id, "py.kw.extend")) =>
                {
                    items.push((None, value.clone()))
                }
                _ => break,
            }
            taken += 1;
        }
        (taken > 1).then_some((name.clone(), display, taken))
    }

    pub(super) fn display(&mut self, display: &Display, delimiters: (&str, &str)) {
        self.out.write(delimiters.0);
        match display {
            Display::List(items) => {
                for (index, (spread, value)) in items.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    if *spread {
                        self.out.write("*");
                    }
                    self.inline_expr(value);
                }
                if delimiters.0 == "(" {
                    self.out.write(",");
                }
            }
            Display::Dict(items) => {
                for (index, (key, value)) in items.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    match key {
                        Some(key) => {
                            self.inline_expr(key);
                            self.out.write(": ");
                        }
                        None => self.out.write("**"),
                    }
                    self.inline_expr(value);
                }
            }
        }
        self.out.write(delimiters.1);
    }

    pub(super) fn unpack(&mut self, statements: &[k::Stmt]) -> usize {
        let Some(k::Stmt::Let {
            name: temporary,
            value,
        }) = statements.first()
        else {
            return 0;
        };
        let value = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => action.clone().into(),
        };
        let k::Expr::Act(act) = value else {
            return 0;
        };
        let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
            return 0;
        };
        let (input, count, star) = match act.args.as_slice() {
            [input, k::Expr::Literal(k::Literal::Int(count))]
                if self.operators.is(id, "py.unpack") =>
            {
                (input, count, None)
            }
            [
                input,
                k::Expr::Literal(k::Literal::Int(count)),
                k::Expr::Literal(k::Literal::Int(star)),
            ] if self.operators.is(id, "py.unpack.star") => (input, count, Some(star)),
            _ => return 0,
        };
        let Some(count) = count
            .to_string()
            .parse::<usize>()
            .ok()
            .filter(|count| *count < statements.len())
        else {
            return 0;
        };
        let mut names = Vec::new();
        for (index, statement) in statements[1..=count].iter().enumerate() {
            let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Member(member)),
            } = statement
            else {
                return 0;
            };
            if !matches!(member.as_ref(), k::Member::Index { target: k::Expr::Variable(target), index: k::Expr::Literal(k::Literal::Int(at)) } if target == temporary && at == &k::Integer::from(index as i64))
            {
                return 0;
            }
            names.push(name);
        }
        self.padding();
        for (index, name) in names.into_iter().enumerate() {
            if index > 0 {
                self.out.write(", ");
            }
            if star == Some(&k::Integer::from(index as i64)) {
                self.out.write("*");
            }
            self.name(name);
            self.scope.bind(name, None);
        }
        if count == 1 {
            self.out.write(",");
        }
        self.out.write(" = ");
        self.inline_expr(input);
        self.out.write("\n");
        for _ in 0..=count {
            self.implicit();
        }
        count + 1
    }
}
