//! Function values lifted back to the arrows and nested functions that
//! lower to them: the function token the front end makes of each, its
//! arguments read from the call's list, and an async one's body started as
//! a promise.

use lash_kernel_doc::{self as k};

use lash_kernel_value::Name;
use lash_kernel_value::sites::Site;

use super::lift::{Lift, Lifter, refuse};
use super::source::{Body, Expr, Function, Param, Type};

/// The token a TypeScript function value is: its tag, its own `this`
/// binding, its name, its arity, its callable and its properties.
const TOKEN: &str = "ts.function";

pub(super) fn authored_name(value: &k::Expr) -> Option<&str> {
    let k::Expr::Tuple(parts) = value else {
        return None;
    };
    let [
        k::Expr::Literal(k::Literal::Text(tag)),
        _,
        k::Expr::Literal(k::Literal::Text(name)),
        _,
        k::Expr::Closure(_),
        _,
    ] = parts.as_slice()
    else {
        return None;
    };
    (tag == TOKEN).then_some(name.as_str())
}

impl Lifter<'_> {
    /// Recover a captured module function's body form. Block bodies can
    /// come from either a declaration or a const-bound function; read-back
    /// selects the declaration form that recreates the capture.
    pub(super) fn module_body(
        &mut self,
        name: &Name,
        body: &[super::source::Stmt],
    ) -> Lift<Option<Body>> {
        for definition in self.document.functions.values() {
            let Some(block) = definition.body() else {
                continue;
            };
            for statement in &block.block {
                let k::Stmt::Let {
                    value: k::Rhs::Expr(value),
                    ..
                } = statement
                else {
                    continue;
                };
                if authored_name(value) != Some(name.as_str()) {
                    continue;
                }
                if let Some(function) = self.closure(
                    value,
                    &Site::new(lash_kernel_value::sites::Unit::Main, Vec::new()),
                )? {
                    if matches!(function.body, Body::Expr(_))
                        && let [
                            super::source::Stmt {
                                kind: super::source::StmtKind::Return(Some(value)),
                                ..
                            },
                        ] = body
                    {
                        return Ok(Some(Body::Expr(value.clone())));
                    }
                    return Ok(Some(Body::Block(body.to_vec())));
                }
            }
        }
        Ok(None)
    }

    /// The function `value`, at `site`, is the token of, when it is one.
    pub(super) fn closure(&mut self, value: &k::Expr, _site: &Site) -> Lift<Option<Function>> {
        let k::Expr::Tuple(parts) = value else {
            return Ok(None);
        };
        let [
            k::Expr::Literal(k::Literal::Text(tag)),
            k::Expr::Literal(k::Literal::Text(bound)),
            k::Expr::Literal(k::Literal::Text(_)),
            k::Expr::Literal(k::Literal::Float(declared)),
            k::Expr::Closure(closure),
            properties,
        ] = parts.as_slice()
        else {
            return Ok(None);
        };
        if tag != TOKEN {
            return Ok(None);
        }
        let bare = match properties {
            k::Expr::Map(entries) => entries.is_empty(),
            k::Expr::Record(entries) => entries.is_empty(),
            _ => false,
        };
        if !bound.is_empty() || !bare {
            return refuse("a function value with a bound `this` or properties");
        }
        let Some(start) = &closure.presentation else {
            return refuse("a function value no host can start");
        };
        if !matches!(&start.convention, k::StartConvention::TsArgsList { await_fn, same_fn }
            if self.is(await_fn, "ts.entry_result") && self.is(same_fn, "same"))
        {
            return refuse("a function value started another way");
        }
        let arity = start.signature.params.len();
        if declared.get() != arity as f64 {
            return refuse("a function value whose arity is not its parameters'");
        }
        let definition = self
            .document
            .functions
            .get(&closure.function)
            .ok_or_else(|| "missing closure body".to_owned())?;
        let params = definition
            .signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect::<Vec<_>>();
        let [_, args] = params.as_slice() else {
            return refuse("a function value's callable takes `this` and its arguments");
        };
        let mut params = Vec::with_capacity(arity);
        for param in &start.signature.params {
            params.push(Param {
                caption: None,
                name: self.binding(&param.name)?,
                optional: param.optional,
                ty: self.callable_annotation(&param.ty)?,
            });
        }
        // The callable's body, at its site: the token's fifth part.
        let site = Site::new(
            lash_kernel_value::sites::Unit::Library(closure.function),
            Vec::new(),
        );
        let wrapper = self.in_function(closure.function);
        let (is_async, args, body, site) = match definition
            .body()
            .ok_or_else(|| "closure has no body".to_owned())?
            .block
            .as_slice()
        {
            [
                k::Stmt::Return {
                    value: k::Expr::Act(act),
                },
            ] if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id))
                if wrapper.is(id, "ts.promise.start")) =>
            {
                let [
                    k::Expr::Closure(inner),
                    k::Expr::Variable(_),
                    k::Expr::Call {
                        function,
                        args: copied,
                    },
                ] = act.args.as_slice()
                else {
                    return refuse("an async function's start");
                };
                if !wrapper.is(function, "list.copy")
                    || copied.as_slice() != [k::Expr::Variable(args.clone())]
                {
                    return refuse("an async function's start");
                }
                let inner_definition = self
                    .document
                    .functions
                    .get(&inner.function)
                    .ok_or_else(|| "missing async body".to_owned())?;
                let inner_params = &inner_definition.signature.params;
                let [_, inner_param] = inner_params.as_slice() else {
                    return refuse("an async function's body takes `this` and its arguments");
                };
                (
                    true,
                    &inner_param.name,
                    inner_definition
                        .body()
                        .ok_or_else(|| "missing async body".to_owned())?
                        .block
                        .as_slice(),
                    Site::new(
                        lash_kernel_value::sites::Unit::Library(inner.function),
                        Vec::new(),
                    ),
                )
            }
            body => (false, args, body, site),
        };
        let names: Vec<&Name> = start
            .signature
            .params
            .iter()
            .map(|param| &param.name)
            .collect();
        let lash_kernel_value::sites::Unit::Library(body_function) = &site.unit else {
            return refuse("a function body outside its defining unit");
        };
        let mut inner = self.in_function(*body_function);
        let body = inner.function_body(is_async, args, &names, body, &site);
        Ok(Some(Function {
            is_async,
            result: self.callable_annotation(&start.signature.result)?,
            params,
            body: body?,
        }))
    }

    /// The body of a function value taking `names` from its argument list
    /// `args`: an arrow's expression, or its statements.
    fn function_body(
        &mut self,
        is_async: bool,
        args: &Name,
        names: &[&Name],
        body: &[k::Stmt],
        site: &Site,
    ) -> Lift<Body> {
        let adopts =
            |lifter: &Self, policy: &k::FunctionTarget| lifter.is(policy, "ts.promise.adopt");
        // An arrow's expression: its arguments bound around it.
        let returned = match body {
            [k::Stmt::Return { value }] if !is_async => Some(value),
            [k::Stmt::ReturnVia { value, policy }] if is_async && adopts(self, policy) => {
                Some(value)
            }
            _ => None,
        };
        if let Some(value) = returned
            && self.site_caption(&site.child(0)).is_none()
        {
            if names.is_empty() {
                return Ok(Body::Expr(self.expr(value)?));
            }
            if let Some(value) = arguments_around(self, args, names, value) {
                return Ok(Body::Expr(self.expr(value)?));
            }
        }
        // Its statements, after those that bind its arguments.
        let skip = if names.is_empty() {
            0
        } else {
            arguments_before(self, args, names, body)
                .ok_or_else(|| "a function value's arguments are bound another way".to_owned())?
        };
        let mut statements = &body[skip..];
        if let [rest @ .., last] = statements {
            let implicit = match last {
                k::Stmt::ReturnVia { value, policy } => {
                    is_async && adopts(self, policy) && *value == absent()
                }
                k::Stmt::Return { value } => !is_async && *value == absent(),
                _ => false,
            };
            if implicit
                && self
                    .site_caption(&site.child((body.len() - 1) as u32))
                    .is_none()
            {
                statements = rest;
            }
        }
        Ok(Body::Block(self.block_at(statements, site, skip)?))
    }
}

