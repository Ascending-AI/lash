//! Free tool calls in Python lexical scopes, independent of lowering.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::{
    CellEnvironment, Diagnostic, Environment, Resolution, Spelling, ToolCalls,
};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::Ranged;

use crate::{Code, diagnostics, scope};

pub(crate) fn tool_calls(
    module: &ast::ModModule,
    environment: &Environment<'_>,
    cell: CellEnvironment<'_>,
    max_nesting: usize,
) -> Result<ToolCalls, Diagnostic> {
    let mut root = lexical_scope(&module.body, None);
    root.locals.extend(scope::rebound(&module.body).0);
    root.locals
        .extend(cell.bindings.iter().map(ToString::to_string));
    let imports = scope::imports(&module.body);
    let mut discovery = Discovery {
        environment,
        scopes: vec![root],
        imports,
        calls: BTreeSet::new(),
        error: None,
        nesting: 0,
        max_nesting,
    };
    discovery.visit_body(&module.body);
    if let Some(error) = discovery.error {
        return Err(error);
    }
    Ok(ToolCalls {
        calls: discovery.calls,
        namespace_collisions: environment
            .namespaces
            .collisions(cell.bindings, Spelling::Python),
    })
}

struct Scope {
    locals: BTreeSet<String>,
    globals: BTreeSet<String>,
}

fn lexical_scope(body: &[Stmt], parameters: Option<&ast::Parameters>) -> Scope {
    let (locals, globals) = scope::lexical_bindings(body, parameters);
    Scope { locals, globals }
}

struct Discovery<'a, 'e> {
    environment: &'a Environment<'e>,
    scopes: Vec<Scope>,
    imports: BTreeMap<String, Vec<String>>,
    calls: BTreeSet<String>,
    error: Option<Diagnostic>,
    nesting: usize,
    max_nesting: usize,
}

impl Discovery<'_, '_> {
    fn enter(&mut self, range: ruff_text_size::TextRange) -> bool {
        if self.error.is_some() {
            return false;
        }
        if self.nesting >= self.max_nesting {
            self.error = Some(diagnostics::with_repair(
                Code::TooDeep,
                format!(
                    "the program exceeds the configured {}-level source nesting limit",
                    self.max_nesting
                ),
                range,
                "split the expression or the block into named parts",
            ));
            return false;
        }
        self.nesting += 1;
        true
    }

    fn bound(&self, name: &str) -> bool {
        for scope in self.scopes.iter().skip(1).rev() {
            if scope.globals.contains(name) {
                break;
            }
            if scope.locals.contains(name) {
                return true;
            }
        }
        self.scopes[0].locals.contains(name)
    }

