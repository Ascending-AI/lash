//! The workflow lens's canonical TypeScript printer.
//!
//! TypeScript is the only cell language, so the lens's canonical text is
//! TypeScript: this module turns a lowered [`Program`] — or one expression of
//! it — back into source a user would have authored.
//!
//! Printing is not a straight walk of the IR. The lowerer desugars the authored
//! surface, so the printer re-sugars the shapes it generates before falling
//! back to the structural spelling:
//!
//! * `ProcessDecl { body: Try(Finish(Call(Function))) }` with the generated
//!   `__typescript_process_error` catch prints back as `defineProcess({ name,
//!   signals, run: async (..) => { .. } })`.
//! * `Print(__typescript_stdlib("__consoleObservationText", x))` prints back as
//!   `console.log(x)`.
//! * `__typescript_await_array([..], false)` prints back as
//!   `await Promise.all([..])`, and the settled flavour as
//!   `await Promise.allSettled([..])`.
//! * The array-callback driver block — generated `__typescript_N_callback_*`
//!   bindings around a `__typescript_stdlib("__singleCallbackResult", Map { .. })`
//!   tail — prints back as `receiver.map(fn)` / `receiver.filter(fn)`.
//!
//! A generated `__typescript_*` binding that reaches the printer without being
//! re-sugared is a defect, not a rendering choice: it has no authored spelling,
//! so it is refused with [`TypeScriptSourceError::GeneratedBinding`] rather than
//! surfaced to a user.

use lashlang::{
    AssignPathStep, AssignTarget, BinaryOp, CatchClause, Declaration, Expr, FunctionDecl,
    FunctionExpr, JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, ProcessDecl,
    ProcessSignalDecl, Program, ResourceRefExpr, TryExpr, TypeExpr, UnaryOp,
};
use thiserror::Error;

use crate::GENERATED_BINDING_PREFIX;

