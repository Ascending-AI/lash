//! Calls: functions, tools, built-ins, methods and asyncio.

use lash_kernel_doc::{
    Action, Atom, Callee, EffectName, Expr, Literal, Name, Rhs, Signature, Stmt,
};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};

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

/// A method: its name, the fewest and the most arguments it takes, and the
/// names its arguments may be passed under, in order.
struct Method {
    name: &'static str,
    min: usize,
    max: usize,
    keywords: &'static [&'static str],
    result: Ty,
}

const fn method(name: &'static str, min: usize, max: usize, result: Ty) -> Method {
    Method {
        name,
        min,
        max,
        keywords: &[],
        result,
    }
}

const METHODS: &[Method] = &[
    method("append", 1, 1, Ty::None),
    method("extend", 1, 1, Ty::None),
    method("insert", 2, 2, Ty::None),
    method("pop", 0, 2, Ty::Unknown),
    method("remove", 1, 1, Ty::None),
    method("index", 1, 1, Ty::Int),
    method("count", 1, 1, Ty::Int),
    Method {
        name: "sort",
        min: 0,
        max: 2,
        keywords: &["key", "reverse"],
        result: Ty::None,
    },
    method("reverse", 0, 0, Ty::None),
    method("copy", 0, 0, Ty::Unknown),
    method("clear", 0, 0, Ty::None),
    method("get", 1, 2, Ty::Unknown),
    method("keys", 0, 0, Ty::List),
    method("values", 0, 0, Ty::List),
    method("items", 0, 0, Ty::List),
    method("setdefault", 1, 2, Ty::Unknown),
    method("update", 1, 1, Ty::None),
    method("add", 1, 1, Ty::None),
    method("discard", 1, 1, Ty::None),
    method("union", 1, 1, Ty::Set),
    method("intersection", 1, 1, Ty::Set),
    method("difference", 1, 1, Ty::Set),
    method("join", 1, 1, Ty::Str),
    method("split", 0, 1, Ty::List),
    method("strip", 0, 0, Ty::Str),
    method("lstrip", 0, 0, Ty::Str),
    method("rstrip", 0, 0, Ty::Str),
    method("upper", 0, 0, Ty::Str),
    method("lower", 0, 0, Ty::Str),
    method("startswith", 1, 1, Ty::Bool),
    method("endswith", 1, 1, Ty::Bool),
    method("replace", 2, 2, Ty::Str),
    method("find", 1, 1, Ty::Int),
    method("cancel", 0, 0, Ty::Bool),
];

/// The names of the methods the dialect has.
fn method_names() -> Vec<&'static str> {
    METHODS.iter().map(|method| method.name).collect()
}

fn absent() -> Operand {
    Operand::literal(Literal::Absent, Ty::Unknown)
}

/// A call of a Python function, ready to be made: the variable that holds
/// the callee and the arguments, all evaluated.
struct Prepared {
    callee: Name,
    args: Vec<Atom>,
}

