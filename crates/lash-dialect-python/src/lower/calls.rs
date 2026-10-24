//! Calls: functions, tools, built-ins, methods and asyncio.

use lash_kernel_doc::{ActExpr, ActKind, Callee, Expr, Literal, RecordEntry, Rhs, Stmt};

use lash_vocabulary::{EffectName, RecordType, Signature, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};

use lash_kernel_dialect::{Resolution, Spelling};

use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

/// Python built-ins the dialect knows of and does not have.
const UNSUPPORTED: &[&str] = &[
    "open",
    "input",
    "hash",
    "id",
    "setattr",
    "delattr",
    "super",
    "object",
    "frozenset",
    "bytearray",
    "complex",
    "eval",
    "exec",
    "compile",
    "memoryview",
    "issubclass",
    "staticmethod",
    "classmethod",
    "property",
    "__import__",
    "breakpoint",
    "exit",
    "quit",
    "help",
    "aiter",
    "anext",
];

pub(super) fn is_builtin(id: &str) -> bool {
    super::builtin_values::BUILTINS
        .iter()
        .any(|spec| spec.name == id)
        || UNSUPPORTED.contains(&id)
}

/// The names of a call's keywords, in order.
pub(super) fn keyword_names(arguments: &ast::Arguments) -> impl Iterator<Item = String> + '_ {
    arguments.keywords.iter().map(|keyword| {
        keyword
            .arg
            .as_ref()
            .map_or("", |name| name.id.as_str())
            .to_owned()
    })
}

pub(super) fn absent() -> Operand {
    Operand::literal(Literal::Absent, Ty::Unknown)
}

/// A call of a Python function, ready to be made: the variable that holds
/// the callee and the arguments, all evaluated.
struct Prepared {
    callee: Callee,
    args: Vec<Expr>,
}

/// What a call gives, each value evaluated once in source order: the
/// callee or receiver written before the arguments, the positional
/// arguments, then the keyword values. Where the call has keywords every
/// value is pinned, so placing them by parameter never moves an
/// evaluation.
pub(super) struct Given {
    pub(super) head: Operand,
    pub(super) positional: Vec<Operand>,
    pub(super) keywords: Vec<Operand>,
}

