//! Calls: functions, tools, built-ins, methods and asyncio.

use lash_kernel_doc::{ActExpr, ActKind, Callee, Expr, Literal, RecordEntry, Rhs, Stmt};

use lash_vocabulary::{EffectName, Name, RecordType, Signature, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};

use lash_kernel_dialect::{Resolution, Spelling};

use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

/// The built-in functions the dialect has.
const SUPPORTED: &[&str] = &[
    "print",
    "len",
    "str",
    "repr",
    "int",
    "float",
    "bool",
    "abs",
    "min",
    "max",
    "sum",
    "sorted",
    "list",
    "tuple",
    "set",
    "dict",
    "range",
    "enumerate",
    "zip",
    "reversed",
    "any",
    "all",
    "round",
    "isinstance",
    "ord",
    "chr",
    "divmod",
    "type",
];

/// Python built-ins the dialect knows of and does not have.
const UNSUPPORTED: &[&str] = &[
    "map",
    "filter",
    "open",
    "input",
    "iter",
    "next",
    "hash",
    "id",
    "callable",
    "getattr",
    "setattr",
    "hasattr",
    "delattr",
    "format",
    "super",
    "object",
    "frozenset",
    "bytes",
    "bytearray",
    "complex",
    "pow",
    "hex",
    "oct",
    "bin",
    "ascii",
    "vars",
    "dir",
    "globals",
    "locals",
    "eval",
    "exec",
    "compile",
    "memoryview",
    "slice",
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

pub(super) fn is_supported_builtin(id: &str) -> bool {
    SUPPORTED.contains(&id)
}

pub(super) fn is_builtin(id: &str) -> bool {
    SUPPORTED.contains(&id) || UNSUPPORTED.contains(&id)
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
    callee: Name,
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
        self.no_stars(&call.arguments)
            .or_else(|error| match self.asyncio_function(call) {
                // `asyncio.gather(*tasks)` is the one starred call.
                Some("gather") => Ok(()),
                _ => Err(error),
            })?;
        if let Some(class) = self.exception_class(&call.func) {
            self.plain_arguments(&call.arguments, "an exception")?;
            let exprs: Vec<&PyExpr> = call.arguments.args.iter().collect();
            let operands = self.operands(&exprs)?;
            let args = Self::tuple_of(&operands);
            return self.raised(&class, args, Operand::none());
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
                if SUPPORTED.contains(&id) {
                    return self.builtin(id, call);
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
        let Given {
            head: callee,
            positional,
            keywords: keyword_values,
        } = self.given(Some(&call.func), &call.arguments)?;
        let mut slots: Vec<Option<Operand>> = positional.into_iter().map(Some).collect();
        if !call.arguments.keywords.is_empty() {
            let Some(signature) = &signature else {
                return Err(diagnostics::with_repair(
                    Code::KeywordCallDynamic,
                    "keyword arguments need a callee the front end can see",
                    call.range(),
                    format!(
                        "bind `{}` with a visible `def`, or pass these values by position: `{}`",
                        self.text(call.func.range()),
                        self.positional_repair(call)
                    ),
                ));
            };
            // A call Python would refuse when it runs raises then, not
            // here: the arguments are evaluated first, as Python does.
            let mut problem = None;
            if slots.len() > signature.params.len() {
                problem = Some("the function takes fewer positional arguments".to_string());
            }
            slots.resize(signature.params.len(), None);
            for (keyword, value) in call.arguments.keywords.iter().zip(keyword_values) {
                let name = keyword.arg.as_ref().map_or("", |name| name.id.as_str());
                match signature
                    .params
                    .iter()
                    .position(|param| param.name.as_str() == name)
                {
                    None => {
                        problem = Some(format!("got an unexpected keyword argument '{name}'"));
                    }
                    Some(index) if slots[index].is_some() => {
                        problem = Some(format!("got multiple values for argument '{name}'"));
                    }
                    Some(index) => slots[index] = Some(value),
                }
            }
            for (slot, param) in slots.iter().zip(&signature.params) {
                if slot.is_none() && !param.has_default {
                    problem.get_or_insert(format!("missing a required argument: '{}'", param.name));
                }
            }
            if let Some(problem) = problem {
                return Ok(Err(problem));
            }
            // A parameter the call leaves to its default is omitted.
            while matches!(slots.last(), Some(None)) {
                slots.pop();
            }
        }
        let mut slots: Vec<Operand> = slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(absent))
            .collect();
        // A function that may raise again what its caller handles takes it
        // after every parameter.
        if let (PyExpr::Name(name), Some(signature)) = (call.func.as_ref(), &signature)
            && self.takes_active(name.id.as_str())
        {
            let count = signature.params.len();
            if slots.len() > count {
                let fewest = signature
                    .params
                    .iter()
                    .filter(|param| !param.has_default)
                    .count();
                let takes = match (fewest == count, count) {
                    (true, 1) => "1 positional argument".to_string(),
                    (true, _) => format!("{count} positional arguments"),
                    (false, _) => format!("from {fewest} to {count} positional arguments"),
                };
                let given = match slots.len() {
                    1 => "1 was given".to_string(),
                    many => format!("{many} were given"),
                };
                return Ok(Err(format!("{}() takes {takes} but {given}", name.id)));
            }
            slots.resize(count, absent());
            slots.push(if in_task { absent() } else { self.active() });
        }
        let callee = match callee.expr {
            Expr::Variable(name) => name,
            _ => match self.pin(callee).expr {
                Expr::Variable(name) => name,
                _ => self.temp(),
            },
        };
        let args = slots.into_iter().map(|slot| slot.expr).collect();
        Ok(Ok(Prepared { callee, args }))
    }

    fn call_function(&mut self, call: &ast::ExprCall, awaited: bool) -> Lowering<Operand> {
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
                kind: ActKind::Call(Callee::Value(prepared.callee)),
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
        self.no_stars(&call.arguments)?;
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
                let tasks = self.gather_members(call)?;
                self.invoke("py.gather", &[tasks], Ty::List)
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
    fn gather_members(&mut self, call: &ast::ExprCall) -> Lowering<Operand> {
        if let Some(keyword) = call.arguments.keywords.first() {
            return Err(diagnostics::with_repair(
                Code::AsyncUnsupported,
                "asyncio.gather takes no keyword arguments here",
                keyword.range,
                "catch each coroutine's exception inside it",
            ));
        }
        if let [PyExpr::Starred(starred)] = &*call.arguments.args {
            let tasks = self.expr(&starred.value)?;
            return self.invoke("py.to_list", &[tasks], Ty::List);
        }
        let mut members = Vec::with_capacity(call.arguments.args.len());
        for member in &call.arguments.args {
            let task = match member {
                PyExpr::Starred(starred) => {
                    return Err(diagnostics::with_repair(
                        Code::StarUnsupported,
                        "asyncio.gather takes one `*tasks` or the awaitables themselves",
                        starred.range,
                        format!(
                            "put the tasks passed to `{}` in one list, then pass that list with `*`",
                            self.text(call.func.range())
                        ),
                    ));
                }
                PyExpr::Call(_) => self.spawn(member)?,
                other => {
                    let task = self.expr(other)?;
                    self.pin(task)
                }
            };
            members.push(task.expr);
        }
        Ok(self.let_rhs(Rhs::Expr(Expr::List(members)), Ty::List))
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
                let tasks = self.gather_members(call)?;
                ActExpr {
                    kind: ActKind::Call(self.helper_callee("py.gather")?),
                    args: vec![tasks.expr],
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
                    self.no_stars(&call.arguments)?;
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
                    // The task reads these when it starts: pin whatever
                    // the caller could still change.
                    let callee = self.pin(Operand::inline(
                        Expr::Variable(prepared.callee),
                        Ty::Unknown,
                    ));
                    let Expr::Variable(callee) = callee.expr else {
                        return Err(refusal(call.range()));
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
                        kind: ActKind::Call(Callee::Value(callee)),
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

    /// The arguments of a built-in whose later parameters may be passed
    /// under the names `keywords`, absent where the call gives none.
    fn with_keywords(
        &mut self,
        name: &str,
        call: &ast::ExprCall,
        min: usize,
        keywords: &[&str],
    ) -> Lowering<Vec<Operand>> {
        let positional = call.arguments.args.len();
        let max = min + keywords.len();
        if positional < min || positional > max {
            return Err(arguments_error(
                format!("`{name}` takes {min} to {max} arguments, and {positional} were given"),
                call.range(),
            ));
        }
        let Given {
            positional: given,
            keywords: keyword_values,
            ..
        } = self.given(None, &call.arguments)?;
        let mut slots: Vec<Option<Operand>> = given.into_iter().map(Some).collect();
        slots.resize(max, None);
        for (keyword, value) in call.arguments.keywords.iter().zip(keyword_values) {
            let keyword_name = keyword.arg.as_ref().map_or("", |name| name.id.as_str());
            let Some(index) = keywords.iter().position(|known| *known == keyword_name) else {
                return Err(arguments_error(
                    format!("`{name}` takes no keyword argument `{keyword_name}` here"),
                    keyword.range,
                ));
            };
            if slots[min + index].is_some() {
                return Err(arguments_error(
                    format!("`{name}` got multiple values for `{keyword_name}`"),
                    keyword.range,
                ));
            }
            slots[min + index] = Some(value);
        }
        while slots.len() > min && matches!(slots.last(), Some(None)) {
            slots.pop();
        }
        Ok(slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(absent))
            .collect())
    }

    fn builtin(&mut self, name: &str, call: &ast::ExprCall) -> Lowering<Operand> {
        let simple = |helper: &'static str, min: usize, max: usize, ty: Ty| (helper, min, max, ty);
        let (helper, min, max, ty) = match name {
            "print" => {
                let mut operands =
                    self.with_keywords(name, call, call.arguments.args.len(), &["sep"])?;
                let sep = if operands.len() > call.arguments.args.len() {
                    operands.pop().unwrap_or_else(|| Operand::text(" "))
                } else {
                    Operand::text(" ")
                };
                let args = Self::tuple_of(&operands);
                let line = self.invoke("py.show", &[args, sep], Ty::Str)?;
                self.emit(Stmt::Print { value: line.expr });
                return Ok(Operand::none());
            }
            "str" => {
                let operands = self.positional(name, call, 0, 1)?;
                return match operands.into_iter().next() {
                    None => Ok(Operand::text("")),
                    Some(value) if value.ty == Ty::Str => Ok(value),
                    Some(value) if value.ty == Ty::Int => {
                        let text = self.native("num.to_text", vec![value.expr])?;
                        Ok(Operand::inline(text, Ty::Str))
                    }
                    Some(value) => self.invoke("py.str", &[value], Ty::Str),
                };
            }
            "bool" => {
                let operands = self.positional(name, call, 0, 1)?;
                return match operands.into_iter().next() {
                    None => Ok(Operand::literal(Literal::Bool(false), Ty::Bool)),
                    Some(value) => Ok(Operand::inline(self.truth(value)?, Ty::Bool)),
                };
            }
            "int" | "float" if call.arguments.is_empty() => {
                return Ok(if name == "int" {
                    Operand::literal(Literal::Int(0.into()), Ty::Int)
                } else {
                    Operand::literal(Literal::Float(0.0.into()), Ty::Float)
                });
            }
            "min" | "max" => {
                let operands = self.positional(name, call, 1, usize::MAX)?;
                let args = Self::tuple_of(&operands);
                let largest = Operand::literal(Literal::Bool(name == "max"), Ty::Bool);
                return self.invoke("py.extreme", &[args, largest], Ty::Unknown);
            }
            "sorted" => {
                let operands = self.with_keywords(name, call, 1, &["key", "reverse"])?;
                if call.arguments.args.len() > 1 {
                    return Err(arguments_error(
                        "`sorted` takes `key` and `reverse` by keyword",
                        call.range(),
                    ));
                }
                return self.invoke("py.sorted", &operands, Ty::List);
            }
            "enumerate" => {
                let operands = self.with_keywords(name, call, 1, &["start"])?;
                return self.invoke("py.enumerate", &operands, Ty::List);
            }
            "zip" => {
                let operands = self.positional(name, call, 0, usize::MAX)?;
                let sequences = Self::tuple_of(&operands);
                return self.invoke("py.zip", &[sequences], Ty::List);
            }
            "list" | "tuple" | "set" | "dict" if call.arguments.is_empty() => {
                return Ok(match name {
                    "list" => self.let_rhs(Rhs::Expr(Expr::List(Vec::new())), Ty::List),
                    "set" => self.let_rhs(Rhs::Expr(Expr::Set(Vec::new())), Ty::Set),
                    "dict" => self.let_rhs(Rhs::Expr(Expr::Map(Vec::new())), Ty::Dict),
                    _ => Operand::inline(Expr::Tuple(Vec::new()), Ty::Tuple),
                });
            }
            "isinstance" => return self.isinstance(call),
            "type" => {
                return Err(diagnostics::with_repair(
                    Code::BuiltinUnsupported,
                    "a type is not a value in the dialect",
                    call.range(),
                    self.type_repair(call),
                ));
            }
            "len" => simple("py.length", 1, 1, Ty::Int),
            "repr" => simple("py.repr", 1, 1, Ty::Str),
            "int" => simple("py.int", 1, 2, Ty::Int),
            "float" => simple("py.float", 1, 1, Ty::Float),
            "abs" => simple("py.abs", 1, 1, Ty::Unknown),
            "sum" => simple("py.sum", 1, 2, Ty::Unknown),
            "list" => simple("py.to_list", 1, 1, Ty::List),
            "tuple" => simple("py.to_tuple", 1, 1, Ty::Tuple),
            "set" => simple("py.to_set", 1, 1, Ty::Set),
            "dict" => simple("py.to_dict", 1, 1, Ty::Dict),
            "range" => simple("py.range", 1, 3, Ty::List),
            "reversed" => simple("py.reversed", 1, 1, Ty::List),
            "any" => simple("py.any", 1, 1, Ty::Bool),
            "all" => simple("py.all", 1, 1, Ty::Bool),
            "round" => simple("py.round", 1, 2, Ty::Unknown),
            "ord" => simple("py.ord", 1, 1, Ty::Int),
            "chr" => simple("py.chr", 1, 1, Ty::Str),
            "divmod" => simple("py.divmod", 2, 2, Ty::Tuple),
            other => {
                return Err(diagnostics::with_repair(
                    Code::BuiltinUnsupported,
                    format!("the built-in `{other}` is not in the dialect"),
                    call.range(),
                    "see the dialect's README for the built-ins it has",
                ));
            }
        };
        let operands = self.positional(name, call, min, max)?;
        self.invoke(helper, &operands, ty)
    }

    /// `isinstance(value, type)` against built-in types named in place.
    fn isinstance(&mut self, call: &ast::ExprCall) -> Lowering<Operand> {
        self.plain_arguments(&call.arguments, "isinstance")?;
        let [value, types] = &*call.arguments.args else {
            return Err(arguments_error(
                "`isinstance` takes a value and a type",
                call.range(),
            ));
        };
        let listed: Vec<&PyExpr> = match types {
            PyExpr::Tuple(tuple) => tuple.elts.iter().collect(),
            single => vec![single],
        };
        let mut names = Vec::with_capacity(listed.len());
        for ty in listed {
            let known = match ty {
                PyExpr::Name(name) if self.variable(name.id.as_str()).is_none() => {
                    match name.id.as_str() {
                        "int" | "float" | "str" | "bool" | "list" | "dict" | "set" | "tuple" => {
                            Some(name.id.as_str())
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some(known) = known else {
                return Err(diagnostics::with_repair(
                    Code::BuiltinUnsupported,
                    "`isinstance` tests against int, float, str, bool, list, dict, set and tuple, named in place",
                    ty.range(),
                    format!(
                        "catch `{}` with `except` if it is an exception class; otherwise compare `type({}).__name__`",
                        self.text(ty.range()),
                        self.text(value.range())
                    ),
                ));
            };
            names.push(Expr::Literal(Literal::Text(known.to_string())));
        }
        let value = self.expr(value)?;
        let names = Operand::inline(Expr::Tuple(names), Ty::Tuple);
        self.invoke("py.isinstance", &[value, names], Ty::Bool)
    }
}

pub(super) fn arguments_error(
    message: impl Into<String>,
    range: TextRange,
) -> lash_kernel_dialect::Diagnostic {
    diagnostics::diagnostic(Code::Arguments, message, range)
}