    fn path(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Name(name) => {
                let name = name.id.as_str();
                if self.bound(name) {
                    return None;
                }
                if let Some(path) = self.imports.get(name) {
                    return Some(path.join("."));
                }
                (!crate::lower::is_builtin(name) && name != "asyncio").then(|| name.to_owned())
            }
            Expr::Attribute(attribute) => Some(format!(
                "{}.{}",
                self.path(&attribute.value)?,
                attribute.attr.id
            )),
            Expr::Call(escape) => {
                let Expr::Name(getattr) = escape.func.as_ref() else {
                    return None;
                };
                if getattr.id.as_str() != "getattr"
                    || self.bound("getattr")
                    || !escape.arguments.keywords.is_empty()
                {
                    return None;
                }
                let [receiver, Expr::StringLiteral(name)] = &escape.arguments.args[..] else {
                    return None;
                };
                let path = self.path(receiver)?;
                if !path.contains('.') && !self.environment.namespaces.is_root(&path) {
                    return None;
                }
                Some(format!("{path}.{}", name.value.to_str()))
            }
            _ => None,
        }
    }

    fn call(&mut self, call: &ast::ExprCall) -> Result<(), Diagnostic> {
        let Some(path) = self.path(&call.func) else {
            return Ok(());
        };
        let segments = path.split('.').collect::<Vec<_>>();
        let Some((name, package)) = segments.split_last() else {
            return Ok(());
        };
        if package.is_empty()
            || !package
                .iter()
                .all(|segment| lash_kernel_value::Segment::is_canonical(segment))
            || lash_ext_py_helpers::pure_modules::installed(package[0])
            || package[0] == "asyncio"
            || path == "time.time"
            || lash_kernel_dialect::value_constructors::Constructor::named(&path).is_some()
            || matches!(
                self.environment
                    .namespaces
                    .resolve(&segments, Spelling::Python),
                Resolution::Export { .. } | Resolution::Namespace
            )
        {
            return Ok(());
        }
        if lash_kernel_value::Segment::is_canonical(name) {
            self.calls.insert(path);
        } else if let Some(canonical) =
            Spelling::TypeScript.unspell(lash_kernel_dialect::SpellKind::Member, name)
        {
            let repair = format!("{}.{canonical}", package.join("."));
            return Err(diagnostics::with_repair(
                Code::OperationSpelling,
                format!("`{path}` spells an operation name in another dialect's spelling"),
                call.func.range(),
                format!("write `{repair}`"),
            ));
        }
        Ok(())
    }

    fn comprehension(&mut self, generators: &[ast::Comprehension], values: &[&Expr]) {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        let mut locals = BTreeSet::new();
        fn target(expr: &Expr, locals: &mut BTreeSet<String>) {
            match expr {
                Expr::Name(name) => {
                    locals.insert(name.id.to_string());
                }
                Expr::Tuple(tuple) => tuple.elts.iter().for_each(|elt| target(elt, locals)),
                Expr::List(list) => list.elts.iter().for_each(|elt| target(elt, locals)),
                Expr::Starred(starred) => target(&starred.value, locals),
                _ => {}
            }
        }
        for generator in generators {
            target(&generator.target, &mut locals);
        }
        self.scopes.push(Scope {
            locals,
            globals: BTreeSet::new(),
        });
        self.visit_expr(&first.target);
        for condition in &first.ifs {
            self.visit_expr(condition);
        }
        for generator in rest {
            self.visit_comprehension(generator);
        }
        for value in values {
            self.visit_expr(value);
        }
        self.scopes.pop();
    }
}

impl<'a> Visitor<'a> for Discovery<'_, '_> {
    fn visit_annotation(&mut self, _: &'a Expr) {}

    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if !self.enter(statement.range()) {
            return;
        }
        if let Stmt::FunctionDef(def) = statement {
            for decorator in &def.decorator_list {
                self.visit_decorator(decorator);
            }
            self.visit_parameters(&def.parameters);
            self.scopes
                .push(lexical_scope(&def.body, Some(&def.parameters)));
            self.visit_body(&def.body);
            self.scopes.pop();
        } else if !matches!(statement, Stmt::TypeAlias(_)) {
            visitor::walk_stmt(self, statement);
        }
        self.nesting -= 1;
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        if !self.enter(expression.range()) {
            return;
        }
        match expression {
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_parameters(parameters);
                }
                let mut scope = lexical_scope(&[], lambda.parameters.as_deref());
                scope
                    .locals
                    .extend(scope::expression_bindings(&lambda.body));
                self.scopes.push(scope);
                self.visit_expr(&lambda.body);
                self.scopes.pop();
            }
            Expr::ListComp(comp) => self.comprehension(&comp.generators, &[&comp.elt]),
            Expr::SetComp(comp) => self.comprehension(&comp.generators, &[&comp.elt]),
            Expr::Generator(comp) => self.comprehension(&comp.generators, &[&comp.elt]),
            Expr::DictComp(comp) => {
                let mut values = Vec::new();
                if let Some(key) = &comp.key {
                    values.push(key.as_ref());
                }
                values.push(comp.value.as_ref());
                self.comprehension(&comp.generators, &values);
            }
            Expr::Call(call) => {
                if let Err(error) = self.call(call) {
                    self.error = Some(error);
                } else {
                    visitor::walk_expr(self, expression);
                }
            }
            _ => visitor::walk_expr(self, expression),
        }
        self.nesting -= 1;
    }
}