/// Error returned when canonical IR has no TypeScript spelling.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeScriptSourceError {
    #[error("cannot render {kind} as TypeScript")]
    Unrepresentable { kind: &'static str },
    #[error("generated binding `{name}` has no authored TypeScript spelling")]
    GeneratedBinding { name: String },
    #[error("invalid {context} identifier `{name}`")]
    InvalidIdentifier { context: &'static str, name: String },
    #[error("cannot render number literal `{value}` as TypeScript")]
    UnsupportedNumber { value: String },
    #[error("cannot render host descriptor constructor `{type_name}` without a constructor path")]
    UnknownHostDescriptorConstructor { type_name: String },
}

type Printed = Result<String, TypeScriptSourceError>;

/// The suffix the process wrapper's generated catch binding carries.
const PROCESS_ERROR_SUFFIX: &str = "process_error";

/// Print a lowered program as a canonical TypeScript module.
pub fn typescript_program_source(program: &Program) -> Printed {
    Printer.program(program)
}

/// Print one expression as canonical TypeScript.
///
/// This is the textual form carried by editable workflow-graph node fields.
pub fn typescript_expression_source(expression: &Expr) -> Printed {
    Printer.statement_expression(expression)
}

/// Print one assignment target as canonical TypeScript.
pub fn typescript_assign_target_source(target: &AssignTarget) -> Printed {
    Printer.assign_target(target)
}

struct Printer;

impl Printer {
    fn program(&self, program: &Program) -> Printed {
        let mut out = String::new();
        let processes = program
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                Declaration::Process(process) => Some(process),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut bound = Vec::new();

        for declaration in &program.declarations {
            match declaration {
                // A process declaration is printed where `main` binds it, so the
                // authored `const <binding> = defineProcess(..)` keeps its name.
                Declaration::Process(_) => {}
                Declaration::Type(_) => {
                    return Err(TypeScriptSourceError::Unrepresentable {
                        kind: "a type declaration",
                    });
                }
                Declaration::Function(function) => {
                    out.push_str(&self.function_declaration(function)?);
                    bound.push(function.name.to_string());
                }
            }
        }

        let statements = match &program.main {
            Expr::Block(statements) => statements.as_slice(),
            statement => std::slice::from_ref(statement),
        };
        let mut emitted_processes = Vec::new();
        for statement in statements {
            if let Expr::Assign { target, expr } = statement
                && target.is_simple()
                && let Expr::ProcessRef { process } = expr.as_ref()
                && let Some(declaration) = processes
                    .iter()
                    .find(|candidate| candidate.name == *process)
                && !emitted_processes.contains(&process.to_string())
            {
                emitted_processes.push(process.to_string());
                out.push_str(&self.define_process(
                    target.root.as_str(),
                    declaration,
                    &mut bound,
                )?);
                continue;
            }
            out.push_str(&self.statement(statement, 0, &mut bound)?);
        }

        for process in &processes {
            if !emitted_processes.contains(&process.name.to_string()) {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a process declaration with no binding in the module body",
                });
            }
        }
        Ok(out)
    }

    fn function_declaration(&self, function: &FunctionDecl) -> Printed {
        let params = function
            .params
            .iter()
            .map(|param| self.identifier("function parameter", param.name.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = format!(
            "function {}(",
            self.identifier("function", function.name.as_str())?
        );
        out.push_str(&params.join(", "));
        out.push_str(") ");
        out.push_str(&self.block(&function.body, 0, &mut Vec::new())?);
        out.push('\n');
        Ok(out)
    }

    /// Re-sugar a lowered process declaration into its authored `defineProcess`.
    fn define_process(
        &self,
        binding: &str,
        process: &ProcessDecl,
        bound: &mut Vec<String>,
    ) -> Printed {
        let body = process_run_body(process).ok_or(TypeScriptSourceError::Unrepresentable {
            kind: "a process body that is not the lowerer's `defineProcess` wrapper",
        })?;
        let params = process
            .params
            .iter()
            .map(|param| self.identifier("process parameter", param.name.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = format!(
            "const {} = defineProcess({{\n  name: {},\n  signals: {},\n  run: async ({}) => ",
            self.identifier("process binding", binding)?,
            string_literal(process.name.as_str()),
            self.signals(&process.signals)?,
            params.join(", "),
        );
        let mut run_bound = process
            .params
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>();
        out.push_str(&self.block(body, 1, &mut run_bound)?);
        out.push_str(",\n});\n");
        bound.push(binding.to_string());
        Ok(out)
    }

    fn signals(&self, signals: &[ProcessSignalDecl]) -> Printed {
        if signals.is_empty() {
            return Ok("{}".to_string());
        }
        let entries = signals
            .iter()
            .map(|signal| {
                Ok(format!(
                    "{}: {}",
                    key(signal.name.as_str()),
                    self.ty(&signal.ty)?
                ))
            })
            .collect::<Result<Vec<_>, TypeScriptSourceError>>()?;
        Ok(format!("{{ {} }}", entries.join(", ")))
    }

    /// The signal-schema literal the dialect accepts, which is a value, not a
    /// TypeScript type annotation.
    fn ty(&self, ty: &TypeExpr) -> Printed {
        match ty {
            TypeExpr::Any | TypeExpr::Null => Ok("null".to_string()),
            TypeExpr::Str => Ok("\"\"".to_string()),
            TypeExpr::Int | TypeExpr::Float => Ok("0".to_string()),
            TypeExpr::Bool => Ok("false".to_string()),
            _ => Err(TypeScriptSourceError::Unrepresentable {
                kind: "this signal schema",
            }),
        }
    }

    fn block(&self, expression: &Expr, level: usize, bound: &mut Vec<String>) -> Printed {
        let statements = match expression {
            Expr::Block(statements) => statements.as_slice(),
            statement => std::slice::from_ref(statement),
        };
        if statements.is_empty() {
            return Ok("{}".to_string());
        }
        let mut out = String::from("{\n");
        for statement in statements {
            out.push_str(&self.statement(statement, level + 1, bound)?);
        }
        out.push_str(&indent(level));
        out.push('}');
        Ok(out)
    }

    fn statement(&self, expression: &Expr, level: usize, bound: &mut Vec<String>) -> Printed {
        let prefix = indent(level);
        match expression {
            // The lowerer's unit completion value. It is not something a user
            // wrote, and `undefined;` is not a statement worth showing.
            Expr::Undefined => Ok(String::new()),
            Expr::LabelAnnotated { expr, .. } => {
                // `@label(title:)` has no TypeScript form (FIG-3047): the label
                // travels on the graph node, and the closest TypeScript is the
                // annotated statement itself.
                self.statement(expr, level, bound)
            }
            Expr::Block(_) => {
                let mut inner = bound.clone();
                Ok(format!(
                    "{prefix}{}\n",
                    self.block(expression, level, &mut inner)?
                ))
            }
            Expr::Assign { target, expr } => {
                let rendered = format!(
                    "{} = {};\n",
                    self.assign_target(target)?,
                    self.expression(expr)?
                );
                if target.is_simple() && !bound.contains(&target.root.to_string()) {
                    bound.push(target.root.to_string());
                    return Ok(format!("{prefix}let {rendered}"));
                }
                Ok(format!("{prefix}{rendered}"))
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } if matches!(then_block.as_ref(), Expr::Block(_)) => {
                let mut out = format!("{prefix}if ({}) ", self.expression(condition)?);
                let mut then_bound = bound.clone();
                out.push_str(&self.block(then_block, level, &mut then_bound)?);
                match else_block.as_ref() {
                    Expr::Undefined => {}
                    Expr::Block(statements) if statements.is_empty() => {}
                    other => {
                        let mut else_bound = bound.clone();
                        out.push_str(" else ");
                        out.push_str(&self.block(other, level, &mut else_bound)?);
                    }
                }
                out.push('\n');
                Ok(out)
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                let mut body_bound = bound.clone();
                body_bound.push(binding.to_string());
                Ok(format!(
                    "{prefix}for (const {} of {}) {}\n",
                    self.identifier("loop binding", binding.as_str())?,
                    self.expression(iterable)?,
                    self.block(body, level, &mut body_bound)?
                ))
            }
            Expr::While { condition, body } => {
                let mut body_bound = bound.clone();
                Ok(format!(
                    "{prefix}while ({}) {}\n",
                    self.expression(condition)?,
                    self.block(body, level, &mut body_bound)?
                ))
            }
            Expr::Try(try_expr) => {
                let mut out = format!("{prefix}try ");
                let mut body_bound = bound.clone();
                out.push_str(&self.block(&try_expr.body, level, &mut body_bound)?);
                if let Some(catch) = &try_expr.catch {
                    let mut catch_bound = bound.clone();
                    catch_bound.push(catch.binding.to_string());
                    out.push_str(&format!(
                        " catch ({}) {}",
                        self.identifier("catch binding", catch.binding.as_str())?,
                        self.block(&catch.body, level, &mut catch_bound)?
                    ));
                }
                if let Some(finally) = &try_expr.finally {
                    let mut finally_bound = bound.clone();
                    out.push_str(&format!(
                        " finally {}",
                        self.block(finally, level, &mut finally_bound)?
                    ));
                }
                out.push('\n');
                Ok(out)
            }
            Expr::Throw(value) => Ok(format!("{prefix}throw {};\n", self.expression(value)?)),
            Expr::Return(value) => match value.as_ref() {
                Expr::Undefined => Ok(format!("{prefix}return;\n")),
                value => Ok(format!("{prefix}return {};\n", self.expression(value)?)),
            },
            Expr::Break => Ok(format!("{prefix}break;\n")),
            Expr::Continue => Ok(format!("{prefix}continue;\n")),
            expression => Ok(format!(
                "{prefix}{};\n",
                self.statement_expression(expression)?
            )),
        }
    }

    /// One expression in statement position, with no trailing semicolon.
    fn statement_expression(&self, expression: &Expr) -> Printed {
        match expression {
            Expr::Assign { target, expr } => Ok(format!(
                "{} = {}",
                self.assign_target(target)?,
                self.expression(expr)?
            )),
            Expr::Break => Ok("break".to_string()),
            Expr::Continue => Ok("continue".to_string()),
            expression => self.expression(expression),
        }
    }

    fn expression(&self, expression: &Expr) -> Printed {
        if let Some(sugared) = self.sugar(expression)? {
            return Ok(sugared);
        }
        match expression {
            Expr::Null => Ok("null".to_string()),
            Expr::Undefined => Ok("undefined".to_string()),
            Expr::Bool(value) => Ok(value.to_string()),
            Expr::Number(value) => number_literal(*value),
            Expr::String(value) => Ok(string_literal(value.as_str())),
            Expr::Variable(name) => self.identifier("variable", name.as_str()),
            Expr::List(items) => {
                let items = items
                    .iter()
                    .map(|item| self.expression(item))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("[{}]", items.join(", ")))
            }
            Expr::Record(entries) => {
                let entries = entries
                    .iter()
                    .map(|(name, value)| {
                        Ok(format!(
                            "{}: {}",
                            key(name.as_str()),
                            self.expression(value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, TypeScriptSourceError>>()?;
                if entries.is_empty() {
                    return Ok("{}".to_string());
                }
                Ok(format!("{{ {} }}", entries.join(", ")))
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => Ok(format!(
                "({} ? {} : {})",
                self.expression(condition)?,
                self.expression(then_block)?,
                self.expression(else_block)?
            )),
            Expr::StartProcess(start) => {
                let args = start
                    .args
                    .iter()
                    .map(|(name, value)| {
                        Ok(format!(
                            "{}: {}",
                            key(name.as_str()),
                            self.expression(value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, TypeScriptSourceError>>()?;
                let process = self.identifier("process", start.process.as_str())?;
                if args.is_empty() {
                    return Ok(format!("start({process})"));
                }
                Ok(format!("start({process}, {{ {} }})", args.join(", ")))
            }
            Expr::ProcessRef { process } => self.identifier("process", process.as_str()),
            Expr::ResourceRef(resource) => self.resource_ref(resource),
            Expr::HostDescriptorConstructor { type_name, .. } => {
                Err(TypeScriptSourceError::UnknownHostDescriptorConstructor {
                    type_name: type_name.to_string(),
                })
            }
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}.{}({})",
                    self.member_target(receiver)?,
                    self.identifier("operation", operation.as_str())?,
                    args.join(", ")
                ))
            }
            Expr::Await(value) => Ok(format!("await {}", self.unary_operand(value)?)),
            Expr::SleepFor(value) => Ok(format!("await sleep({})", self.expression(value)?)),
            Expr::SleepUntil(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "an absolute sleep deadline",
            }),
            Expr::WaitSignal { name } => Ok(format!(
                "await waitSignal({})",
                string_literal(name.as_str())
            )),
            Expr::SignalRun { run, name, payload } => Ok(format!(
                "wake({}, {}, {})",
                self.expression(run)?,
                string_literal(name.as_str()),
                self.expression(payload)?
            )),
            Expr::Cancel(value) => Ok(format!("cancel({})", self.expression(value)?)),
            Expr::Print(value) => Ok(format!("print({})", self.expression(value)?)),
            Expr::Yield(value) => Ok(format!("yield {}", self.expression(value)?)),
            Expr::Wake(value) => Ok(format!("wake({})", self.expression(value)?)),
            Expr::Finish(value) => Ok(format!("finish({})", self.expression(value)?)),
            Expr::Fail(value) => Ok(format!("fail({})", self.expression(value)?)),
            Expr::FunctionCall { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}({})",
                    self.identifier("function", function.as_str())?,
                    args.join(", ")
                ))
            }
            Expr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}({})",
                    self.member_target(function)?,
                    args.join(", ")
                ))
            }
            Expr::Function(function) => self.arrow(function),
            Expr::Field { target, field } => Ok(format!(
                "{}.{}",
                self.member_target(target)?,
                self.identifier("field", field.as_str())?
            )),
            Expr::Index { target, index } => Ok(format!(
                "{}[{}]",
                self.member_target(target)?,
                self.expression(index)?
            )),
            Expr::Unary { op, expr } => {
                let op = match op {
                    UnaryOp::Negate => "-",
                    UnaryOp::Not => "!",
                };
                Ok(format!("{op}{}", self.unary_operand(expr)?))
            }
            Expr::JavaScriptUnary { op, expr } => {
                let op = match op {
                    JavaScriptUnaryOp::Plus => "+",
                    JavaScriptUnaryOp::Negate => "-",
                    JavaScriptUnaryOp::Not => "!",
                    JavaScriptUnaryOp::TypeOf => "typeof ",
                };
                Ok(format!("{op}{}", self.unary_operand(expr)?))
            }
            Expr::Binary { left, op, right } => Ok(format!(
                "({} {} {})",
                self.expression(left)?,
                lash_binary_op(*op)?,
                self.expression(right)?
            )),
            Expr::JavaScriptBinary { left, op, right } => Ok(format!(
                "({} {} {})",
                self.expression(left)?,
                javascript_binary_op(*op),
                self.expression(right)?
            )),
            Expr::JavaScriptLogical { left, op, right } => Ok(format!(
                "({} {} {})",
                self.expression(left)?,
                match op {
                    JavaScriptLogicalOp::And => "&&",
                    JavaScriptLogicalOp::Or => "||",
                    JavaScriptLogicalOp::NullishCoalesce => "??",
                },
                self.expression(right)?
            )),
            Expr::BuiltinCall { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}({})",
                    self.identifier("builtin", name.as_str())?,
                    args.join(", ")
                ))
            }
            Expr::Block(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a block in expression position",
            }),
            Expr::LabelAnnotated { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a label-annotated expression",
            }),
            Expr::Assign { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "an assignment in expression position",
            }),
            Expr::For { .. } | Expr::While { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a loop in expression position",
            }),
            Expr::Break | Expr::Continue => Err(TypeScriptSourceError::Unrepresentable {
                kind: "loop control in expression position",
            }),
            Expr::Try(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "try/catch in expression position",
            }),
            Expr::Throw(_) | Expr::Return(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a jump in expression position",
            }),
            // The lens is a source-language projection: these shapes only reach
            // it from a producer other than the TypeScript front-end.
            Expr::Tuple(_) => Err(TypeScriptSourceError::Unrepresentable { kind: "a tuple" }),
            Expr::ListComprehension { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a list comprehension",
            }),
            Expr::ResultUnwrap(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a result unwrap",
            }),
            Expr::Map { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a bare map intrinsic",
            }),
            Expr::TypeLiteral(_) => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a type literal",
            }),
        }
    }

    /// Re-sugar one lowered shape, or `Ok(None)` if this is not one.
    fn sugar(&self, expression: &Expr) -> Result<Option<String>, TypeScriptSourceError> {
        if let Expr::Print(inner) = expression
            && let Some(args) = stdlib_call(inner, "__consoleObservationText")
            && let [value] = args
        {
            return Ok(Some(format!("console.log({})", self.expression(value)?)));
        }
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__typescript_await_array"
            && let [items, Expr::Bool(settled)] = args.as_slice()
        {
            let method = if *settled { "allSettled" } else { "all" };
            return Ok(Some(format!(
                "await Promise.{method}({})",
                self.expression(items)?
            )));
        }
        if let Some(sugared) = self.array_callback_sugar(expression)? {
            return Ok(Some(sugared));
        }
        Ok(None)
    }

    /// Re-sugar the array-callback driver block the lowerer generates for
    /// `receiver.map(fn)` and friends.
    ///
    /// The block is entirely generated: a `__typescript_N_callback_receiver`
    /// binding, a `__typescript_N_callback_function` binding, a worker closure,
    /// and a `__singleCallbackResult` tail that drives it. Nothing in it has an
    /// authored spelling except the receiver, the callback, and the method name
    /// the worker's shape identifies.
    fn array_callback_sugar(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Expr::Block(statements) = expression else {
            return Ok(None);
        };
        let Some((tail, setup)) = statements.split_last() else {
            return Ok(None);
        };
        let Some(args) = stdlib_call(tail, "__singleCallbackResult") else {
            return Ok(None);
        };
        let [Expr::Map { function, .. }] = args else {
            return Ok(None);
        };
        let Expr::Variable(worker) = function.as_ref() else {
            return Ok(None);
        };
        let mut receiver = None;
        let mut callback = None;
        let mut worker_body = None;
        for statement in setup {
            let Expr::Assign { target, expr } = statement else {
                return Ok(None);
            };
            let name = target.root.as_str();
            if name.ends_with("_callback_receiver") {
                receiver = Some(expr.as_ref());
            } else if name.ends_with("_callback_function") {
                callback = Some(expr.as_ref());
            } else if name == worker.as_str() {
                worker_body = Some(expr.as_ref());
            }
        }
        let (Some(receiver), Some(callback), Some(Expr::Function(worker))) =
            (receiver, callback, worker_body)
        else {
            return Ok(None);
        };
        let Some(method) = array_callback_method(&worker.body) else {
            return Ok(None);
        };
        Ok(Some(format!(
            "{}.{method}({})",
            self.member_target(receiver)?,
            self.expression(callback)?
        )))
    }

    fn arrow(&self, function: &FunctionExpr) -> Printed {
        let params = function
            .params
            .iter()
            .map(|param| self.identifier("arrow parameter", param.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut bound = function.params.iter().map(ToString::to_string).collect();
        Ok(format!(
            "({}) => {}",
            params.join(", "),
            self.block(&function.body, 0, &mut bound)?
        ))
    }

    fn member_target(&self, expression: &Expr) -> Printed {
        match expression {
            Expr::Null
            | Expr::Undefined
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::List(_)
            | Expr::Record(_)
            | Expr::ProcessRef { .. }
            | Expr::ResourceRef(_)
            | Expr::ReceiverCall { .. }
            | Expr::BuiltinCall { .. }
            | Expr::FunctionCall { .. }
            | Expr::Call { .. }
            | Expr::Field { .. }
            | Expr::Index { .. } => self.expression(expression),
            _ => Ok(format!("({})", self.expression(expression)?)),
        }
    }

    fn unary_operand(&self, expression: &Expr) -> Printed {
        match expression {
            Expr::Binary { .. }
            | Expr::JavaScriptBinary { .. }
            | Expr::JavaScriptLogical { .. }
            | Expr::If { .. } => self.expression(expression),
            _ => self.member_target(expression),
        }
    }

    fn resource_ref(&self, resource: &ResourceRefExpr) -> Printed {
        let path = if resource.path.is_empty() {
            resource
                .alias
                .split('.')
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        } else {
            resource
                .path
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        let Some((root, rest)) = path.split_first() else {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "an unnamed resource",
            });
        };
        let mut out = self.identifier("resource", root)?;
        for segment in rest {
            out.push('.');
            out.push_str(&self.identifier("resource", segment)?);
        }
        Ok(out)
    }

    fn assign_target(&self, target: &AssignTarget) -> Printed {
        let mut out = self.identifier("assignment target", target.root.as_str())?;
        for step in &target.steps {
            match step {
                AssignPathStep::Field(field) => {
                    out.push('.');
                    out.push_str(&self.identifier("field", field.as_str())?);
                }
                AssignPathStep::Index(index) => {
                    out.push('[');
                    out.push_str(&self.expression(index)?);
                    out.push(']');
                }
            }
        }
        Ok(out)
    }

    fn identifier(&self, context: &'static str, name: &str) -> Printed {
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            return Err(TypeScriptSourceError::GeneratedBinding {
                name: name.to_string(),
            });
        }
        if !is_typescript_identifier(name) {
            return Err(TypeScriptSourceError::InvalidIdentifier {
                context,
                name: name.to_string(),
            });
        }
        Ok(name.to_string())
    }
}