impl Lowerer<'_> {
    /// Evaluates `head` and the arguments of `arguments` in source order,
    /// before keywords place any of them by parameter.
    pub(super) fn given(
        &mut self,
        head: Option<&PyExpr>,
        arguments: &ast::Arguments,
    ) -> Lowering<Given> {
        let mut exprs: Vec<&PyExpr> = head.into_iter().collect();
        exprs.extend(arguments.args.iter());
        exprs.extend(arguments.keywords.iter().map(|keyword| &keyword.value));
        let mut operands = self.operands(&exprs)?;
        if !arguments.keywords.is_empty() {
            operands = operands.into_iter().map(|value| self.pin(value)).collect();
        }
        let keywords = operands.split_off(usize::from(head.is_some()) + arguments.args.len());
        let positional = operands.split_off(usize::from(head.is_some()));
        Ok(Given {
            head: operands.pop().unwrap_or_else(Operand::none),
            positional,
            keywords,
        })
    }

    pub(super) fn is_asyncio(&self, expr: &PyExpr) -> bool {
        matches!(expr, PyExpr::Name(name) if self.variable(name.id.as_str()).is_none() && (name.id.as_str() == "asyncio" || self.imported.get(name.id.as_str()).is_some_and(|path|path==&["asyncio"])))
    }

    /// The asyncio function a call names, if it names one.
    fn asyncio_function<'e>(&self, call: &'e ast::ExprCall) -> Option<&'e str> {
        match call.func.as_ref() {
            PyExpr::Attribute(attribute) if self.is_asyncio(&attribute.value) => {
                Some(attribute.attr.id.as_str())
            }
            _ => None,
        }
    }

    /// The refusal of a control call written where the turn cannot end on
    /// it: made into a task, or called inside a function. Only the
    /// program's top level ends the turn.
    fn control_placement(
        &self,
        effect: &EffectName,
        call: &ast::ExprCall,
    ) -> lash_kernel_dialect::Diagnostic {
        diagnostics::with_repair(
            Code::ControlCallPlacement,
            format!(
                "`{effect}` ends the turn, so it is awaited directly at the top level of the program: not passed to `asyncio.gather` or `asyncio.create_task`, or called inside a function"
            ),
            call.range(),
            format!(
                "await everything else first, then write `await {effect}(...)` as the last top-level statement; a function returns its value for the top level to pass on"
            ),
        )
    }

    /// The operation a call names, if it names one: the dotted path of its
    /// callee, from a root that is no variable, resolved through the
    /// namespaces (`web.fetch(...)`, `getattr(processes, "await")(...)`).
    pub(super) fn effect(&self, call: &ast::ExprCall) -> Option<(EffectName, Signature)> {
        let (path, bound) = self.call_path(call.func.as_ref())?;
        if bound {
            return None;
        }
        let written = path.iter().map(String::as_str).collect::<Vec<_>>();
        match self.namespaces.resolve(&written, Spelling::Python) {
            Resolution::Operation { operation, offered } => {
                Some((operation.effect_name(), offered.signature.clone()))
            }
            Resolution::Deferred { operation } => Some((
                operation.effect_name(),
                lash_kernel_dialect::deferred_signature(),
            )),
            _ => None,
        }
    }

    /// Whether the free dotted `path` names package code, which no
    /// deferred provider serves.
    fn names_code(&self, path: &str) -> bool {
        let written = path.split('.').collect::<Vec<_>>();
        matches!(
            self.namespaces.resolve(&written, Spelling::Python),
            Resolution::Export { .. } | Resolution::Namespace
        )
    }

    /// `expr` as a dotted path whose root names no variable: a free
    /// candidate for the run's deferred provider.
    fn tool_path(&self, expr: &PyExpr) -> Option<String> {
        let (path, bound) = self.call_path(expr)?;
        (!bound).then(|| path.join("."))
    }

    /// `expr` as the segments of a dotted path, with whether its root is a
    /// variable. `getattr(path, "name")` is the qualified escape: the path
    /// with `name` after it, for a name Python cannot write as an attribute.
    pub(super) fn call_path(&self, expr: &PyExpr) -> Option<(Vec<String>, bool)> {
        match expr {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                if self.variable(id).is_some() {
                    return Some((vec![id.to_owned()], true));
                }
                if let Some(path) = self.imported.get(id) {
                    return Some((path.clone(), false));
                }
                (!is_builtin(id) && !crate::exceptions::is_builtin(id) && id != "asyncio")
                    .then(|| (vec![id.to_owned()], false))
            }
            PyExpr::Attribute(attribute) => {
                let (mut path, bound) = self.call_path(&attribute.value)?;
                path.push(attribute.attr.id.as_str().to_owned());
                Some((path, bound))
            }
            PyExpr::Call(escape) => {
                let PyExpr::Name(getattr) = escape.func.as_ref() else {
                    return None;
                };
                if getattr.id.as_str() != "getattr"
                    || self.variable("getattr").is_some()
                    || !escape.arguments.keywords.is_empty()
                {
                    return None;
                }
                let [receiver, PyExpr::StringLiteral(name)] = &escape.arguments.args[..] else {
                    return None;
                };
                let (mut path, bound) = self.call_path(receiver)?;
                if path.len() < 2 && !bound && !self.namespaces.is_root(&path[0]) {
                    return None;
                }
                path.push(name.value.to_str().to_owned());
                Some((path, bound))
            }
            _ => None,
        }
    }

    /// The canonical operation a free dotted `path` would name, as a
    /// candidate for the run's deferred provider: `None` for a path that
    /// names no operation. A path that spells one in another dialect's
    /// spelling is refused with the spelling Python writes.
    fn candidate(&self, path: &str, call: &ast::ExprCall) -> Lowering<Option<String>> {
        let segments = path.split('.').collect::<Vec<_>>();
        let Some((name, package)) = segments.split_last() else {
            return Ok(None);
        };
        if package.is_empty()
            || !package
                .iter()
                .all(|segment| lash_vocabulary::Segment::is_canonical(segment))
        {
            return Ok(None);
        }
        if lash_vocabulary::Segment::is_canonical(name) {
            return Ok(Some(path.to_owned()));
        }
        let Some(canonical) =
            Spelling::TypeScript.unspell(lash_kernel_dialect::SpellKind::Member, name)
        else {
            return Ok(None);
        };
        let repair = format!("{}.{canonical}", package.join("."));
        Err(diagnostics::with_repair(
            Code::OperationSpelling,
            format!("`{path}` spells an operation name in another dialect's spelling"),
            call.func.range(),
            format!("write `{repair}`"),
        ))
    }

    /// The refusal of a call whose dotted path names an operation the
    /// program cannot reach as written: its root is a variable, which
    /// shadows the package root lexically, or it spells the operation in
    /// another dialect's spelling.
    fn operation_refusal(&self, call: &ast::ExprCall) -> Lowering<()> {
        let Some((path, bound)) = self.call_path(call.func.as_ref()) else {
            return Ok(());
        };
        let written = path.iter().map(String::as_str).collect::<Vec<_>>();
        if bound {
            if let Some(shadowing) = self.namespaces.shadowed_call(&written, Spelling::Python) {
                return Err(lash_kernel_dialect::Diagnostic::namespace_shadowed(
                    shadowing,
                    Some(diagnostics::span(call.func.range())),
                ));
            }
            return Ok(());
        }
        if let Some(misspelled) = self.namespaces.misspelled(&written, Spelling::Python) {
            return Err(diagnostics::with_repair(
                Code::OperationSpelling,
                format!(
                    "`{}` names the operation `{}` in another dialect's spelling",
                    path.join("."),
                    misspelled.operation.effect_name()
                ),
                call.func.range(),
                format!("write `{}`", misspelled.repair),
            ));
        }
        Ok(())
    }

    /// Refuses `*args` and `**kwargs` at a call.
    pub(super) fn no_stars(&self, arguments: &ast::Arguments) -> Lowering<()> {
        let starred = arguments
            .args
            .iter()
            .find(|arg| arg.is_starred_expr())
            .map(Ranged::range)
            .or_else(|| {
                arguments
                    .keywords
                    .iter()
                    .find(|keyword| keyword.arg.is_none())
                    .map(|keyword| keyword.range)
            });
        match starred {
            Some(range) => Err(diagnostics::with_repair(
                Code::StarUnsupported,
                "`*` and `**` at a call are not in the dialect",
                range,
                format!(
                    "pass the values of `{}` as individual arguments",
                    self.text(range)
                ),
            )),
            None => Ok(()),
        }
    }

    /// Refuses anything but positional arguments.
    pub(super) fn plain_arguments(&self, arguments: &ast::Arguments, what: &str) -> Lowering<()> {
        self.no_stars(arguments)?;
        match arguments.keywords.first() {
            Some(keyword) => Err(arguments_error(
                format!("{what} takes no keyword arguments"),
                keyword.range,
            )),
            None => Ok(()),
        }
    }

    pub(super) fn call(&mut self, call: &ast::ExprCall, awaited: bool) -> Lowering<Operand> {
        let mut named = std::collections::BTreeSet::new();
        for keyword in &call.arguments.keywords {
            if let Some(name) = &keyword.arg
                && !named.insert(name.id.as_str())
            {
                return Err(arguments_error("keyword argument repeated", keyword.range));
            }
        }
        let constructor_path = match call.func.as_ref() {
            PyExpr::Attribute(attribute) => match attribute.value.as_ref() {
                PyExpr::Name(name) if self.variable(name.id.as_str()).is_none() => {
                    Some(format!("{}.{}", name.id, attribute.attr.id))
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(path) = constructor_path
            && let Some(constructor) =
                lash_kernel_dialect::value_constructors::Constructor::named(&path)
        {
            self.plain_arguments(&call.arguments, "a value constructor")?;
            let args: Vec<_> = call.arguments.args.iter().collect();
            let operands = self.operands(&args)?;
            let value = constructor
                .build(operands.into_iter().map(|v| v.expr).collect(), |name| {
                    self.function(name).ok()
                })
                .map_err(|message| diagnostics::unplaced(Code::Syntax, message))?;
            return Ok(Operand::inline(value, Ty::Unknown));
        }
        if self
            .call_path(&call.func)
            .is_some_and(|(path, bound)| !bound && path == ["time", "time"])
        {
            self.plain_arguments(&call.arguments, "time.time")?;
            if !call.arguments.args.is_empty() {
                return Err(arguments_error(
                    "time.time takes no arguments",
                    call.range(),
                ));
            }
            return self.invoke("py.time", &[], Ty::Float);
        }
        if self.call_path(&call.func).is_some_and(|(path, bound)| {
            !bound && matches!(path.first().map(String::as_str), Some("math" | "json"))
        }) {
            return self.call_function(call, awaited);
        }
        self.operation_refusal(call)?;
        if self.calls.is_some()
            && let Some(path) = self.tool_path(&call.func)
            && !self.names_code(&path)
        {
            if let Some(candidate) = self.candidate(&path, call)?
                && let Some(calls) = &mut self.calls
            {
                calls.insert(candidate);
            }
            for argument in &call.arguments.args {
                self.expr(argument)?;
            }
            for keyword in &call.arguments.keywords {
                self.expr(&keyword.value)?;
            }
            return Ok(Operand::none());
        }
        if self.exception_class(&call.func).is_some() {
            return self.call_function(call, awaited);
        }
        if let Some(function) = self.asyncio_function(call) {
            return self.asyncio(function, call, awaited);
        }
        if let Some((effect, signature)) = self.effect(call) {
            return self.tool_call(&effect, &signature, call, awaited);
        }
        if let Some(called) = self.call_package(call, awaited) {
            return called;
        }
        // `getattr(dataframe, "lambda")`: the escape, read as a value.
        if matches!(call.func.as_ref(), PyExpr::Name(name) if name.id.as_str() == "getattr")
            && let Some(read) = self.read_package(&PyExpr::Call(call.clone()))
        {
            return read;
        }
        match call.func.as_ref() {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                if self.variable(id).is_some() {
                    return self.call_function(call, awaited);
                }
                if let Some(spec) = super::builtin_values::BUILTINS
                    .iter()
                    .find(|spec| spec.name == id)
                {
                    if !call.arguments.args.iter().any(PyExpr::is_starred_expr)
                        && call.arguments.keywords.iter().all(|kw| kw.arg.is_some())
                    {
                        return self.builtin_direct(spec, call);
                    }
                    return self.call_function(call, awaited);
                }
                if UNSUPPORTED.contains(&id) {
                    let repair = self.builtin_repair(id, call);
                    return Err(diagnostics::with_repair(
                        Code::BuiltinUnsupported,
                        format!("the built-in `{id}` is not in the dialect"),
                        call.range(),
                        repair,
                    ));
                }
                Err(self.not_a_variable(id, name.range))
            }
            PyExpr::Attribute(attribute) => self.method(attribute, call, awaited),
            _ => self.call_function(call, awaited),
        }
    }

    /// A call of the tool `effect`, which a cell awaits.
    fn tool_call(
        &mut self,
        effect: &EffectName,
        signature: &Signature,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        if !awaited {
            return Err(diagnostics::with_repair(
                Code::CoroutineNotAwaited,
                format!("the tool `{effect}` is called without `await`"),
                call.range(),
                format!(
                    "write `await {}`, or `asyncio.create_task({})` to run it beside this code",
                    self.text(call.range()),
                    self.text(call.range())
                ),
            ));
        }
        let control = self.namespaces.controls(effect).is_some();
        if control && !self.in_module() {
            return Err(self.control_placement(effect, call));
        }
        let action = self.perform(effect, signature, call)?;
        let result = Operand::inline(Expr::Act(Box::new(action)), Ty::Unknown);

        if control {
            // The call settled, so the turn is over: nothing after it runs
            // (`K-FORM-019`). The host reads the control from the settled
            // call, never from this value.
            let ignored = self.temp();
            self.emit(Stmt::Finish {
                value: Expr::Let {
                    name: ignored,
                    value: Box::new(result.expr),
                    body: Box::new(Expr::Literal(Literal::Null)),
                },
            });
            return Ok(Operand::none());
        }
        // The reply reads as `json.loads` gives it: an object is a dict.
        self.invoke("py.reply", &[result], Ty::Unknown)
    }

    /// Evaluates the callee and the arguments of a call of a Python
    /// function, in order, and places the arguments by the parameters
    /// where keywords need it.
    ///
    /// The inner error is the message of the TypeError a call Python would
    /// refuse raises when it runs. `in_task` is a call a new task makes,
    /// where no exception is being handled.
    fn prepare(
        &mut self,
        call: &ast::ExprCall,
        awaited: bool,
        in_task: bool,
    ) -> Lowering<Result<Prepared, String>> {
        let signature = match call.func.as_ref() {
            PyExpr::Name(name) => self.known_def(name.id.as_str()),
            _ => None,
        };
        if !awaited
            && signature
                .as_ref()
                .is_some_and(|signature| signature.is_async)
        {
            return Err(diagnostics::with_repair(
                Code::CoroutineNotAwaited,
                "a coroutine function is called without `await`",
                call.range(),
                format!(
                    "write `await {0}`, or `asyncio.create_task({0})` to run it beside this code",
                    self.text(call.range())
                ),
            ));
        }
        if let PyExpr::Name(name) = call.func.as_ref()
            && let Some(scope) = self
                .variable(name.id.as_str())
                .and_then(|found| found.scope)
            && self.is_bound(scope, name.id.as_str())
            && let Some(direct) = self.scopes[scope]
                .direct_defs
                .get(name.id.as_str())
                .cloned()
            && call.arguments.keywords.is_empty()
            && call.arguments.args.len() == direct.arity
            && !call.arguments.args.iter().any(ast::Expr::is_starred_expr)
        {
            let given = self.given(None, &call.arguments)?;
            let mut args = given
                .positional
                .into_iter()
                .map(|value| value.expr)
                .collect::<Vec<_>>();
            if direct.pass_active {
                args.push(if in_task { absent() } else { self.active() }.expr);
            }
            return Ok(Ok(Prepared {
                callee: Callee::Value(direct.binding),
                args,
            }));
        }
        let bare = match call.func.as_ref() {
            PyExpr::Name(name) => self
                .variable(name.id.as_str())
                .and_then(|found| found.scope)
                .is_some_and(|scope| {
                    self.scopes[scope]
                        .direct_defs
                        .get(name.id.as_str())
                        .is_some_and(|direct| !direct.pass_active)
                }),
            _ => false,
        };
        let known = signature.is_some();
        if known
            && let PyExpr::Name(name) = call.func.as_ref()
            && let Some(scope) = self
                .variable(name.id.as_str())
                .and_then(|found| found.scope)
            && !self.scopes[scope]
                .direct_defs
                .contains_key(name.id.as_str())
        {
            // An earlier compiled forward call expects a Python token.
            self.scopes[scope]
                .packed_defs
                .insert(name.id.as_str().to_owned());
        }
        let callee = self.expr(&call.func)?;
        let callee = self.pin(callee);
        let (positional, keywords) = self.packed_arguments(&call.arguments)?;
        // Named module entries keep the declared Direct ABI; ordinary guest
        // callable values carry their packed ABI in the token.
        if let PyExpr::Name(name) = call.func.as_ref()
            && (bare
                || self.module.is_some()
                    && self
                        .variable(name.id.as_str())
                        .is_some_and(|found| found.scope == Some(0)))
            && let Some(signature) = signature
        {
            let metadata = signature
                .params
                .iter()
                .map(|param| {
                    Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(param.name.to_string())),
                        Expr::Literal(Literal::Int(1.into())),
                        Expr::Literal(Literal::Bool(param.has_default)),
                    ])
                })
                .collect();
            let slots = self.invoke(
                "py.bind",
                &[
                    Operand::text(name.id.as_str()),
                    Operand::inline(Expr::Tuple(metadata), Ty::Tuple),
                    positional,
                    keywords,
                    Operand::literal(Literal::Bool(bare), Ty::Bool),
                ],
                Ty::List,
            )?;
            let slots = self.pin(slots);
            let args: Vec<_> = (0..signature.params.len())
                .map(|i| {
                    Expr::Member(Box::new(lash_kernel_doc::Member::Index {
                        target: slots.expr.clone(),
                        index: Expr::Literal(Literal::Int(
                            i64::try_from(i).unwrap_or(i64::MAX).into(),
                        )),
                    }))
                })
                .collect();
            let Expr::Variable(callee) = callee.expr else {
                unreachable!("pinned callee")
            };
            return Ok(Ok(Prepared {
                callee: Callee::Value(callee),
                args,
            }));
        }
        // A once-bound authored def is already a validated Python token.
        // Project its packed closure here so the graph retains the direct
        // callable edge, while the closure's own binder still owns arguments.
        if known {
            let packed = self.pin(Operand::inline(
                Expr::Member(Box::new(lash_kernel_doc::Member::Index {
                    target: callee.expr,
                    index: Expr::Literal(Literal::Int(1.into())),
                })),
                Ty::Unknown,
            ));
            let Expr::Variable(packed) = packed.expr else {
                unreachable!("pinned closure")
            };
            let active = if in_task { absent() } else { self.active() };
            return Ok(Ok(Prepared {
                callee: Callee::Value(packed),
                args: vec![positional.expr, keywords.expr, active.expr],
            }));
        }
        let provider_body = self.block(|this| {
            let namespace = this.namespace_copy(false)?;
            this.return_value(namespace);
            Ok(())
        })?;
        let namespace = self.emit_closure(Vec::new(), provider_body, None);
        let active = if in_task { absent() } else { self.active() };
        Ok(Ok(Prepared {
            callee: self.helper_callee("py.call")?,
            args: vec![
                callee.expr,
                positional.expr,
                keywords.expr,
                active.expr,
                namespace.expr,
            ],
        }))
    }

    pub(super) fn call_function(
        &mut self,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        let prepared = match self.prepare(call, awaited, false)? {
            Ok(prepared) => prepared,
            Err(problem) => {
                self.invoke_do(
                    "py.fail",
                    &[Operand::text("TypeError"), Operand::text(problem)],
                )?;
                return Ok(Operand::none());
            }
        };
        Ok(Operand::inline(
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(prepared.callee),
                args: prepared.args,
            })),
            Ty::Unknown,
        ))
    }

    /// One run of a tool, with its arguments placed by the tool's
    /// signature.
    fn perform(
        &mut self,
        effect: &EffectName,
        signature: &Signature,
        call: &ast::ExprCall,
    ) -> Lowering<ActExpr> {
        if call.arguments.args.iter().any(PyExpr::is_starred_expr)
            || call.arguments.keywords.iter().any(|kw| kw.arg.is_none())
        {
            let (positional, keywords) = self.packed_arguments(&call.arguments)?;
            let metadata = signature
                .params
                .iter()
                .map(|param| {
                    Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(param.name.to_string())),
                        Expr::Literal(Literal::Int(1.into())),
                        Expr::Literal(Literal::Bool(param.optional)),
                    ])
                })
                .collect();
            let slots = self.invoke(
                "py.bind",
                &[
                    Operand::text(effect.to_string()),
                    Operand::inline(Expr::Tuple(metadata), Ty::Tuple),
                    positional,
                    keywords,
                    Operand::literal(Literal::Bool(false), Ty::Bool),
                ],
                Ty::List,
            )?;
            let slots = self.pin(slots);
            let args: Vec<_> = (0..signature.params.len())
                .map(|i| {
                    Expr::Member(Box::new(lash_kernel_doc::Member::Index {
                        target: slots.expr.clone(),
                        index: Expr::Literal(Literal::Int(
                            i64::try_from(i).unwrap_or(i64::MAX).into(),
                        )),
                    }))
                })
                .collect();
            let operation = lash_vocabulary::names::QualifiedOperation::from_effect(effect)
                .map_err(|error| diagnostics::unplaced(Code::LibraryMissing, error.to_string()))?;
            let slot = self.imports.operation(
                lash_vocabulary::names::QualifiedName {
                    package: operation.package,
                    member: operation.name,
                    type_member: None,
                },
                signature.clone(),
            );
            return Ok(ActExpr {
                kind: ActKind::Perform {
                    slot,
                    result: signature.result.clone(),
                    then_yield: false,
                },
                args,
            });
        }
        let deferred = self.namespaces.operation(effect).is_none();
        let args = if deferred {
            let exprs = call.arguments.args.iter().collect::<Vec<_>>();
            let values = self.operands(&exprs)?;
            let mut args = values
                .into_iter()
                .map(|value| value.expr)
                .collect::<Vec<_>>();
            if !call.arguments.keywords.is_empty() {
                let mut fields = Vec::new();
                for keyword in &call.arguments.keywords {
                    let name = keyword
                        .arg
                        .as_ref()
                        .ok_or_else(|| {
                            diagnostics::unplaced(
                                Code::StarUnsupported,
                                "keyword expansion is not supported",
                            )
                        })?
                        .id
                        .to_string();
                    let value = self.expr(&keyword.value)?;
                    fields.push(RecordEntry {
                        field: name,
                        value: value.expr,
                    });
                }
                args.push(Expr::Record(fields));
            }
            args
        } else if let [param] = signature.params.as_slice()
            && let Type::Record(input) = &param.ty
            && !call.arguments.keywords.is_empty()
        {
            let input = self.tool_keyword_input(effect, input, call)?;
            vec![input.expr]
        } else if let [param] = signature.params.as_slice()
            && matches!(param.ty, Type::Record(_))
            && let [input] = call.arguments.args.as_ref()
            && call.arguments.keywords.is_empty()
        {
            vec![self.tool_input(input, &param.ty)?.expr]
        } else {
            self.tool_arguments(effect, signature, call)?
        };
        let operation = lash_vocabulary::names::QualifiedOperation::from_effect(effect)
            .map_err(|error| diagnostics::unplaced(Code::LibraryMissing, error.to_string()))?;
        let deferred = self.namespaces.operation(effect).is_none();
        let slot = self.imports.operation(
            lash_vocabulary::names::QualifiedName {
                package: operation.package,
                member: operation.name,
                type_member: None,
            },
            signature.clone(),
        );
        Ok(ActExpr {
            kind: ActKind::Perform {
                slot,
                result: signature.result.clone(),
                then_yield: false,
            },
            args: if deferred {
                vec![Expr::List(args)]
            } else {
                args
            },
        })
    }

    /// A literal Python dict at a declared record boundary is transported as
    /// that record. Nested literals follow the declared field types; ordinary
    /// dictionary values elsewhere keep Python's Map representation.
    fn tool_input(&mut self, expr: &PyExpr, ty: &Type) -> Lowering<Operand> {
        match (expr, ty) {
            (PyExpr::Dict(dict), Type::Record(record))
                if dict
                    .items
                    .iter()
                    .all(|item| matches!(&item.key, Some(PyExpr::StringLiteral(_)))) =>
            {
                let mut fields = Vec::<RecordEntry>::new();
                for item in &dict.items {
                    let Some(PyExpr::StringLiteral(key)) = &item.key else {
                        unreachable!()
                    };
                    let name = key.value.to_str();
                    let ty = record
                        .fields
                        .iter()
                        .find(|field| field.name == name)
                        .map(|field| &field.ty)
                        .or(record.rest.kept_type())
                        .unwrap_or(&Type::Any);
                    let value = self.tool_input(&item.value, ty)?;
                    let value = self.pin(value);
                    if let Some(field) = fields.iter_mut().find(|field| field.field == name) {
                        field.value = value.expr;
                    } else {
                        fields.push(RecordEntry {
                            field: name.into(),
                            value: value.expr,
                        });
                    }
                }
                Ok(Operand::inline(Expr::Record(fields), Ty::Unknown))
            }
            (_, Type::Record(_)) => {
                let value = self.expr(expr)?;
                self.invoke("py.operation_input", &[value], Ty::Unknown)
            }
            _ => self.expr(expr),
        }
    }

    /// The host's single input record is spelled as declared keyword fields
    /// in Python. Keep evaluation in source order and omit absent fields.
    fn tool_keyword_input(
        &mut self,
        effect: &EffectName,
        input: &RecordType,
        call: &ast::ExprCall,
    ) -> Lowering<Operand> {
        if !call.arguments.args.is_empty() {
            return Err(arguments_error(
                format!("the tool `{effect}` takes either an input record or keyword fields"),
                call.range(),
            ));
        }
        let mut fields = Vec::<RecordEntry>::new();
        for keyword in &call.arguments.keywords {
            let name = keyword.arg.as_ref().map_or("", |name| name.id.as_str());
            if !input.fields.iter().any(|field| field.name == name) {
                return Err(arguments_error(
                    format!("the tool `{effect}` has no parameter `{name}`"),
                    keyword.range,
                ));
            }
            if fields.iter().any(|field| field.field == name) {
                return Err(arguments_error(
                    format!("the tool `{effect}` got multiple values for `{name}`"),
                    keyword.range,
                ));
            }
            let ty = input
                .fields
                .iter()
                .find(|field| field.name == name)
                .map(|field| &field.ty)
                .or(input.rest.kept_type())
                .unwrap_or(&Type::Any);
            let value = self.tool_input(&keyword.value, ty)?;
            let value = self.pin(value);
            fields.push(RecordEntry {
                field: name.to_owned(),
                value: value.expr,
            });
        }
        for field in &input.fields {
            if !field.optional && !fields.iter().any(|given| given.field == field.name) {
                return Err(arguments_error(
                    format!(
                        "the call gives the tool `{effect}` no value for `{}`",
                        field.name
                    ),
                    call.range(),
                ));
            }
        }
        Ok(self.let_rhs(Rhs::Expr(Expr::Record(fields)), Ty::Unknown))
    }

    fn tool_arguments(
        &mut self,
        effect: &EffectName,
        signature: &Signature,
        call: &ast::ExprCall,
    ) -> Lowering<Vec<Expr>> {
        let positional = call.arguments.args.len();
        if positional > signature.params.len() {
            return Err(arguments_error(
                format!(
                    "the tool `{effect}` takes {} arguments but {positional} were given",
                    signature.params.len()
                ),
                call.range(),
            ));
        }
        let Given {
            positional: given,
            keywords: keyword_values,
            ..
        } = self.given(None, &call.arguments)?;
        let mut slots: Vec<Option<Operand>> = given.into_iter().map(Some).collect();
        slots.resize(signature.params.len(), None);
        for (keyword, value) in call.arguments.keywords.iter().zip(keyword_values) {
            let name = keyword.arg.as_ref().map_or("", |name| name.id.as_str());
            let Some(index) = signature
                .params
                .iter()
                .position(|param| param.name.as_str() == name)
            else {
                return Err(arguments_error(
                    format!("the tool `{effect}` has no parameter `{name}`"),
                    keyword.range,
                ));
            };
            if slots[index].is_some() {
                return Err(arguments_error(
                    format!("the tool `{effect}` got multiple values for `{name}`"),
                    keyword.range,
                ));
            }
            slots[index] = Some(value);
        }
        for (slot, param) in slots.iter().zip(&signature.params) {
            if slot.is_none() && !param.optional {
                return Err(arguments_error(
                    format!(
                        "the call gives the tool `{effect}` no value for `{}`",
                        param.name
                    ),
                    call.range(),
                ));
            }
        }
        while matches!(slots.last(), Some(None)) {
            slots.pop();
        }
        // A task may run this later: every argument is one the caller's
        // later statements cannot change.
        let slots: Vec<Operand> = slots
            .into_iter()
            .map(|slot| match slot {
                Some(operand) => operand,
                None => absent(),
            })
            .collect();
        Ok(slots.into_iter().map(|slot| slot.expr).collect())
    }

    pub(super) fn await_expr(&mut self, await_expr: &ast::ExprAwait) -> Lowering<Operand> {
        if !self.in_async() {
            return Err(diagnostics::diagnostic(
                Code::AwaitOutsideAsync,
                "`await` outside an `async def`",
                await_expr.range,
            ));
        }
        self.awaited(&await_expr.value)
    }

    /// `await value`: an `await` action by the policy that awaits what
    /// the value can be (`K-TASK-026`). Python's policies take no turn of
    /// their own: a task that has ended gives its answer at once.
    fn awaited(&mut self, value: &PyExpr) -> Lowering<Operand> {
        let PyExpr::Call(call) = value else {
            let value = self.expr(value)?;
            return self.await_via("py.await", value);
        };
        // An awaited coroutine call is an ordinary call, and an awaited
        // tool call is one `perform` and the reading of its reply. The value
        // of a call the front end can see is awaitable only if it is a task.
        // Of a call of a value it cannot see, a coroutine call has run
        // already: what it returned is what the `await` gives.
        let helper = match call.func.as_ref() {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                match self.known_def(id) {
                    Some(signature) if signature.is_async => None,
                    Some(_) => Some("py.await"),
                    None if self.effect(call).is_some() => None,
                    None if self.variable(id).is_some() => Some("py.await.result"),
                    None => Some("py.await"),
                }
            }
            PyExpr::Attribute(_) if self.effect(call).is_some() => None,
            PyExpr::Attribute(_) => match self.asyncio_function(call) {
                Some("sleep" | "gather") => None,
                Some(_) => Some("py.await"),
                // A method call awaits in its own lowering: a package
                // member that waits gives its result where it stands.
                None => None,
            },
            // `await getattr(dataframe, "name")(...)`.
            PyExpr::Call(_) if matches!(self.package_ref(&call.func), Ok(Some(_))) => None,
            _ => Some("py.await.result"),
        };
        let value = self.call(call, true)?;
        match helper {
            None => Ok(value),
            Some(policy) => self.await_via(policy, value),
        }
    }

    /// `await value via policy`.
    pub(super) fn await_via(&mut self, policy: &str, value: Operand) -> Lowering<Operand> {
        let policy = self.function_target(policy)?;
        Ok(Operand::inline(
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Await(policy),
                args: vec![value.expr],
            })),
            Ty::Unknown,
        ))
    }

    fn asyncio(
        &mut self,
        function: &str,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        let not_awaited = |range| {
            diagnostics::with_repair(
                Code::CoroutineNotAwaited,
                format!("`{}` is called without `await`", self.text(call.range())),
                range,
                format!("write `await {}`", self.text(call.range())),
            )
        };
        match function {
            "sleep" => {
                if !awaited {
                    return Err(not_awaited(call.range()));
                }
                self.plain_arguments(&call.arguments, "asyncio.sleep")?;
                let [seconds] = &*call.arguments.args else {
                    return Err(arguments_error(
                        "asyncio.sleep takes the number of seconds",
                        call.range(),
                    ));
                };
                let seconds = self.expr(seconds)?;
                self.invoke("py.sleep", &[seconds], Ty::None)
            }
            "gather" => {
                if !awaited {
                    return Err(not_awaited(call.range()));
                }
                self.creates_tasks = true;
                let (tasks, exceptions) = self.gather_members(call)?;
                self.invoke("py.gather", &[tasks, exceptions], Ty::List)
            }
            "create_task" | "ensure_future" => {
                self.creates_tasks = true;
                self.plain_arguments(&call.arguments, "asyncio.create_task")?;
                let [coroutine] = &*call.arguments.args else {
                    return Err(arguments_error(
                        "asyncio.create_task takes one coroutine call",
                        call.range(),
                    ));
                };
                self.spawn(coroutine)
            }
            "run" => {
                if !self.in_module() {
                    return Err(diagnostics::with_repair(
                        Code::AsyncUnsupported,
                        "`asyncio.run` stands at the top level of a cell",
                        call.range(),
                        format!(
                            "inside an async function, await the coroutine passed to `{}`",
                            self.text(call.range())
                        ),
                    ));
                }
                self.plain_arguments(&call.arguments, "asyncio.run")?;
                let [coroutine] = &*call.arguments.args else {
                    return Err(arguments_error(
                        "asyncio.run takes one coroutine call",
                        call.range(),
                    ));
                };
                let value = self.awaited(coroutine)?;
                let value = self.pin(value);
                self.invoke_do("tasks.cancel_all", &[])?;
                Ok(value)
            }
            other => Err(diagnostics::with_repair(
                Code::AsyncUnsupported,
                format!("`asyncio.{other}` is not in the dialect"),
                call.range(),
                format!(
                    "replace `{}` with asyncio.sleep, gather, create_task or run",
                    self.text(call.func.range())
                ),
            )),
        }
    }

    /// The tasks a `gather` waits for, as a list: a task for each
    /// coroutine call, made in order, and each task given as it is.
    fn gather_members(&mut self, call: &ast::ExprCall) -> Lowering<(Operand, Operand)> {
        if !call.arguments.args.iter().any(PyExpr::is_starred_expr)
            && call.arguments.keywords.iter().all(|kw| {
                kw.arg
                    .as_ref()
                    .is_some_and(|name| name.id.as_str() == "return_exceptions")
            })
        {
            let mut members = Vec::new();
            for member in &call.arguments.args {
                let value = if matches!(member, PyExpr::Call(_)) {
                    self.spawn(member)?
                } else {
                    self.expr(member)?
                };
                members.push(self.pin(value).expr);
            }
            let exceptions = match call.arguments.keywords.first() {
                Some(keyword) => {
                    let value = self.expr(&keyword.value)?;
                    self.pin(value)
                }
                None => Operand::literal(Literal::Bool(false), Ty::Bool),
            };
            return Ok((Operand::inline(Expr::List(members), Ty::List), exceptions));
        }
        let tasks = self.let_rhs(Rhs::Expr(Expr::List(Vec::new())), Ty::List);
        for member in &call.arguments.args {
            match member {
                PyExpr::Starred(starred) => {
                    let values = self.expr(&starred.value)?;
                    self.invoke_do("py.args.extend", &[tasks.clone(), values])?;
                }
                member => {
                    let value = if matches!(member, PyExpr::Call(_)) {
                        self.spawn(member)?
                    } else {
                        self.expr(member)?
                    };
                    self.invoke_do("py.args.append", &[tasks.clone(), value])?;
                }
            }
        }
        let keywords = self.let_rhs(Rhs::Expr(Expr::Map(Vec::new())), Ty::Dict);
        for keyword in &call.arguments.keywords {
            let value = self.expr(&keyword.value)?;
            if let Some(name) = &keyword.arg {
                self.invoke_do(
                    "py.kw.put",
                    &[keywords.clone(), Operand::text(name.id.as_str()), value],
                )?;
            } else {
                self.invoke_do("py.kw.extend", &[keywords.clone(), value])?;
            }
        }
        let exceptions = self.invoke("py.gather.options", &[keywords], Ty::Bool)?;
        Ok((tasks, exceptions))
    }

    /// A new task that runs the coroutine call `coroutine`. Its callee and
    /// arguments are evaluated here; the call itself runs in the task,
    /// when the ready queue reaches it.
    fn spawn(&mut self, coroutine: &PyExpr) -> Lowering<Operand> {
        let written = self.text(coroutine.range()).to_string();
        let refusal = |range| {
            diagnostics::with_repair(
                Code::AsyncUnsupported,
                "a task is made from a coroutine call written in place",
                range,
                format!(
                    "replace `{}` with a coroutine call written in place; pass that call to `asyncio.create_task`",
                    written
                ),
            )
        };
        let PyExpr::Call(call) = coroutine else {
            return Err(refusal(coroutine.range()));
        };
        let action = match self.asyncio_function(call) {
            // A task that already exists is given as it is.
            Some("create_task" | "ensure_future") => return self.call(call, false),
            Some("sleep") => {
                self.plain_arguments(&call.arguments, "asyncio.sleep")?;
                let [seconds] = &*call.arguments.args else {
                    return Err(arguments_error(
                        "asyncio.sleep takes the number of seconds",
                        call.range(),
                    ));
                };
                let seconds = self.expr(seconds)?;
                let seconds = self.pin(seconds);
                ActExpr {
                    kind: ActKind::Call(self.helper_callee("py.sleep")?),
                    args: vec![seconds.expr],
                }
            }
            Some("gather") => {
                let (tasks, exceptions) = self.gather_members(call)?;
                ActExpr {
                    kind: ActKind::Call(self.helper_callee("py.gather")?),
                    args: vec![tasks.expr, exceptions.expr],
                }
            }
            Some(_) => return Err(refusal(call.range())),
            None => {
                if let Some((effect, signature)) = self.effect(call) {
                    if self.namespaces.controls(&effect).is_some() {
                        return Err(self.control_placement(&effect, call));
                    }
                    self.perform(&effect, &signature, call)?
                } else {
                    let callable = match call.func.as_ref() {
                        PyExpr::Name(name) => self.variable(name.id.as_str()).is_some(),
                        PyExpr::Attribute(_) => false,
                        _ => true,
                    };
                    if !callable || self.exception_class(&call.func).is_some() {
                        return Err(refusal(call.range()));
                    }
                    let prepared = match self.prepare(call, true, true)? {
                        Ok(prepared) => prepared,
                        Err(problem) => {
                            self.invoke_do(
                                "py.fail",
                                &[Operand::text("TypeError"), Operand::text(problem)],
                            )?;
                            return Ok(Operand::none());
                        }
                    };
                    let args = prepared
                        .args
                        .into_iter()
                        .map(|arg| match arg {
                            Expr::Variable(name) => {
                                let pinned =
                                    self.pin(Operand::inline(Expr::Variable(name), Ty::Unknown));
                                pinned.expr
                            }
                            expr => self.pin(Operand::inline(expr, Ty::Unknown)).expr,
                        })
                        .collect();
                    ActExpr {
                        kind: ActKind::Call(prepared.callee),
                        args,
                    }
                }
            }
        };
        let mut action = action;
        action.args = action
            .args
            .into_iter()
            .map(|expr| self.pin(Operand::inline(expr, Ty::Unknown)).expr)
            .collect();
        let reply = matches!(action.kind, ActKind::Perform { .. });
        let body = self.block(|this| {
            let mut result = Operand::inline(Expr::Act(Box::new(action)), Ty::Unknown);
            if reply {
                result = this.invoke("py.reply", &[result], Ty::Unknown)?;
            }
            this.emit(Stmt::Return { value: result.expr });
            Ok(())
        })?;
        let work = self.emit_closure(Vec::new(), body, None);
        let run = self.helper_callee("py.task.run")?;
        Ok(Operand::inline(
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Spawn(run),
                args: vec![work.expr],
            })),
            Ty::Unknown,
        ))
    }

    /// The positional arguments of a built-in that takes between `min`
    /// and `max` of them and no keywords.
    pub(super) fn positional(
        &mut self,
        name: &str,
        call: &ast::ExprCall,
        min: usize,
        max: usize,
    ) -> Lowering<Vec<Operand>> {
        self.plain_arguments(&call.arguments, name)?;
        let count = call.arguments.args.len();
        if count < min || count > max {
            return Err(arguments_error(
                format!("`{name}` takes {min} to {max} arguments, and {count} were given"),
                call.range(),
            ));
        }
        let exprs: Vec<&PyExpr> = call.arguments.args.iter().collect();
        self.operands(&exprs)
    }
}

pub(super) fn arguments_error(
    message: impl Into<String>,
    range: TextRange,
) -> lash_kernel_dialect::Diagnostic {
    diagnostics::diagnostic(Code::Arguments, message, range)
}