fn absent() -> k::Expr {
    k::Expr::Literal(k::Literal::Absent)
}

/// `if(num.lt(list.len(args), n), invoke ts.pad(args, n), args)`: the
/// argument list, padded to the function's arity.
fn padded(lifter: &Lifter<'_>, args: &Name, arity: usize, value: &k::Expr) -> bool {
    let list = k::Expr::Variable(args.clone());
    let n = k::Expr::Literal(k::Literal::Float(k::Float::new(arity as f64)));
    let k::Expr::If { cond, then, r#else } = value else {
        return false;
    };
    let tests = matches!(&**cond, k::Expr::Call { function, args: operands }
        if lifter.is(function, "num.lt")
            && matches!(operands.as_slice(), [k::Expr::Call { function: len, args: of }, bound]
                if lifter.is(len, "list.len") && of.as_slice() == [list.clone()] && *bound == n));
    let pads = matches!(&**then, k::Expr::Act(act)
        if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if lifter.is(id, "ts.pad"))
            && act.args.as_slice() == [list.clone(), n.clone()]);
    tests && pads && **r#else == list
}

/// `args'[index]` of the padded list `padded`.
fn argument(lifter: &Lifter<'_>, padded: &Name, index: usize, value: &k::Expr) -> bool {
    let value = match value {
        k::Expr::Act(act) if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if lifter.is(id, "ts.bigint.input")) =>
        {
            let [value, _shape] = act.args.as_slice() else {
                return false;
            };
            value
        }
        _ => value,
    };
    matches!(value, k::Expr::Member(member) if matches!(&**member, k::Member::Index { target, index: at }
        if *target == k::Expr::Variable(padded.clone())
            && *at == k::Expr::Literal(k::Literal::Float(k::Float::new(index as f64)))))
}