/// The authored `run` body inside the lowerer's process wrapper.
///
/// The wrapper is `Try { body: Finish(Call(Function)), catch: <generated> }`;
/// only its function body was authored, so that is what prints back.
pub(super) fn process_run_body(process: &ProcessDecl) -> Option<&Expr> {
    let Expr::Try(wrapper) = &process.body else {
        return None;
    };
    let TryExpr {
        body,
        catch: Some(CatchClause {
            binding,
            body: catch_body,
        }),
        finally: None,
    } = wrapper.as_ref()
    else {
        return None;
    };
    if !binding.starts_with(GENERATED_BINDING_PREFIX) || !binding.ends_with(PROCESS_ERROR_SUFFIX) {
        return None;
    }
    match catch_body.as_ref() {
        Expr::Fail(value) => match value.as_ref() {
            Expr::Variable(name) if name == binding => {}
            _ => return None,
        },
        _ => return None,
    }
    let Expr::Finish(call) = body.as_ref() else {
        return None;
    };
    let Expr::Call { function, .. } = call.as_ref() else {
        return None;
    };
    let Expr::Function(function) = function.as_ref() else {
        return None;
    };
    Some(strip_completion_value(&function.body))
}

/// Drop the lowerer's trailing unit completion value from a function body.
fn strip_completion_value(body: &Expr) -> &Expr {
    let Expr::Block(statements) = body else {
        return body;
    };
    match statements.as_slice() {
        [inner @ Expr::Block(_), Expr::Undefined] => inner,
        _ => body,
    }
}

