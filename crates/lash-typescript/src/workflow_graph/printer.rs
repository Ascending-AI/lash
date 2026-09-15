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
//!   `__typescript_process_error` catch prints back as
//!   `const <name> = async (..) => { .. };`.
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
    AssignPathStep, AssignTarget, BinaryOp, Declaration, Expr, FunctionDecl, FunctionExpr,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, ProcessDecl, ProcessLiteralExpr,
    Program, ResourceRefExpr, UnaryOp,
};
use thiserror::Error;

use crate::GENERATED_BINDING_PREFIX;
use crate::node_label::render_label_comment;

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
    #[error("label `{title}` has no TypeScript spelling")]
    UnrepresentableLabel { title: String },
}

type Printed = Result<String, TypeScriptSourceError>;

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

/// Print one statement as canonical TypeScript.
///
/// `bound` names the identifiers already in scope where the statement sits, so
/// a re-assignment is not re-declared. This is the textual form carried by an
/// opaque node, which owns a whole statement rather than one expression.
pub fn typescript_statement_source(expression: &Expr, bound: &[String]) -> Printed {
    let mut bound = bound.to_vec();
    Ok(Printer
        .statement(expression, 0, &mut bound)?
        .trim_end()
        .to_string())
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
                // A process declaration is printed where `main` binds it, so
                // the authored `const <binding> = async (..) => ..` keeps its
                // name.
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

    /// Re-sugar a lowered process declaration into its authored arrow.
    ///
    /// A process is an uncalled `const`-bound `async` arrow (FIG-2999): the
    /// binding is the name a reader sees, and the declaration's own name is a
    /// lift digest that no authored source spells.
    fn define_process(
        &self,
        binding: &str,
        process: &ProcessDecl,
        bound: &mut Vec<String>,
    ) -> Printed {
        let body = process_run_body(process).ok_or(TypeScriptSourceError::Unrepresentable {
            kind: "a process body that is not the lowerer's process wrapper",
        })?;
        let params = process
            .params
            .iter()
            .map(|param| self.identifier("process parameter", param.name.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = String::new();
        if let Some(label) = &process.label {
            out.push_str(&label_comment(label)?);
            out.push('\n');
        }
        out.push_str(&format!(
            "const {} = async ({}) => ",
            self.identifier("process binding", binding)?,
            params.join(", "),
        ));
        let mut run_bound = process
            .params
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>();
        out.push_str(&self.block(body, 0, &mut run_bound)?);
        out.push_str(";\n");
        bound.push(binding.to_string());
        Ok(out)
    }

    fn block(&self, expression: &Expr, level: usize, bound: &mut Vec<String>) -> Printed {
        let statements = statement_block_contents(expression);
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

    /// Print an already-normalised statement list as a braced block.
    fn block_statements(
        &self,
        statements: &[Expr],
        level: usize,
        bound: &mut Vec<String>,
    ) -> Printed {
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
        // The lowerer gives each statement in a block a value by wrapping it;
        // the statement the user wrote is inside that wrapper.
        let expression = authored_statement(expression);
        match expression {
            // The lowerer's unit completion value. It is not something a user
            // wrote, and `undefined;` is not a statement worth showing.
            Expr::Undefined => Ok(String::new()),
            Expr::LabelAnnotated { label, expr } => {
                // The label is a one-line doc comment on the statement it
                // names, which is exactly what a parse reads back into this
                // node (FIG-3047).
                let statement = self.statement(expr, level, bound)?;
                if statement.is_empty() {
                    // Nothing was printed, so there is no statement for the
                    // comment to attach to; a dangling comment would re-parse
                    // onto whatever came next.
                    return Ok(statement);
                }
                Ok(format!("{prefix}{}\n{statement}", label_comment(label)?))
            }
            expression if let Some((target, value)) = assignment_sugar(expression) => Ok(format!(
                "{prefix}{} = {};\n",
                self.assign_target(&target)?,
                self.expression(value)?
            )),
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
                    // A process literal only lifts from a `const` binding, so
                    // the one binding form the lens cannot spell as `let` is
                    // the one that introduces a process.
                    let keyword = if matches!(expr.as_ref(), Expr::ProcessLiteral(_)) {
                        "const"
                    } else {
                        "let"
                    };
                    return Ok(format!("{prefix}{keyword} {rendered}"));
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
                    // An absent `else` is the lowerer's unit value, and an
                    // authored `else {}` is a block whose only element is that
                    // unit completion. Neither carries a statement, and a graph
                    // whose else branch holds no nodes renders as an empty
                    // block too, so all three print alike: GetPut would break
                    // if canonical text kept an else the graph cannot hold.
                    Expr::Undefined => {}
                    other if statement_block_contents(other).is_empty() => {}
                    other => {
                        let mut else_bound = bound.clone();
                        out.push_str(" else ");
                        // An `else if` chain lowers to a block holding the one
                        // nested `if`, so a block of that shape prints back as
                        // the chain the author wrote rather than a nested block.
                        match else_if_chain(other) {
                            Some(chain) => out.push_str(
                                self.statement(chain, level, &mut else_bound)?
                                    .trim_start()
                                    .trim_end_matches('\n'),
                            ),
                            None => out.push_str(&self.block(other, level, &mut else_bound)?),
                        }
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
                // `for (const x of xs)` lowers to a generated element binding
                // over `Lash.ArrayFromIterable(xs)` whose body opens by copying
                // the element into the authored binding. Re-sugar that shape
                // back to the loop the user wrote.
                if let Some((authored, iterable, body)) = for_of_sugar(binding, iterable, body) {
                    let mut body_bound = bound.clone();
                    body_bound.push(authored.to_string());
                    return Ok(format!(
                        "{prefix}for ({} {} of {}) {}\n",
                        element_binding_kind(body, authored),
                        self.identifier("loop binding", authored)?,
                        self.expression(iterable)?,
                        self.block_statements(
                            match body {
                                [single] => statement_block_contents(single),
                                body => body,
                            },
                            level,
                            &mut body_bound,
                        )?
                    ));
                }
                let mut body_bound = bound.clone();
                body_bound.push(binding.to_string());
                Ok(format!(
                    "{prefix}for ({} {} of {}) {}\n",
                    element_binding_kind(statement_block_contents(body), binding.as_str()),
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
            Expr::Print(value) => Ok(format!("print({})", self.expression(value)?)),
            Expr::Yield(value) => Ok(format!("yield {}", self.expression(value)?)),
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
            // An inline process body prints back as the authored async arrow
            // in its argument position, which re-parses to the same literal.
            Expr::ProcessLiteral(literal) => {
                let body = crate::lower::wrapped_run_body(&literal.body).ok_or(
                    TypeScriptSourceError::Unrepresentable {
                        kind: "a process body that is not the lowerer's wrapper",
                    },
                )?;
                let params = literal
                    .params
                    .iter()
                    .map(|param| self.identifier("process parameter", param.name.as_str()))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bound = literal
                    .params
                    .iter()
                    .map(|param| param.name.to_string())
                    .collect();
                Ok(format!(
                    "async ({}) => {}",
                    params.join(", "),
                    self.block(body, 0, &mut bound)?
                ))
            }
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
            // A failed host operation throws in TypeScript, so the unwrap the
            // lowerer wraps every module call in has no spelling of its own.
            Expr::ResultUnwrap(value) => self.expression(value),
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
        // `await x` on a value that may be a pending promise.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__typescript_await_pending"
            && let [value] = args.as_slice()
        {
            return Ok(Some(format!("await {}", self.unary_operand(value)?)));
        }
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
    crate::lower::process_run_body_path(process).map(|(_, body)| strip_completion_value(body))
}

/// The authored `run` body inside a process *literal*'s wrapper.
///
/// FIG-2999 made a top-level `const`-bound `async` arrow a process literal in
/// `main` rather than a [`ProcessDecl`], so a lens door that re-parses a
/// fragment inside such an arrow reads the body out of the literal. The
/// wrapper shape is the same one [`process_run_body`] unwraps.
pub(super) fn process_literal_run_body(literal: &ProcessLiteralExpr) -> Option<&Expr> {
    crate::lower::process_run_body_path_of(&literal.body)
        .map(|(_, body)| strip_completion_value(body))
}

/// The statements of a block, with the lowerer's block wrapper removed.
///
/// The lowerer wraps every authored statement block and ends it with the
/// block's completion value. Neither piece is authored text: the wrapper has no
/// spelling and the completion value is unobservable in statement position, and
/// both are re-synthesised when the printed block is lowered again.
pub(super) fn statement_block_contents(expression: &Expr) -> &[Expr] {
    let mut expression = expression;
    let mut unwrapped = false;
    while let Some(inner) = block_wrapper_inner(expression) {
        expression = inner;
        unwrapped = true;
    }
    let Expr::Block(statements) = expression else {
        return std::slice::from_ref(expression);
    };
    match statements.as_slice() {
        [rest @ .., last] if trailing_is_generated(last, unwrapped) => rest,
        other => other,
    }
}

/// Whether a block's last element is the lowerer's completion value.
///
/// `Undefined` is the unit completion the lowerer appends to every statement
/// block; inside the wrapper the completion is the block's own value, which is
/// unobservable in statement position and re-synthesised on the way back down.
pub(super) fn trailing_is_generated(last: &Expr, unwrapped: bool) -> bool {
    matches!(last, Expr::Undefined) || (unwrapped && lashlang::is_pure_expr(last))
}

/// The inner block of the lowerer's statement-block wrapper.
pub(super) fn block_wrapper_inner(expression: &Expr) -> Option<&Expr> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    let inner = match statements.as_slice() {
        [inner @ Expr::Block(_), Expr::Undefined] | [inner @ Expr::Block(_)] => inner,
        _ => return None,
    };
    // A lowered member assignment is also a block of generated bindings, but
    // it is one statement rather than a nested scope.
    if assignment_sugar(inner).is_some() {
        return None;
    }
    Some(inner)
}

/// The authored target and value of a lowered member assignment.
///
/// `a.b = v` lowers to a block that pins the reference base, evaluates the
/// value, stores through the pinned base and completes with the stored value.
/// Every binding in that block is generated, so the block prints and projects
/// as the one assignment the user wrote.
/// One authored statement, stripped of the completion-value wrapper.
///
/// The lowerer gives every statement in a block a value by wrapping it as
/// `Block([statement, <completion value>])`. Neither the printer nor the
/// projector wants that wrapper: it is not something anyone wrote.
pub(super) fn authored_statement(expression: &Expr) -> &Expr {
    let Expr::Block(statements) = expression else {
        return expression;
    };
    match statements.as_slice() {
        [single, last]
            if lashlang::is_pure_expr(last) && assignment_sugar(expression).is_none() =>
        {
            authored_statement(single)
        }
        _ => expression,
    }
}

/// `let` when the loop body reassigns its element binding, `const` otherwise.
///
/// The lowerer erases the declaration kind — it copies the element into the
/// authored binding either way — so the kind is recovered from whether the body
/// writes the binding back, which is the only thing `const` would have refused.
fn element_binding_kind(body: &[Expr], authored: &str) -> &'static str {
    fn reassigns(expression: &Expr, authored: &str) -> bool {
        if let Expr::Assign { target, .. } = expression
            && target.root.as_str() == authored
        {
            return true;
        }
        expression
            .children()
            .any(|child| reassigns(child, authored))
    }

    if body.iter().any(|statement| reassigns(statement, authored)) {
        "let"
    } else {
        "const"
    }
}

/// The single nested `if` an `else if` chain lowers to, if this is one.
pub(super) fn else_if_chain(expression: &Expr) -> Option<&Expr> {
    match statement_block_contents(expression) {
        [nested @ Expr::If { then_block, .. }] if matches!(then_block.as_ref(), Expr::Block(_)) => {
            Some(nested)
        }
        _ => None,
    }
}

pub(super) fn assignment_sugar(expression: &Expr) -> Option<(AssignTarget, &Expr)> {
    let Expr::Block(statements) = expression else {
        return None;
    };
    let [
        Expr::Assign {
            target: base_target,
            expr: base,
        },
        Expr::Assign {
            target: result_target,
            expr: value,
        },
        Expr::Assign {
            target: store,
            expr: stored,
        },
        Expr::Variable(completion),
    ] = statements.as_slice()
    else {
        return None;
    };
    if !generated_binding(base_target, "_reference_base")
        || !generated_binding(result_target, "_assignment_result")
        || store.root != base_target.root
        || store.steps.is_empty()
        || *completion != result_target.root
    {
        return None;
    }
    match stored.as_ref() {
        Expr::Variable(name) if *name == result_target.root => {}
        _ => return None,
    }
    let Expr::Variable(root) = base.as_ref() else {
        return None;
    };
    Some((
        AssignTarget {
            root: root.clone(),
            steps: store.steps.clone(),
        },
        value,
    ))
}

fn generated_binding(target: &AssignTarget, suffix: &str) -> bool {
    target.is_simple()
        && target.root.starts_with(GENERATED_BINDING_PREFIX)
        && target.root.ends_with(suffix)
}

/// The authored binding, iterable and body of a lowered `for (.. of ..)` loop.
///
/// `for (const x of xs)` lowers to a generated element binding over
/// `Lash.ArrayFromIterable(xs)` whose body opens by copying the element into
/// the authored binding.
pub(super) fn for_of_sugar<'a>(
    binding: &str,
    iterable: &'a Expr,
    body: &'a Expr,
) -> Option<(&'a str, &'a Expr, &'a [Expr])> {
    if !binding.starts_with(GENERATED_BINDING_PREFIX) {
        return None;
    }
    let [source] = stdlib_call(iterable, "Lash.ArrayFromIterable")? else {
        return None;
    };
    let Expr::Block(statements) = body else {
        return None;
    };
    let [Expr::Assign { target, expr }, rest @ ..] = statements.as_slice() else {
        return None;
    };
    if !target.is_simple() {
        return None;
    }
    match expr.as_ref() {
        Expr::Variable(name) if name.as_str() == binding => {}
        _ => return None,
    }
    Some((target.root.as_str(), source, rest))
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
            Expr::Assign { target, expr }
                if target.root.as_str().ends_with("_callback_output")
                    && matches!(expr.as_ref(), Expr::Call { .. }) =>
            {
                method = Some("map");
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

/// The doc comment a label is spelled as, or the refusal when its text has no
/// spelling that would read back as the same label.
fn label_comment(label: &lashlang::LabelMetadata) -> Printed {
    render_label_comment(
        label.title.as_str(),
        label.description.as_ref().map(|text| text.as_str()),
    )
    .ok_or_else(|| TypeScriptSourceError::UnrepresentableLabel {
        title: label.title.to_string(),
    })
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
