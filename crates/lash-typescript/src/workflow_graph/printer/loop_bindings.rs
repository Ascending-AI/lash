use super::*;

impl Printer<'_> {
    fn displayed_binding(&self, identity: &str) -> String {
        self.binding_names
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| scope.get(identity))
            .map_or_else(|| identity.to_string(), Clone::clone)
    }

    pub(super) fn binding_identifier(&self, context: &'static str, identity: &str) -> Printed {
        self.identifier(context, &self.displayed_binding(identity))
    }

    pub(super) fn loop_binding_name(
        &self,
        identity: &str,
        authored: Option<&str>,
        body: &Expr,
        bound: &[String],
    ) -> Printed {
        let Some(authored) = authored else {
            return self.binding_identifier("loop binding", identity);
        };
        self.identifier("loop binding", authored)?;
        let mut names = BTreeSet::new();
        collect_binding_names(body, &mut names);
        let mut free_names = BTreeSet::new();
        collect_free_binding_names(body, &BTreeSet::new(), &mut free_names);
        let other_names = free_names
            .iter()
            .filter(|name| name.as_str() != identity)
            .map(|name| self.displayed_binding(name))
            .collect::<BTreeSet<_>>();
        if !other_names.contains(authored) {
            return Ok(authored.to_string());
        }
        let mut occupied = names
            .iter()
            .map(|name| self.displayed_binding(name))
            .collect::<BTreeSet<_>>();
        occupied.extend(bound.iter().map(|name| self.displayed_binding(name)));
        for suffix in 1.. {
            let candidate = format!("{authored}_{suffix}");
            if !occupied.contains(&candidate) {
                return Ok(candidate);
            }
        }
        unreachable!("a finite body cannot occupy every numeric suffix")
    }
}

fn collect_binding_names(expression: &Expr, names: &mut BTreeSet<String>) {
    match expression {
        Expr::Variable(name) => {
            names.insert(name.to_string());
        }
        Expr::Assign { target, .. } => {
            names.insert(target.root.to_string());
        }
        Expr::FunctionCall { function, .. } => {
            names.insert(function.to_string());
        }
        Expr::Function(function) => {
            names.extend(function.params.iter().map(ToString::to_string));
            names.extend(function.name.iter().map(ToString::to_string));
        }
        Expr::For { binding, .. } => {
            names.insert(binding.to_string());
        }
        Expr::Try(try_expr) => {
            if let Some(catch) = &try_expr.catch {
                names.insert(catch.binding.to_string());
            }
        }
        _ => {}
    }
    for child in expression.children() {
        collect_binding_names(child, names);
    }
}

/// Names that would resolve through a newly printed loop binding. A nested
/// function, loop or catch owns its local names and can shadow it safely.
fn collect_free_binding_names(
    expression: &Expr,
    locals: &BTreeSet<String>,
    names: &mut BTreeSet<String>,
) {
    match expression {
        Expr::Function(function) => {
            let mut locals = locals.clone();
            locals.extend(function.params.iter().map(ToString::to_string));
            locals.extend(function.name.iter().map(ToString::to_string));
            collect_free_binding_names(&function.body, &locals, names);
        }
        Expr::For {
            binding,
            iterable,
            bind,
            body,
            ..
        } => {
            collect_free_binding_names(iterable, locals, names);
            let mut locals = locals.clone();
            locals.insert(binding.to_string());
            if let Some(bind) = bind {
                if let Ok(header) = loop_header(binding.as_str(), iterable, Some(bind)) {
                    locals.insert(header.binding.to_string());
                }
                collect_free_binding_names(bind, &locals, names);
            }
            collect_free_binding_names(body, &locals, names);
        }
        Expr::Try(try_expr) => {
            collect_free_binding_names(&try_expr.body, locals, names);
            if let Some(catch) = &try_expr.catch {
                let mut locals = locals.clone();
                locals.insert(catch.binding.to_string());
                collect_free_binding_names(&catch.body, &locals, names);
            }
            if let Some(finally) = &try_expr.finally {
                collect_free_binding_names(finally, locals, names);
            }
        }
        _ => {
            let name = match expression {
                Expr::Variable(name) => Some(name.as_str()),
                Expr::Assign { target, .. } => Some(target.root.as_str()),
                Expr::FunctionCall { function, .. } => Some(function.as_str()),
                _ => None,
            };
            if let Some(name) = name
                && !locals.contains(name)
            {
                names.insert(name.to_string());
            }
            for child in expression.children() {
                collect_free_binding_names(child, locals, names);
            }
        }
    }
}