/// The array method a generated callback worker implements.
///
/// `map` appends the callback's result on every step; `filter` appends the
/// element under an `if` on the callback's result. The worker is generated, so
/// this shape is the lowerer's, not a user's.
fn array_callback_method(body: &Expr) -> Option<&'static str> {
    let Expr::Block(statements) = body else {
        return None;
    };
    let loop_body = statements.iter().find_map(|statement| match statement {
        Expr::While { body, .. } => Some(body.as_ref()),
        _ => None,
    })?;
    let Expr::Block(loop_statements) = loop_body else {
        return None;
    };
    let mut method = None;
    for statement in loop_statements {
        match statement {
            Expr::If { then_block, .. } => {
                if let Expr::Assign { target, .. } = then_block.as_ref()
                    && target.root.as_str().ends_with("_callback_output")
                {
                    method = Some("filter");
                }
            }
            Expr::Assign { target, expr } => {
                if target.root.as_str().ends_with("_callback_output")
                    && matches!(expr.as_ref(), Expr::Call { .. })
                {
                    method = Some("map");
                }
            }
            _ => {}
        }
    }
    method
}

/// The arguments of a `__typescript_stdlib` call with the given selector.
fn stdlib_call<'a>(expression: &'a Expr, selector: &str) -> Option<&'a [Expr]> {
    let Expr::BuiltinCall { name, args } = expression else {
        return None;
    };
    if name.as_str() != "__typescript_stdlib" {
        return None;
    }
    let [Expr::String(found), rest @ ..] = args.as_slice() else {
        return None;
    };
    (found.as_str() == selector).then_some(rest)
}

