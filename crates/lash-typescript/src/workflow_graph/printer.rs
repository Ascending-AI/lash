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
//! * A process body in the process-wrapper role prints back as
//!   `const <name> = async (..) => { .. };`.
//! * `Print(__typescript_stdlib("__consoleObservationText", x))` prints back as
//!   `console.log(x)`.
//! * `__typescript_await_array([..], "all")` prints back as
//!   `await Promise.all([..])`, and likewise for `allSettled`, `race` and
//!   `any`; `__typescript_pending_timer(ms)` prints back as `sleep(ms)`.
//! * A collection-transform role prints back as `receiver.<operation>(fn)`.
//! * An attribute-assignment role prints back as `object.field = value`.
//! * An iteration whose bind copies the element into one authored binding
//!   prints back as `for (const x of source)` or `for (const x in source)`.
//!
//! Structure is read off IR forms and structural roles only; no generated
//! name is ever inspected to decide what a shape is.
//!
//! A generated `__typescript_*` binding that reaches the printer without being
//! re-sugared is a defect, not a rendering choice: it has no authored spelling,
//! so it is refused with [`TypeScriptSourceError::GeneratedBinding`] rather than
//! surfaced to a user.

use lashlang::{
    AssignPathStep, AssignTarget, BinaryOp, Declaration, Expr, FunctionDecl, FunctionExpr,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, MethodKey, ProcessDecl,
    ProcessLiteralExpr, Program, ResourceRefExpr, StructuralRole, TypeExpr, UnaryOp,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

mod for_loop;
mod templates;

use for_loop::{classic_for, is_statement_body, var_initialization};
use templates::{template_parts, template_text};

#[cfg(test)]
use std::cell::Cell;

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
    #[error("cannot render host descriptor constructor `{type_name}` without a constructor path")]
    UnknownHostDescriptorConstructor { type_name: String },
    #[error("label `{title}` has no TypeScript spelling")]
    UnrepresentableLabel { title: String },
}

type Printed = Result<String, TypeScriptSourceError>;

/// Print a lowered program as a canonical TypeScript module.
pub fn typescript_program_source(program: &Program) -> Printed {
    #[cfg(test)]
    PROGRAM_PRINT_COUNT.with(|count| count.set(count.get() + 1));
    Printer::for_program(program).program(program)
}

#[cfg(test)]
thread_local! {
    static PROGRAM_PRINT_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_program_print_count() {
    PROGRAM_PRINT_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(super) fn program_print_count() -> usize {
    PROGRAM_PRINT_COUNT.with(Cell::get)
}

/// Print one expression as canonical TypeScript.
///
/// This is the textual form carried by editable workflow-graph node fields.
pub fn typescript_expression_source(expression: &Expr) -> Printed {
    Printer::plain().statement_expression(expression)
}

/// Print one statement as canonical TypeScript.
///
/// `bound` names the identifiers already in scope where the statement sits, so
/// a re-assignment is not re-declared. This is the textual form carried by an
/// opaque node, which owns a whole statement rather than one expression.
pub fn typescript_statement_source(expression: &Expr, bound: &[String]) -> Printed {
    let mut bound = bound.to_vec();
    Ok(Printer::plain()
        .statement(expression, 0, &mut bound, &BTreeSet::new())?
        .trim_end()
        .to_string())
}

/// Print one assignment target as canonical TypeScript.
pub fn typescript_assign_target_source(target: &AssignTarget) -> Printed {
    Printer::plain().assign_target(target)
}

/// The printer, with the program's lifted process declarations in view: an
/// admitted program references a lifted literal by its declaration, and that
/// reference prints back as the literal it was lifted from.
struct Printer<'p> {
    lifted: BTreeMap<&'p str, &'p ProcessDecl>,
    /// The update of each classic `for` whose body is being printed,
    /// innermost last. A `continue` in such a body lowers to the update
    /// followed by the jump, and prints back as the bare `continue`.
    continue_epilogues: RefCell<Vec<Option<Expr>>>,
    /// The receiver slots of the functions printed so far. A read of one is
    /// the source's `this`: the function that owns it prints first, and an
    /// arrow inside it reads the same slot as a capture.
    receivers: RefCell<BTreeSet<String>>,
}

impl Printer<'static> {
    fn plain() -> Self {
        Self {
            lifted: BTreeMap::new(),
            continue_epilogues: RefCell::new(Vec::new()),
            receivers: RefCell::new(BTreeSet::new()),
        }
    }
}

impl<'p> Printer<'p> {
    fn for_program(program: &'p Program) -> Self {
        Self {
            lifted: program
                .declarations
                .iter()
                .filter_map(|declaration| match declaration {
                    Declaration::Process(process) if process.origin.is_lifted() => {
                        Some((process.name.as_str(), process))
                    }
                    _ => None,
                })
                .collect(),
            continue_epilogues: RefCell::new(Vec::new()),
            receivers: RefCell::new(BTreeSet::new()),
        }
    }

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