impl Lowerer<'_> {
    pub(super) fn is_asyncio(&self, expr: &PyExpr) -> bool {
        matches!(expr, PyExpr::Name(name) if name.id.as_str() == "asyncio")
            && self.variable("asyncio").is_none()
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

    /// The tool a call names, if it names one.
    fn effect(&self, call: &ast::ExprCall) -> Option<(EffectName, Signature)> {
        let PyExpr::Name(name) = call.func.as_ref() else {
            return None;
        };
        let id = name.id.as_str();
        if self.variable(id).is_some() {
            return None;
        }
        self.effects
            .iter()
            .find(|(effect, _)| effect.as_str() == id)
            .map(|(effect, signature)| (effect.clone(), signature.clone()))
    }

    /// Refuses `*args` and `**kwargs` at a call.
    fn no_stars(&self, arguments: &ast::Arguments) -> Lowering<()> {
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
        match call.func.as_ref() {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                if self.variable(id).is_some() {
                    return self.call_function(call, awaited);
                }
                if let Some((effect, signature)) = self.effect(call) {
                    if !awaited {
                        return Err(diagnostics::with_repair(
                            Code::CoroutineNotAwaited,
                            format!("the tool `{id}` is called without `await`"),
                            call.range(),
                            format!(
                                "write `await {}`, or `asyncio.create_task({})` to run it beside this code",
                                self.text(call.range()),
                                self.text(call.range())
                            ),
                        ));
                    }
                    let action = self.perform(&effect, &signature, call)?;
                    return Ok(self.let_rhs(Rhs::Action(action), Ty::Unknown));
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
            PyExpr::Attribute(attribute) => self.method(attribute, call),
            _ => self.call_function(call, awaited),
        }
    }

    /// Evaluates the callee and the arguments of a call of a Python
    /// function, in order, and places the arguments by the parameters
    /// where keywords need it.
    ///
    /// The inner error is the message of the TypeError a call Python would
    /// refuse raises when it runs.
    fn prepare(
        &mut self,
        call: &ast::ExprCall,
        awaited: bool,
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
        let positional = call.arguments.args.len();
        let mut exprs: Vec<&PyExpr> = vec![&call.func];
        exprs.extend(call.arguments.args.iter());
        exprs.extend(call.arguments.keywords.iter().map(|keyword| &keyword.value));
        let mut operands = self.operands(&exprs)?;
        let keyword_values = operands.split_off(1 + positional);
        let mut slots: Vec<Option<Operand>> = operands.drain(1..).map(Some).collect();
        let callee = operands.pop().unwrap_or_else(Operand::none);
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
                match signature.params.iter().position(|param| param.name == name) {
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
        let slots: Vec<Operand> = slots
            .into_iter()
            .map(|slot| slot.unwrap_or_else(absent))
            .collect();
        let callee = match callee.expr {
            Expr::Variable(name) => name,
            _ => match self.pin(callee).expr {
                Expr::Variable(name) => name,
                _ => self.temp(),
            },
        };
        let args = self.atoms(&slots);
        Ok(Ok(Prepared { callee, args }))
    }

    fn call_function(&mut self, call: &ast::ExprCall, awaited: bool) -> Lowering<Operand> {
        let prepared = match self.prepare(call, awaited)? {
            Ok(prepared) => prepared,
            Err(problem) => {
                self.invoke_do(
                    "py.fail",
                    &[Operand::text("TypeError"), Operand::text(problem)],
                )?;
                return Ok(Operand::none());
            }
        };
        Ok(self.let_rhs(
            Rhs::Action(Action::Call {
                callee: Callee::Value(prepared.callee),
                args: prepared.args,
            }),
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
    ) -> Lowering<Action> {
        self.no_stars(&call.arguments)?;
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
        let mut exprs: Vec<&PyExpr> = call.arguments.args.iter().collect();
        exprs.extend(call.arguments.keywords.iter().map(|keyword| &keyword.value));
        let mut operands = self.operands(&exprs)?;
        let keyword_values = operands.split_off(positional);
        let mut slots: Vec<Option<Operand>> = operands.into_iter().map(Some).collect();
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
                Some(operand) => self.pin(operand),
                None => absent(),
            })
            .collect();
        self.performed.insert(effect.clone(), signature.clone());
        Ok(Action::Perform {
            effect: effect.clone(),
            args: self.atoms(&slots),
            result: signature.result.clone(),
        })
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

    /// `await value`.
    fn awaited(&mut self, value: &PyExpr) -> Lowering<Operand> {
        let PyExpr::Call(call) = value else {
            let value = self.expr(value)?;
            return self.invoke("py.await", &[value], Ty::Unknown);
        };
        // An awaited coroutine call is an ordinary call, and an awaited
        // tool call is one `perform`.
        let direct = match call.func.as_ref() {
            PyExpr::Name(name) => {
                self.effect(call).is_some()
                    || self
                        .known_def(name.id.as_str())
                        .is_some_and(|signature| signature.is_async)
            }
            _ => matches!(self.asyncio_function(call), Some("sleep" | "gather")),
        };
        let value = self.call(call, true)?;
        if direct {
            return Ok(value);
        }
        self.invoke("py.await", &[value], Ty::Unknown)
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
                let action = match sleep_literal(seconds) {
                    // A sleep of zero lets the other ready tasks run.
                    Some(None) => Action::Yield,
                    Some(Some(duration)) => Action::Sleep {
                        duration: Atom::Literal(duration),
                    },
                    None => {
                        let seconds = self.expr(seconds)?;
                        self.invoke_do("py.sleep", &[seconds])?;
                        return Ok(Operand::none());
                    }
                };
                self.emit(Stmt::Do { action });
                Ok(Operand::none())
            }
            "gather" => {
                if !awaited {
                    return Err(not_awaited(call.range()));
                }
                let tasks = self.gather_members(call)?;
                self.invoke("py.gather", &[tasks], Ty::List)
            }
            "create_task" | "ensure_future" => {
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
                Action::Call {
                    callee: Callee::Library(self.function("py.sleep")?),
                    args: self.atoms(&[seconds]),
                }
            }
            Some("gather") => {
                let tasks = self.gather_members(call)?;
                Action::Call {
                    callee: Callee::Library(self.function("py.gather")?),
                    args: self.atoms(&[tasks]),
                }
            }
            Some(_) => return Err(refusal(call.range())),
            None => {
                if let Some((effect, signature)) = self.effect(call) {
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
                    let prepared = match self.prepare(call, true)? {
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
                            Atom::Variable(name) => {
                                let pinned =
                                    self.pin(Operand::inline(Expr::Variable(name), Ty::Unknown));
                                match pinned.expr {
                                    Expr::Variable(name) => Atom::Variable(name),
                                    _ => Atom::Literal(Literal::Null),
                                }
                            }
                            literal @ Atom::Literal(_) => literal,
                        })
                        .collect();
                    Action::Call {
                        callee: Callee::Value(callee),
                        args,
                    }
                }
            }
        };
        let body = self.block(|this| {
            let result = this.let_rhs(Rhs::Action(action), Ty::Unknown);
            this.emit(Stmt::Return { value: result.expr });
            Ok(())
        })?;
        let work = self.emit_closure(Vec::new(), body);
        let run = self.function("py.task.run")?;
        let args = self.atoms(&[work]);
        Ok(self.let_rhs(
            Rhs::Action(Action::Spawn {
                callee: Callee::Library(run),
                args,
            }),
            Ty::Unknown,
        ))
    }

    fn method(
        &mut self,
        attribute: &ast::ExprAttribute,
        call: &ast::ExprCall,
    ) -> Lowering<Operand> {
        let name = attribute.attr.id.as_str();
        let Some(method) = METHODS.iter().find(|method| method.name == name) else {
            let repair = match name {
                "format" => self.format_repair(call),
                _ => format!(
                    "replace `{}` with a supported method of `{}`; the dialect's methods are: {}",
                    self.text(attribute.range),
                    self.text(attribute.value.range()),
                    method_names().join(", ")
                ),
            };
            return Err(diagnostics::with_repair(
                Code::MethodUnsupported,
                format!("the method `{name}` is not in the dialect"),
                attribute.range,
                &repair,
            ));
        };
        let positional = call.arguments.args.len();
        let mut exprs: Vec<&PyExpr> = vec![&attribute.value];
        exprs.extend(call.arguments.args.iter());
        exprs.extend(call.arguments.keywords.iter().map(|keyword| &keyword.value));
        let mut operands = self.operands(&exprs)?;
        let keyword_values = operands.split_off(1 + positional);
        let mut slots: Vec<Option<Operand>> = operands.drain(1..).map(Some).collect();
        let receiver = operands.pop().unwrap_or_else(Operand::none);
        for (keyword, value) in call.arguments.keywords.iter().zip(keyword_values) {
            let keyword_name = keyword.arg.as_ref().map_or("", |name| name.id.as_str());
            let Some(index) = method
                .keywords
                .iter()
                .position(|known| *known == keyword_name)
            else {
                return Err(arguments_error(
                    format!("`{name}` takes no keyword argument `{keyword_name}`"),
                    keyword.range,
                ));
            };
            if slots.len() <= index {
                slots.resize(index + 1, None);
            }
            if slots[index].is_some() {
                return Err(arguments_error(
                    format!("`{name}` got multiple values for `{keyword_name}`"),
                    keyword.range,
                ));
            }
            slots[index] = Some(value);
        }
        if slots.len() < method.min || slots.len() > method.max {
            return Err(arguments_error(
                format!(
                    "`{name}` takes {} to {} arguments here, and {} were given",
                    method.min,
                    method.max,
                    slots.len()
                ),
                call.range(),
            ));
        }
        let mut all = vec![receiver];
        all.extend(slots.into_iter().map(|slot| slot.unwrap_or_else(absent)));
        self.invoke(&format!("py.method.{name}"), &all, method.result)
    }

    /// The positional arguments of a built-in that takes between `min`
    /// and `max` of them and no keywords.
    fn positional(
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
        let mut exprs: Vec<&PyExpr> = call.arguments.args.iter().collect();
        exprs.extend(call.arguments.keywords.iter().map(|keyword| &keyword.value));
        let mut operands = self.operands(&exprs)?;
        let keyword_values = operands.split_off(positional);
        let mut slots: Vec<Option<Operand>> = operands.into_iter().map(Some).collect();
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

fn arguments_error(
    message: impl Into<String>,
    range: TextRange,
) -> lash_kernel_dialect::Diagnostic {
    diagnostics::diagnostic(Code::Arguments, message, range)
}

/// A literal sleep: `Some(None)` for zero, `Some(Some(ms))` for a positive
/// number of seconds as milliseconds, and `None` when the source computes
/// it.
fn sleep_literal(seconds: &PyExpr) -> Option<Option<Literal>> {
    let PyExpr::NumberLiteral(literal) = seconds else {
        return None;
    };
    match &literal.value {
        ast::Number::Int(value) => {
            let seconds = value.as_u64()?;
            if seconds == 0 {
                return Some(None);
            }
            let milliseconds = i64::try_from(seconds.checked_mul(1000)?).ok()?;
            Some(Some(Literal::Int(milliseconds.into())))
        }
        ast::Number::Float(value) => {
            if *value == 0.0 {
                return Some(None);
            }
            (value.is_finite() && *value > 0.0)
                .then(|| Some(Literal::Float((*value * 1000.0).into())))
        }
        ast::Number::Complex { .. } => None,
    }
}