fn indent(level: usize) -> String {
    "  ".repeat(level)
}

fn key(name: &str) -> String {
    if is_typescript_identifier(name) {
        return name.to_string();
    }
    string_literal(name)
}

fn number_literal(value: f64) -> Printed {
    if !value.is_finite() {
        return Err(TypeScriptSourceError::UnsupportedNumber {
            value: value.to_string(),
        });
    }
    Ok(ryu_js::Buffer::new().format_finite(value).to_string())
}

fn string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn is_typescript_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        })
        && !crate::reserved_words().contains(&name)
}

fn lash_binary_op(op: BinaryOp) -> Result<&'static str, TypeScriptSourceError> {
    Ok(match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Equal => "===",
        BinaryOp::NotEqual => "!==",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::In => {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a Lashlang `in` test",
            });
        }
    })
}

fn javascript_binary_op(op: JavaScriptBinaryOp) -> &'static str {
    match op {
        JavaScriptBinaryOp::Add => "+",
        JavaScriptBinaryOp::Subtract => "-",
        JavaScriptBinaryOp::Multiply => "*",
        JavaScriptBinaryOp::Divide => "/",
        JavaScriptBinaryOp::Remainder => "%",
        JavaScriptBinaryOp::StrictEqual => "===",
        JavaScriptBinaryOp::StrictNotEqual => "!==",
        JavaScriptBinaryOp::LooseEqual => "==",
        JavaScriptBinaryOp::LooseNotEqual => "!=",
        JavaScriptBinaryOp::Less => "<",
        JavaScriptBinaryOp::LessEqual => "<=",
        JavaScriptBinaryOp::Greater => ">",
        JavaScriptBinaryOp::GreaterEqual => ">=",
    }
}