fn bigint_binding(lifter: &Lifter<'_>, value: &k::Expr, name: &Name) -> bool {
    matches!(value, k::Expr::Act(act)
        if matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if lifter.is(id, "ts.bigint.input"))
            && matches!(act.args.first(), Some(k::Expr::Variable(value)) if value == name))
}

/// The body `value` binds `names` around, from the padded `args`.
fn arguments_around<'v>(
    lifter: &Lifter<'_>,
    args: &Name,
    names: &[&Name],
    value: &'v k::Expr,
) -> Option<&'v k::Expr> {
    let mut value = value;
    while let k::Expr::Act(act) = value
        && matches!(&act.kind, k::ActKind::Call(k::Callee::Local(id)) if lifter.is(id, "ts.bigint.input"))
        && let [inner, _shape] = act.args.as_slice()
    {
        value = inner;
    }
    let k::Expr::Let {
        name: list,
        value: list_value,
        body,
    } = value
    else {
        return None;
    };
    if !padded(lifter, args, names.len(), list_value) {
        return None;
    }
    let mut body: &k::Expr = body;
    for (index, name) in names.iter().enumerate() {
        let k::Expr::Let {
            name: bound,
            value,
            body: rest,
        } = body
        else {
            return None;
        };
        if bound == *name && argument(lifter, list, index, value) {
            body = rest;
            continue;
        }
        if !argument(lifter, list, index, value) {
            return None;
        }
        let k::Expr::Let {
            name: authored,
            value: normalized,
            body: after,
        } = &**rest
        else {
            return None;
        };
        if authored != *name || !bigint_binding(lifter, normalized, bound) {
            return None;
        }
        body = after;
    }
    Some(body)
}

/// How many statements of `body` bind `names` from the padded `args`.
fn arguments_before(
    lifter: &Lifter<'_>,
    args: &Name,
    names: &[&Name],
    body: &[k::Stmt],
) -> Option<usize> {
    let [
        k::Stmt::Let {
            name: list,
            value: k::Rhs::Expr(value),
        },
        rest @ ..,
    ] = body
    else {
        return None;
    };
    if !padded(lifter, args, names.len(), value) || rest.len() < names.len() {
        return None;
    }
    let mut consumed = 0;
    for (index, name) in names.iter().enumerate() {
        let k::Stmt::Let {
            name: bound,
            value: k::Rhs::Expr(value),
        } = rest.get(consumed)?
        else {
            return None;
        };
        if !argument(lifter, list, index, value) {
            return None;
        }
        consumed += 1;
        if bound == *name {
            continue;
        }
        let k::Stmt::Let {
            name: authored,
            value,
        } = rest.get(consumed)?
        else {
            return None;
        };
        let normalized = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => k::Expr::Act(Box::new(super::statements::act_of(action))),
        };
        if authored != *name || !bigint_binding(lifter, &normalized, bound) {
            return None;
        }
        consumed += 1;
    }
    Some(1 + consumed)
}

impl Lifter<'_> {
    fn callable_annotation(&self, ty: &lash_kernel_value::Type) -> Lift<Option<Type>> {
        if matches!(ty, lash_kernel_value::Type::Any) {
            return Ok(None);
        }
        self.ts_type(None, ty, &k::FieldRoot::Result, &mut Vec::new())
            .map(Some)
    }
}

impl Expr {
    /// An arrow of `function`.
    pub(super) fn arrow(function: Function) -> Self {
        Self::Arrow(Box::new(function))
    }
}