        let statements: Vec<&Expr> = match &program.main {
            Expr::Block(statements) => statements.iter().collect(),
            statement => std::slice::from_ref(statement).iter().collect(),
        };
        let mut vars = BTreeSet::new();
        let hoisted = self.hoisted_vars(&statements, 0, &mut bound, &mut vars, &mut out)?;
        let mut emitted_processes = Vec::new();
        for statement in statements[hoisted..].iter().copied() {
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
            out.push_str(&self.statement(statement, 0, &mut bound, &vars)?);
        }

        for process in &processes {
            // A lifted literal prints wherever the program references it.
            if !emitted_processes.contains(&process.name.to_string()) && !process.origin.is_lifted()
            {
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
        let mut bound: Vec<String> = function
            .params
            .iter()
            .map(|param| param.name.to_string())
            .collect();
        out.push_str(&self.rooted_block(&function.body, 0, &mut bound)?);
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
        let params = authored_params(process)
            .iter()
            .map(|param| self.process_param(param))
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
        let mut run_bound = authored_params(process)
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>();
        out.push_str(&self.rooted_block(body, 0, &mut run_bound)?);
        out.push_str(";\n");
        bound.push(binding.to_string());
        Ok(out)
    }

    fn block(
        &self,
        expression: &Expr,
        level: usize,
        bound: &mut Vec<String>,
        vars: &BTreeSet<String>,
    ) -> Printed {
        let statements = statement_block_contents(expression);
        if statements.is_empty() {
            return Ok("{}".to_string());
        }
        let mut out = String::from("{\n");
        for statement in statements {
            out.push_str(&self.statement(statement, level + 1, bound, vars)?);
        }
        out.push_str(&indent(level));
        out.push('}');
        Ok(out)
    }

    /// The block of a function-rooted body — a function's or a process's run
    /// body, where the module's own root list is [`Self::program`]'s.
    fn rooted_block(&self, expression: &Expr, level: usize, bound: &mut Vec<String>) -> Printed {
        let statements = statement_block_contents(expression);
        if statements.is_empty() {
            return Ok("{}".to_string());
        }
        let mut out = String::from("{\n");
        let mut vars = BTreeSet::new();
        let hoisted = self.hoisted_vars(&statements, level + 1, bound, &mut vars, &mut out)?;
        for statement in &statements[hoisted..] {
            out.push_str(&self.statement(statement, level + 1, bound, &vars)?);
        }
        out.push_str(&indent(level));
        out.push('}');
        Ok(out)
    }

    /// Print the `name = undefined` assignments a root statement list opens
    /// with as `var name;`, collecting the hoisted names into `vars` and
    /// returning how many leading statements they are.
    ///
    /// The lowerer hoists every `var` a root list — the module's `main`, a
    /// function body, a process's run body — declares to such an assignment
    /// at its head, ahead of the assignments hoisted function declarations
    /// flush to. Spelled `let name = undefined;` the assignment stays where
    /// it was printed, behind those declarations, and the reparsed program is
    /// a different one; `var name;` lowers back to the head. A leading
    /// `name = undefined` that no `var` produced — a first statement
    /// `let name = undefined;` is one — re-lowers identically either way, so
    /// the spelling is safe. An already-bound name (a function's parameter)
    /// is never a hoist and prints as an ordinary assignment.
    fn hoisted_vars(
        &self,
        statements: &[&Expr],
        level: usize,
        bound: &mut Vec<String>,
        vars: &mut BTreeSet<String>,
        out: &mut String,
    ) -> Result<usize, TypeScriptSourceError> {
        let mut count = 0;
        while let Some(Expr::Assign { target, expr }) = statements.get(count).copied() {
            let hoist = target.is_simple()
                && matches!(expr.as_ref(), Expr::Undefined)
                && !bound
                    .iter()
                    .any(|name| name.as_str() == target.root.as_str());
            if !hoist {
                break;
            }
            out.push_str(&format!(
                "{}var {};\n",
                indent(level),
                self.identifier("var binding", target.root.as_str())?
            ));
            bound.push(target.root.to_string());
            vars.insert(target.root.to_string());
            count += 1;
        }
        Ok(count)
    }

    fn statement(
        &self,
        expression: &Expr,
        level: usize,
        bound: &mut Vec<String>,
        vars: &BTreeSet<String>,
    ) -> Printed {
        let prefix = indent(level);
        match expression {
            // A statement the front end closed with a completion value prints
            // as the statements it wraps — unless it is a `var` initializer:
            // `var name = init` lowers to the assignment `name = init` closed
            // by `name`, which only the `var` spelling lowers back into. A
            // `var` binding is not one a bare `name = init` may assign, and
            // `let name = init` declares a second, lexical `name`.
            Expr::Role {
                role: StructuralRole::Completion,
                ..
            } => {
                if let Some((target, init)) = var_initialization(expression)
                    && vars.contains(target.root.as_str())
                    && !suppresses_named_evaluation(target, init)
                {
                    return Ok(format!(
                        "{prefix}var {} = {};\n",
                        self.identifier("var binding", target.root.as_str())?,
                        self.expression(init)?
                    ));
                }
                let mut out = String::new();
                for statement in statement_block_contents(expression) {
                    out.push_str(&self.statement(statement, level, bound, vars)?);
                }
                Ok(out)
            }
            Expr::Role {
                role: StructuralRole::Scope,
                expr,
            } => {
                let mut inner = bound.clone();
                Ok(format!(
                    "{prefix}{}\n",
                    self.block(expr, level, &mut inner, vars)?
                ))
            }
            // The lowerer's unit completion value. It is not something a user
            // wrote, and `undefined;` is not a statement worth showing.
            Expr::Undefined => Ok(String::new()),
            Expr::LabelAnnotated { label, expr } => {
                // The label is a one-line doc comment on the statement it
                // names, which is exactly what a parse reads back into this
                // node (FIG-3047).
                let statement = self.statement(expr, level, bound, vars)?;
                if statement.is_empty() {
                    // Nothing was printed, so there is no statement for the
                    // comment to attach to; a dangling comment would re-parse
                    // onto whatever came next.
                    return Ok(statement);
                }
                Ok(format!("{prefix}{}\n{statement}", label_comment(label)?))
            }
            expression
                if let Some((target, operator, value)) = attribute_assignment(expression)? =>
            {
                Ok(format!(
                    "{prefix}{target} {operator} {};\n",
                    self.expression(value)?
                ))
            }
            expression if let Some(classic) = classic_for(expression) => {
                let mut inner = bound.clone();
                let head = self.for_head(classic.init, &mut inner, vars)?;
                let condition = match classic.condition {
                    Expr::Bool(true) => String::new(),
                    condition => format!(" {}", self.expression(condition)?),
                };
                let update = match classic.update {
                    Some(update) => format!(" {}", self.for_update(update)?),
                    None => String::new(),
                };
                self.continue_epilogues
                    .borrow_mut()
                    .push(classic.update.cloned());
                let body = self.block(classic.body, level, &mut inner, vars);
                self.continue_epilogues.borrow_mut().pop();
                Ok(format!(
                    "{prefix}for ({head};{condition};{update}) {}\n",
                    body?
                ))
            }
            // A classic-for `continue`: the loop's update, then the jump.
            Expr::Block(items)
                if let [epilogue, Expr::Continue] = items.as_slice()
                    && self
                        .continue_epilogues
                        .borrow()
                        .last()
                        .is_some_and(|update| update.as_ref() == Some(epilogue)) =>
            {
                Ok(format!("{prefix}continue;\n"))
            }
            Expr::Block(items) => {
                if let Some((name, operator, operand)) = compound_assign_block(items) {
                    let name = self.identifier("binding", name)?;
                    let operand = self.expression(operand)?;
                    return Ok(format!("{prefix}{name} {operator}= {operand};\n"));
                }
                let mut inner = bound.clone();
                Ok(format!(
                    "{prefix}{}\n",
                    self.block(expression, level, &mut inner, vars)?
                ))
            }
            // A function bound to its own name is its declaration.
            Expr::Assign { target, expr }
                if target.is_simple()
                    && let Expr::Function(function) = expr.as_ref()
                    && function.name.as_deref() == Some(target.root.as_str()) =>
            {
                bound.push(target.root.to_string());
                Ok(format!(
                    "{prefix}{}\n",
                    self.named_function(target.root.as_str(), function)?
                ))
            }
            Expr::Assign { target, expr } => {
                // A `var` initializer is this assignment wearing the
                // declaration, and a graph carries it as the bare assign the
                // completion unwraps to. Only `var` spells it back: the
                // binding is not one `name = ..` may assign.
                if target.is_simple()
                    && vars.contains(target.root.as_str())
                    && !suppresses_named_evaluation(target, expr)
                {
                    return Ok(format!(
                        "{prefix}var {} = {};\n",
                        self.identifier("var binding", target.root.as_str())?,
                        self.expression(expr)?
                    ));
                }
                // NamedEvaluation names an anonymous function for a bare
                // `name = ..` target; a function whose `js_name` is not the
                // target's was authored `(name) = ..` and only prints so.
                let rendered = if suppresses_named_evaluation(target, expr) {
                    format!(
                        "({}) = {};\n",
                        self.assign_target(target)?,
                        self.expression(expr)?
                    )
                } else {
                    format!(
                        "{} = {};\n",
                        self.assign_target(target)?,
                        self.expression(expr)?
                    )
                };
                if !suppresses_named_evaluation(target, expr)
                    && target.is_simple()
                    && !bound.contains(&target.root.to_string())
                {
                    bound.push(target.root.to_string());
                    // A process literal only lifts from a `const` binding, so
                    // the one binding form the lens cannot spell as `let` is
                    // the one that introduces a process: a literal, or a
                    // lifted process's reference, which prints as its literal.
                    let keyword = if matches!(expr.as_ref(), Expr::ProcessLiteral(_))
                        || matches!(expr.as_ref(), Expr::ProcessRef { process }
                            if self.lifted.contains_key(process.as_str()))
                    {
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
            } if is_statement_body(then_block) => {
                let mut out = format!("{prefix}if ({}) ", self.expression(condition)?);
                let mut then_bound = bound.clone();
                out.push_str(&self.block(then_block, level, &mut then_bound, vars)?);
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
                        match lashlang::else_if_chain(other) {
                            Some(chain) => out.push_str(
                                self.statement(chain, level, &mut else_bound, vars)?
                                    .trim_start()
                                    .trim_end_matches('\n'),
                            ),
                            None => {
                                out.push_str(&self.block(other, level, &mut else_bound, vars)?)
                            }
                        }
                    }
                }
                out.push('\n');
                Ok(out)
            }
            Expr::For {
                binding,
                iterable,
                bind,
                body,
            } => {
                let header = loop_header(binding.as_str(), iterable, bind.as_deref())?;
                // An element binding already in scope is assigned by the
                // loop, not declared — but a `var` head keeps its `var`: the
                // binding is not one a bare `for (x of ..)` may assign.
                let declaration = if vars.contains(header.binding) {
                    "var"
                } else if bound.iter().any(|name| name == header.binding) {
                    ""
                } else {
                    element_binding_kind(&statement_block_contents(body), header.binding)
                };
                let mut body_bound = bound.clone();
                body_bound.push(header.binding.to_string());
                Ok(format!(
                    "{prefix}for ({}{}{} {} {}) {}\n",
                    declaration,
                    if declaration.is_empty() { "" } else { " " },
                    self.identifier("loop binding", header.binding)?,
                    if header.keys { "in" } else { "of" },
                    self.expression(header.source)?,
                    self.block(body, level, &mut body_bound, vars)?
                ))
            }
            Expr::While { condition, body } => {
                let mut body_bound = bound.clone();
                Ok(format!(
                    "{prefix}while ({}) {}\n",
                    self.expression(condition)?,
                    self.block(body, level, &mut body_bound, vars)?
                ))
            }
            Expr::Try(try_expr) => {
                let mut out = format!("{prefix}try ");
                let mut body_bound = bound.clone();
                out.push_str(&self.block(&try_expr.body, level, &mut body_bound, vars)?);
                if let Some(catch) = &try_expr.catch {
                    let mut catch_bound = bound.clone();
                    catch_bound.push(catch.binding.to_string());
                    out.push_str(&format!(
                        " catch ({}) {}",
                        self.identifier("catch binding", catch.binding.as_str())?,
                        self.block(&catch.body, level, &mut catch_bound, vars)?
                    ));
                }
                if let Some(finally) = &try_expr.finally {
                    let mut finally_bound = bound.clone();
                    out.push_str(&format!(
                        " finally {}",
                        self.block(finally, level, &mut finally_bound, vars)?
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

    /// A classic `for` head, from the statements its initialization lowered
    /// to: a `var` list assigns hoisted names, each closed by reading it back;
    /// a `let` list assigns names not yet in scope, which it adds to `bound`;
    /// and an expression head is the one expression statement.
    fn for_head(&self, init: &[Expr], bound: &mut Vec<String>, vars: &BTreeSet<String>) -> Printed {
        if init.is_empty() {
            return Ok(String::new());
        }
        let var_list = init
            .iter()
            .map(|item| {
                var_initialization(item).filter(|(target, _)| vars.contains(target.root.as_str()))
            })
            .collect::<Option<Vec<_>>>();
        if let Some(declarations) = var_list {
            let declarators = declarations
                .into_iter()
                .map(|(target, value)| {
                    Ok(format!(
                        "{} = {}",
                        self.identifier("var binding", target.root.as_str())?,
                        self.expression(value)?
                    ))
                })
                .collect::<Result<Vec<_>, TypeScriptSourceError>>()?;
            return Ok(format!("var {}", declarators.join(", ")));
        }
        let let_list = init
            .iter()
            .map(|item| match item {
                Expr::Assign { target, expr }
                    if target.is_simple()
                        && !bound
                            .iter()
                            .any(|name| name.as_str() == target.root.as_str()) =>
                {
                    Some((target, expr.as_ref()))
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        if let Some(declarations) = let_list {
            // The one binding form `let` cannot spell is a process's: a
            // process literal lifts only from a `const` binding.
            let keyword = if declarations.iter().any(|(_, value)| {
                matches!(value, Expr::ProcessLiteral(_))
                    || matches!(value, Expr::ProcessRef { process }
                        if self.lifted.contains_key(process.as_str()))
            }) {
                "const"
            } else {
                "let"
            };
            let mut declarators = Vec::with_capacity(declarations.len());
            for (target, value) in declarations {
                let name = self.identifier("loop binding", target.root.as_str())?;
                declarators.push(format!("{name} = {}", self.expression(value)?));
                bound.push(target.root.to_string());
            }
            return Ok(format!("{keyword} {}", declarators.join(", ")));
        }
        match init {
            [expression] if !matches!(expression, Expr::Assign { .. }) => {
                self.head_expression(expression)
            }
            _ => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a classic for head that is neither one declaration list nor one expression",
            }),
        }
    }

    /// A classic `for` update. `x++` and `x--` lower to the one assignment
    /// `x = x - -1` or `x = x - 1`, which print back as the update operator.
    fn for_update(&self, update: &Expr) -> Printed {
        if let Expr::Assign { target, expr } = update
            && target.is_simple()
            && let Expr::JavaScriptBinary {
                left,
                op: JavaScriptBinaryOp::Subtract,
                right,
            } = expr.as_ref()
            && matches!(left.as_ref(), Expr::Variable(name) if name.as_str() == target.root.as_str())
            && let Expr::Number(step) = right.as_ref()
            && (*step == -1.0 || *step == 1.0)
        {
            return Ok(format!(
                "{}{}",
                self.identifier("loop binding", target.root.as_str())?,
                if *step == -1.0 { "++" } else { "--" }
            ));
        }
        self.head_expression(update)
    }

    /// An expression statement of a classic `for` head or update, with no
    /// trailing semicolon. An assignment statement lowers to the assignment
    /// closed by reading its target back, and prints as the assignment.
    fn head_expression(&self, expression: &Expr) -> Printed {
        if let Some((target, value)) = var_initialization(expression) {
            return Ok(format!(
                "{} = {}",
                self.assign_target(target)?,
                self.expression(value)?
            ));
        }
        self.statement_expression(expression)
    }

    /// One expression in statement position, with no trailing semicolon.
    fn statement_expression(&self, expression: &Expr) -> Printed {
        match expression {
            Expr::Assign { target, expr } => Ok(format!(
                "{} = {}",
                if suppresses_named_evaluation(target, expr) {
                    format!("({})", self.assign_target(target)?)
                } else {
                    self.assign_target(target)?
                },
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
            Expr::Variable(name) if self.receivers.borrow().contains(name.as_str()) => {
                Ok("this".to_string())
            }
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
            Expr::ProcessRef { process } => match self.lifted.get(process.as_str()) {
                Some(lifted) => {
                    let body =
                        process_run_body(lifted).ok_or(TypeScriptSourceError::Unrepresentable {
                            kind: "a process body that is not the lowerer's wrapper",
                        })?;
                    let params = authored_params(lifted);
                    let printed = params
                        .iter()
                        .map(|param| self.process_param(param))
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut bound = params.iter().map(|param| param.name.to_string()).collect();
                    Ok(format!(
                        "async ({}) => {}",
                        printed.join(", "),
                        self.rooted_block(body, 0, &mut bound)?
                    ))
                }
                None => self.identifier("process", process.as_str()),
            },
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
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => {
                let args = args
                    .iter()
                    .map(|arg| self.expression(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                let receiver = self.member_target(receiver)?;
                Ok(match method {
                    MethodKey::Field(field) => format!(
                        "{receiver}.{}({})",
                        self.identifier("method", field.as_str())?,
                        args.join(", ")
                    ),
                    MethodKey::Index(key) => {
                        format!("{receiver}[{}]({})", self.expression(key)?, args.join(", "))
                    }
                })
            }
            // Only generated code passes an explicit receiver (a callback's
            // `thisArg`); the dialect has no `Function.prototype.call`.
            Expr::ThisCall { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a call with an explicit receiver",
            }),
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
                    .map(|param| self.process_param(param))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bound = literal
                    .params
                    .iter()
                    .map(|param| param.name.to_string())
                    .collect();
                Ok(format!(
                    "async ({}) => {}",
                    params.join(", "),
                    self.rooted_block(body, 0, &mut bound)?
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
                    JavaScriptUnaryOp::BitNot => "~",
                    JavaScriptUnaryOp::ToString => {
                        return Ok(format!("String({})", self.expression(expr)?));
                    }
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
                self.binary_operand(left)?,
                javascript_binary_op(*op),
                self.binary_operand(right)?
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
            Expr::Role { .. } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a statement structure in expression position",
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
            && let [items, Expr::String(method)] = args.as_slice()
        {
            return Ok(Some(format!(
                "await Promise.{method}({})",
                self.expression(items)?
            )));
        }
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__typescript_pending_timer"
            && let [duration] = args.as_slice()
        {
            return Ok(Some(format!("sleep({})", self.expression(duration)?)));
        }
        // `globalThis.name`, read live through the root-global read.
        if let Expr::BuiltinCall { name, args } = expression
            && name.as_str() == "__typescript_global_get"
            && let [Expr::String(global)] = args.as_slice()
        {
            return Ok(Some(format!(
                "globalThis.{}",
                self.identifier("global", global.as_str())?
            )));
        }
        if let Some(sugared) = self.collection_transform(expression)? {
            return Ok(Some(sugared));
        }
        if let Some((quasis, holes)) = template_parts(expression) {
            let mut out = String::from("`");
            for (index, quasi) in quasis.iter().enumerate() {
                out.push_str(&template_text(quasi));
                if let Some(hole) = holes.get(index) {
                    out.push_str(&format!("${{{}}}", self.expression(hole)?));
                }
            }
            out.push('`');
            return Ok(Some(out));
        }
        if let Some((target, operator, value)) = attribute_assignment(expression)? {
            return Ok(Some(format!(
                "({target} {operator} {})",
                self.expression(value)?
            )));
        }
        Ok(None)
    }

    /// A collection-transform role prints back as `receiver.<operation>(fn)`
    /// when it binds no operand beyond its receiver and callback; any other
    /// setup (an initial value, extra arguments) has no one-call spelling
    /// here.
    fn collection_transform(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Expr::Role {
            role: StructuralRole::CollectionTransform { operation },
            expr,
        } = expression
        else {
            return Ok(None);
        };
        let Some(parts) = lashlang::CollectionTransformParts::of(expr) else {
            return Ok(None);
        };
        if !parts.operands.is_empty() {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a collection transform with extra arguments",
            });
        }
        Ok(Some(format!(
            "{}.{}({})",
            self.member_target(parts.receiver)?,
            self.identifier("operation", operation.as_str())?,
            self.expression(parts.callback)?
        )))
    }

    /// A closure prints as an arrow, and as an `async` arrow when its own body
    /// awaits: only an async arrow lowers to a closure that awaits, and an
    /// `await` in a sync arrow does not parse.
    /// A process parameter with the annotation its declared type lowers
    /// from, so a typed parameter keeps its type through a re-admission.
    fn process_param(&self, param: &lashlang::ProcessParam) -> Printed {
        let name = self.identifier("process parameter", param.name.as_str())?;
        Ok(match type_annotation(&param.ty)? {
            Some(annotation) => format!("{name}: {annotation}"),
            None => name,
        })
    }

    fn arrow(&self, function: &FunctionExpr) -> Printed {
        if let Some(name) = &function.name {
            return self.named_function(name.as_str(), function);
        }
        // A function that reads its receiver is a `function` form: an arrow's
        // `this` is its enclosing function's.
        if let Some(receiver) = &function.receiver {
            self.receivers.borrow_mut().insert(receiver.to_string());
            let params = function
                .params
                .iter()
                .map(|param| self.identifier("function parameter", param.as_str()))
                .collect::<Result<Vec<_>, _>>()?;
            let mut bound = function.params.iter().map(ToString::to_string).collect();
            return Ok(format!(
                "{}function ({}) {}",
                if awaits_in_own_body(&function.body) {
                    "async "
                } else {
                    ""
                },
                params.join(", "),
                self.rooted_block(&function.body, 0, &mut bound)?
            ));
        }
        let params = function
            .params
            .iter()
            .map(|param| self.identifier("arrow parameter", param.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut bound = function.params.iter().map(ToString::to_string).collect();
        // An expression-bodied arrow lowers to a body that is one `return`;
        // it prints back as an expression body, which lowers to that `return`
        // again, and a braced body would lower to a different program.
        let body = match function.body.as_ref() {
            Expr::Block(items) if let [Expr::Return(value)] = items.as_slice() => {
                format!("({})", self.expression(value)?)
            }
            body => self.rooted_block(body, 0, &mut bound)?,
        };
        Ok(format!(
            "{}({}) => {body}",
            if awaits_in_own_body(&function.body) {
                "async "
            } else {
                ""
            },
            params.join(", "),
        ))
    }

    /// A function with a name of its own is a `function` form: the name is
    /// bound inside it, so an arrow (which has none) would lower to a
    /// different function.
    fn named_function(&self, name: &str, function: &FunctionExpr) -> Printed {
        if let Some(receiver) = &function.receiver {
            self.receivers.borrow_mut().insert(receiver.to_string());
        }
        let params = function
            .params
            .iter()
            .map(|param| self.identifier("function parameter", param.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut bound = function.params.iter().map(ToString::to_string).collect();
        Ok(format!(
            "{}function {}({}) {}",
            if awaits_in_own_body(&function.body) {
                "async "
            } else {
                ""
            },
            self.identifier("function", name)?,
            params.join(", "),
            self.rooted_block(&function.body, 0, &mut bound)?
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
            | Expr::MethodCall { .. }
            | Expr::Field { .. }
            | Expr::Index { .. } => self.expression(expression),
            _ => Ok(format!("({})", self.expression(expression)?)),
        }
    }

    /// An arrow is an AssignmentExpression, so as an operand it needs
    /// parentheses.
    fn binary_operand(&self, expression: &Expr) -> Printed {
        let printed = self.expression(expression)?;
        Ok(if matches!(expression, Expr::Function(_)) {
            format!("({printed})")
        } else {
            printed
        })
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

/// A process's authored parameters: a lifted literal's hidden start arguments
/// are not among them.
fn authored_params(process: &ProcessDecl) -> &[lashlang::ProcessParam] {
    let hidden = match &process.origin {
        lashlang::ProcessOrigin::Lifted { hidden_params, .. } => *hidden_params as usize,
        lashlang::ProcessOrigin::Declared => 0,
    };
    &process.params[..process.params.len().saturating_sub(hidden)]
}

/// The authored `run` body inside a process-wrapper body.
pub(super) fn process_run_body(process: &ProcessDecl) -> Option<&Expr> {
    crate::lower::wrapped_run_body(&process.body)
}

/// The authored `run` body inside a process *literal*'s wrapper.
pub(super) fn process_literal_run_body(literal: &ProcessLiteralExpr) -> Option<&Expr> {
    crate::lower::wrapped_run_body(&literal.body)
}

/// The statements of a body, in authored order, without the structure that
/// carries them: a completion list contributes its statements and never its
/// completion value.
pub(super) fn statement_block_contents(expression: &Expr) -> Vec<&Expr> {
    match expression {
        // The unit value a missing branch is spelled as holds no statement.
        Expr::Undefined => Vec::new(),
        expression => lashlang::statement_list(expression)
            .into_iter()
            .map(|listed| listed.expr)
            .filter(|statement| !matches!(statement, Expr::Undefined))
            .collect(),
    }
}

/// Whether `name = value` would not re-lower to this assignment: a bare
/// identifier target is a NamedEvaluation position, so it names an anonymous
/// function after the target — an assigned anonymous function whose `js_name`
/// is not the target's was authored `(name) = ..` and keeps its parentheses.
fn suppresses_named_evaluation(target: &AssignTarget, expr: &Expr) -> bool {
    target.is_simple()
        && matches!(expr, Expr::Function(function)
            if function.name.is_none()
                && function.js_name.as_deref() != Some(target.root.as_str()))
}

/// The `x op= rhs` a compound assignment on a bare name lowers to: the
/// assignment `x = x op rhs` closed by reading the target back, as the two
/// statements of one block. Only the operators with a compound spelling take
/// this shape: the comparisons and equalities spell `=` or `<`/`>` alone.
fn compound_assign_block(items: &[Expr]) -> Option<(&str, &str, &Expr)> {
    let [Expr::Assign { target, expr }, Expr::Variable(read)] = items else {
        return None;
    };
    let Expr::JavaScriptBinary { left, op, right } = expr.as_ref() else {
        return None;
    };
    (target.is_simple()
        && read.as_str() == target.root.as_str()
        && matches!(left.as_ref(), Expr::Variable(v) if v.as_str() == target.root.as_str()))
    .then(|| javascript_binary_op(*op))
    .filter(|op| !op.contains('=') && !matches!(*op, "<" | ">"))
    .map(|operator| (target.root.as_str(), operator, right.as_ref()))
}

/// The authored target spelling, assignment operator (`=`, or `op=` for an
/// update) and right-hand side of an attribute-assignment role.
fn attribute_assignment(
    expression: &Expr,
) -> Result<Option<(String, String, &Expr)>, TypeScriptSourceError> {
    let Expr::Role {
        role: StructuralRole::AttributeAssign,
        expr,
    } = expression
    else {
        return Ok(None);
    };
    let Some(parts) = lashlang::AttributeAssignParts::of(expr) else {
        return Ok(None);
    };
    let printer = Printer::plain();
    let object = printer.member_target(parts.object)?;
    let target = match parts.step {
        lashlang::AttributeStep::Field(field) => {
            format!("{object}.{}", printer.identifier("field", field.as_str())?)
        }
        lashlang::AttributeStep::Index(index) => {
            format!("{object}[{}]", printer.expression(index)?)
        }
    };
    Ok(Some(match parts.update {
        Some(update) => (
            target,
            format!("{}=", javascript_binary_op(update.operator.javascript_op())),
            update.operand,
        ),
        None => (target, "=".to_string(), parts.value),
    }))
}

/// The TypeScript annotation a process parameter type lowers from, or `None`
/// for `Any`, which an unannotated parameter lowers to. It inverts the
/// lowering's annotation conversion; a type no annotation lowers to is
/// refused rather than widened.
fn type_annotation(ty: &TypeExpr) -> Result<Option<String>, TypeScriptSourceError> {
    fn annotation(ty: &TypeExpr) -> Result<String, TypeScriptSourceError> {
        Ok(match ty {
            TypeExpr::Any => "unknown".to_string(),
            TypeExpr::Str => "string".to_string(),
            TypeExpr::Float => "number".to_string(),
            TypeExpr::Bool => "boolean".to_string(),
            TypeExpr::Null => "null".to_string(),
            TypeExpr::Enum(values) => values
                .iter()
                .map(|value| string_literal(value.as_str()))
                .collect::<Vec<_>>()
                .join(" | "),
            TypeExpr::List(item) => format!("Array<{}>", annotation(item)?),
            TypeExpr::Object(fields) => format!(
                "{{ {} }}",
                fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            "{}{}: {}",
                            property_name(field.name.as_str()),
                            if field.optional { "?" } else { "" },
                            annotation(&field.ty)?
                        ))
                    })
                    .collect::<Result<Vec<_>, TypeScriptSourceError>>()?
                    .join("; ")
            ),
            TypeExpr::Union(items) => items
                .iter()
                .map(annotation)
                .collect::<Result<Vec<_>, _>>()?
                .join(" | "),
            TypeExpr::Ref(name) => name.to_string(),
            _ => {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a process parameter type no TypeScript annotation lowers to",
                });
            }
        })
    }
    match ty {
        TypeExpr::Any => Ok(None),
        ty => annotation(ty).map(Some),
    }
}

/// Whether `expr` awaits in its own function body: a nested closure or process
/// body awaits on its own account.
fn awaits_in_own_body(expr: &Expr) -> bool {
    match expr {
        Expr::Await(_) => true,
        Expr::Function(_) | Expr::ProcessLiteral(_) => false,
        _ => expr.children().any(awaits_in_own_body),
    }
}

/// `let` when the loop body reassigns its element binding, `const` otherwise.
///
/// The lowerer erases the declaration kind — it copies the element into the
/// authored binding either way — so the kind is recovered from whether the body
/// writes the binding back, which is the only thing `const` would have refused.
fn element_binding_kind(body: &[&Expr], authored: &str) -> &'static str {
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

/// An authored loop header read off an iteration.
struct LoopHeader<'a> {
    binding: &'a str,
    source: &'a Expr,
    /// `for .. in` rather than `for .. of`.
    keys: bool,
}

/// The TypeScript header an iteration prints as.
///
/// An iteration with no bind names its element binding; one whose bind only
/// copies the element into one binding names that binding. Any other bind is
/// a destructuring pattern, which has no spelling here. The source is the
/// operand of the iteration protocol the lowerer emits (`Lash.ArrayFromIterable`
/// for `of`, `Object.keys` for `in`), or the iterable itself when a graph
/// supplies one directly.
fn loop_header<'a>(
    binding: &'a str,
    iterable: &'a Expr,
    bind: Option<&'a Expr>,
) -> Result<LoopHeader<'a>, TypeScriptSourceError> {
    let binding = match bind {
        None => binding,
        Some(Expr::Block(items)) => match items.as_slice() {
            [Expr::Assign { target, expr }]
                if target.is_simple()
                    && matches!(expr.as_ref(), Expr::Variable(name) if name.as_str() == binding) =>
            {
                target.root.as_str()
            }
            _ => {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a destructuring loop binding",
                });
            }
        },
        Some(_) => {
            return Err(TypeScriptSourceError::Unrepresentable {
                kind: "a loop bind that is not a statement list",
            });
        }
    };
    let (source, keys) = if let Some([source]) = stdlib_call(iterable, "Lash.ArrayFromIterable") {
        (source, false)
    } else if let Some([source]) = stdlib_call(iterable, "Object.keys") {
        (source, true)
    } else {
        (iterable, false)
    };
    Ok(LoopHeader {
        binding,
        source,
        keys,
    })
}

/// The arguments of a `__typescript_stdlib` call with the given selector.
pub(super) fn stdlib_call<'a>(expression: &'a Expr, selector: &str) -> Option<&'a [Expr]> {
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

/// A number literal by the one IR number rule: `NaN` and `Infinity` by
/// name (the lowering reads them back as the same literals), and every other
/// value in its shortest round-trip form. A negative literal has no spelling:
/// `-5` is the negation of `5`, which lowers to a different program than the
/// literal `-5` a constant (`Number.MIN_SAFE_INTEGER`) folds to, so it is
/// refused rather than printed as the negation (FIG-3599 round-trip law).
fn number_literal(value: f64) -> Printed {
    if value.is_sign_negative() && !value.is_nan() {
        return Err(TypeScriptSourceError::Unrepresentable {
            kind: "a negative number literal",
        });
    }
    Ok(if value.is_nan() {
        "NaN".to_string()
    } else if value == f64::INFINITY {
        "Infinity".to_string()
    } else {
        ryu_js::Buffer::new().format_finite(value).to_string()
    })
}

/// A type literal's property name: bare when it is an identifier, quoted
/// otherwise.
fn property_name(name: &str) -> String {
    let mut chars = name.chars();
    let identifier = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|rest| rest.is_ascii_alphanumeric() || rest == '_' || rest == '$');
    if identifier {
        name.to_string()
    } else {
        string_literal(name)
    }
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
        JavaScriptBinaryOp::BitAnd => "&",
        JavaScriptBinaryOp::BitOr => "|",
        JavaScriptBinaryOp::BitXor => "^",
        JavaScriptBinaryOp::ShiftLeft => "<<",
        JavaScriptBinaryOp::ShiftRight => ">>",
        JavaScriptBinaryOp::ShiftRightUnsigned => ">>>",
    }
}
