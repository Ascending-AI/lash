use lash_vm::{
    AssignPathStep, AssignTarget, CoercingBinaryOp, CoercingUnaryOp, Declaration, Expr,
    FunctionDecl, MethodKey, OperandLogicalOp, ProcessDecl, Program, StructuralRole,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use swc_ecma_ast::EsReserved;
use thiserror::Error;

mod collection_transform;
mod for_loop;
mod json_stringify;
mod loop_bindings;
mod patterns;
mod processes;
mod property_presence;
mod sparse_arrays;
mod sugar;
mod templates;

use for_loop::{classic_for, is_statement_body, var_initialization};
pub(super) use processes::process_literal_run_body;
use processes::{authored_params, process_return_annotation, process_run_body};
use sugar::{attribute_assignment, closure_function, is_closure_wrap};

use crate::LOWERED_BINDING_PREFIX;
use crate::node_label::render_label_comment;

/// Error returned when canonical IR has no TypeScript spelling.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TypeScriptSourceError {
    #[error("cannot render {kind} as TypeScript")]
    Unrepresentable { kind: &'static str },
    #[error("generated binding `{name}` has no authored TypeScript spelling")]
    GeneratedBinding { name: String },
    #[error("malformed sparse array helper: {reason}")]
    MalformedSparseArray { reason: &'static str },
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
    Printer::for_program(program).program(program)
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
    binding_names: RefCell<Vec<BTreeMap<String, String>>>,
}

impl Printer<'static> {
    fn plain() -> Self {
        Self {
            lifted: BTreeMap::new(),
            continue_epilogues: RefCell::new(Vec::new()),
            receivers: RefCell::new(BTreeSet::new()),
            binding_names: RefCell::new(Vec::new()),
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
            binding_names: RefCell::new(Vec::new()),
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
            .map(|param| self.binding_identifier("function parameter", param.name.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = format!(
            "function {}(",
            self.binding_identifier("function", function.name.as_str())?
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
                && matches!(expr.as_ref(), Expr::Absent)
                && !bound
                    .iter()
                    .any(|name| name.as_str() == target.root.as_str());
            if !hoist {
                break;
            }
            out.push_str(&format!(
                "{}var {};\n",
                indent(level),
                self.binding_identifier("var binding", target.root.as_str())?
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
        if let Some(sugared) = self.json_stringify(expression)? {
            return Ok(format!("{prefix}{sugared};\n"));
        }
        if let Some(sugared) = self.property_presence(expression)? {
            return Ok(format!("{prefix}{sugared};\n"));
        }
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
                        self.binding_identifier("var binding", target.root.as_str())?,
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
            Expr::Absent => Ok(String::new()),
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
                    let name = self.binding_identifier("binding", name)?;
                    let operand = self.expression(operand)?;
                    return Ok(format!("{prefix}{name} {operator}= {operand};\n"));
                }
                let mut inner = bound.clone();
                Ok(format!(
                    "{prefix}{}\n",
                    self.block(expression, level, &mut inner, vars)?
                ))
            }
            // A function bound to its own name is its declaration — under
            // the arity wrap a signature with defaults or a rest carries, or
            // bare.
            Expr::Assign { target, expr }
                if target.is_simple()
                    && let Some((function, closure)) = closure_function(expr.as_ref())
                    && function.name.as_deref() == Some(target.root.as_str()) =>
            {
                bound.push(target.root.to_string());
                Ok(format!(
                    "{prefix}{}\n",
                    self.named_function(target.root.as_str(), function, closure)?
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
                        self.binding_identifier("var binding", target.root.as_str())?,
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
                    Expr::Absent => {}
                    other if statement_block_contents(other).is_empty() => {}
                    other => {
                        let mut else_bound = bound.clone();
                        out.push_str(" else ");
                        // An `else if` chain lowers to a block holding the one
                        // nested `if`, so a block of that shape prints back as
                        // the chain the author wrote rather than a nested block.
                        match lash_vm::else_if_chain(other) {
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
                authored_binding,
                iterable,
                bind,
                body,
            } => {
                let header = loop_header(binding.as_str(), iterable, bind.as_deref())?;
                let declaration = if vars.contains(header.binding) {
                    "var"
                } else if bound.iter().any(|name| name == header.binding) {
                    ""
                } else {
                    element_binding_kind(&statement_block_contents(body), header.binding)
                };
                // The iterable resolves outside the loop's lexical binding.
                let source = self.expression(header.source)?;
                let printed_binding = self.loop_binding_name(
                    header.binding,
                    authored_binding.as_deref(),
                    body,
                    bound,
                )?;
                let mut body_bound = bound.clone();
                body_bound.push(header.binding.to_string());
                self.binding_names.borrow_mut().push(BTreeMap::from([(
                    header.binding.to_string(),
                    printed_binding.clone(),
                )]));
                let printed_body = self.block(body, level, &mut body_bound, vars);
                self.binding_names.borrow_mut().pop();
                Ok(format!(
                    "{prefix}for ({}{}{} {} {}) {}\n",
                    declaration,
                    if declaration.is_empty() { "" } else { " " },
                    printed_binding,
                    if header.keys { "in" } else { "of" },
                    source,
                    printed_body?,
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
                        self.binding_identifier("catch binding", catch.binding.as_str())?,
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
            Expr::FunctionReturn(value) => match value.as_ref() {
                Expr::Absent => Ok(format!("{prefix}return;\n")),
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
                        self.binding_identifier("var binding", target.root.as_str())?,
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
                let name = self.binding_identifier("loop binding", target.root.as_str())?;
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
            && let Expr::CoercingBinary {
                left,
                op: CoercingBinaryOp::Subtract,
                right,
            } = expr.as_ref()
            && matches!(left.as_ref(), Expr::Variable(name) if name.as_str() == target.root.as_str())
            && let Expr::Number(step) = right.as_ref()
            && (*step == -1.0 || *step == 1.0)
        {
            return Ok(format!(
                "{}{}",
                self.binding_identifier("loop binding", target.root.as_str())?,
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
            Expr::Absent => Ok("undefined".to_string()),
            Expr::Bool(value) => Ok(value.to_string()),
            Expr::Number(value) => number_literal(*value),
            Expr::String(value) => Ok(string_literal(value.as_str())),
            Expr::Variable(name) if self.receivers.borrow().contains(name.as_str()) => {
                Ok("this".to_string())
            }
            Expr::Variable(name) => self.binding_identifier("variable", name.as_str()),
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
            Expr::HostDescriptorConstructor { type_name, input } => {
                let path = type_name.split('.').collect::<Vec<_>>();
                if path.len() >= 2
                    && path.iter().all(|segment| {
                        is_typescript_identifier(segment) && !is_lowered_binding(segment)
                    })
                {
                    Ok(format!("{type_name}({})", self.expression(input)?))
                } else {
                    Err(TypeScriptSourceError::UnknownHostDescriptorConstructor {
                        type_name: type_name.to_string(),
                    })
                }
            }
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let args = self.arguments(args)?;
                Ok(format!(
                    "{}.{}({})",
                    self.member_target(receiver)?,
                    self.identifier("operation", operation.as_str())?,
                    args
                ))
            }
            Expr::Await(value) => Ok(format!("await {}", self.unary_operand(value)?)),
            Expr::SleepFor(value) => Ok(format!("await sleep({})", self.expression(value)?)),
            Expr::Print(value) => Ok(format!("print({})", self.expression(value)?)),
            Expr::Finish(value) => Ok(format!("finish({})", self.expression(value)?)),
            Expr::Fail(value) => Ok(format!("fail({})", self.expression(value)?)),
            Expr::FunctionCall { function, args } => {
                let args = self.arguments(args)?;
                Ok(format!(
                    "{}({})",
                    self.binding_identifier("function", function.as_str())?,
                    args
                ))
            }
            Expr::Call { function, args } => {
                let args = self.arguments(args)?;
                Ok(format!("{}({})", self.member_target(function)?, args))
            }
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => {
                let args = self.arguments(args)?;
                let receiver = self.member_target(receiver)?;
                Ok(match method {
                    MethodKey::Field(field) => format!(
                        "{receiver}.{}({})",
                        self.identifier("method", field.as_str())?,
                        args
                    ),
                    MethodKey::Index(key) => {
                        format!("{receiver}[{}]({})", self.expression(key)?, args)
                    }
                })
            }
            // A builtin's explicit-receiver call — the callback `thisArg` of
            // a collection transform or a replacer's holder. `f["call"](t,
            // ..)` reads the `call` builtin from the function value and
            // invokes it with `f` bound as its receiver, so `call`'s own
            // `this` is the function and its first argument is the explicit
            // receiver — the same evaluation `CallMethod` performs. The
            // spelling evaluates `function` before `this`, the reverse of
            // the node's order; every reachable `ThisCall` holds generated
            // slots in both. Re-lowering yields a computed `MethodCall`,
            // not this node — the spelling is evaluation-equal, not
            // canonical.
            Expr::ThisCall {
                this,
                function,
                args,
            } => {
                let mut arguments = self.expression(this)?;
                for arg in args {
                    arguments.push_str(", ");
                    arguments.push_str(&self.expression(arg)?);
                }
                Ok(format!(
                    "{}[\"call\"]({arguments})",
                    self.member_target(function)?
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
                    .map(|param| self.process_param(param))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bound = literal
                    .params
                    .iter()
                    .map(|param| param.name.to_string())
                    .collect();
                Ok(format!(
                    "async ({}){} => {}",
                    params.join(", "),
                    process_return_annotation(literal.return_ty.as_ref())?,
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
            Expr::CoercingUnary { op, expr } => {
                let op = match op {
                    CoercingUnaryOp::Plus => "+",
                    CoercingUnaryOp::Negate => "-",
                    CoercingUnaryOp::Not => "!",
                    CoercingUnaryOp::TypeOf => "typeof ",
                    CoercingUnaryOp::BitNot => "~",
                    CoercingUnaryOp::ToString => {
                        return Ok(format!("String({})", self.expression(expr)?));
                    }
                };
                Ok(format!("{op}{}", self.unary_operand(expr)?))
            }
            Expr::CoercingBinary { left, op, right } => Ok(format!(
                "({} {} {})",
                self.binary_operand(left)?,
                javascript_binary_op(*op),
                self.binary_operand(right)?
            )),
            Expr::OperandLogical { left, op, right } => Ok(format!(
                "({} {} {})",
                self.expression(left)?,
                match op {
                    OperandLogicalOp::And => "&&",
                    OperandLogicalOp::Or => "||",
                    OperandLogicalOp::NullishCoalesce => "??",
                },
                self.expression(right)?
            )),
            Expr::BuiltinCall { name, args } => {
                let args = self.arguments(args)?;
                Ok(format!(
                    "{}({})",
                    self.identifier("builtin", name.as_str())?,
                    args
                ))
            }
            Expr::Block(_)
            | Expr::Role {
                role: StructuralRole::JsonTraversal,
                ..
            } => Err(TypeScriptSourceError::Unrepresentable {
                kind: "a block in expression position",
            }),
            // A role is transparent: it executes exactly as the expression it
            // wraps. A role whose own recognizer did not fire — a malformed
            // shape, or a scope/completion that names no operand value —
            // prints as its inner expression and lets the inner form decide.
            Expr::Role { expr, .. } => self.expression(expr),
            // The label annotates a node for the graph; in operand position
            // the node it names is what evaluates.
            Expr::LabelAnnotated { expr, .. } => self.expression(expr),
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
            Expr::Throw(_) | Expr::FunctionReturn(_) => {
                Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a jump in expression position",
                })
            }
            // A failed host operation throws in TypeScript, so the unwrap the
            // lowerer wraps every module call in has no spelling of its own.
            Expr::ResultUnwrap(value) => self.expression(value),
            // The map intrinsic drives the callback once per item, which is
            // the same traversal `items.map(fn)` lowers to. The spelling
            // re-lowers to the collection-transform role around this node —
            // evaluation-equal, not canonical — so every reachable `Map` the
            // lowerer itself emits still prints inside the shape that owns
            // it, and only a bare intrinsic reaches this arm.
            Expr::Map { items, function } => Ok(format!(
                "{}.map({})",
                self.member_target(items)?,
                self.expression(function)?
            )),
        }
    }

    fn member_target(&self, expression: &Expr) -> Printed {
        match expression {
            Expr::Null
            | Expr::Absent
            | Expr::Bool(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::List(_)
            | Expr::Record(_)
            | Expr::ProcessRef { .. }
            | Expr::ResourceRef(_)
            | Expr::ReceiverCall { .. }
            | Expr::FunctionCall { .. }
            | Expr::Call { .. }
            | Expr::MethodCall { .. }
            | Expr::Field { .. }
            | Expr::Index { .. } => self.expression(expression),
            // A `__lash_vm_closure` wrap prints as the arrow it carries,
            // an AssignmentExpression, so it needs parentheses as a member
            // target the same way `Expr::Function` does.
            Expr::BuiltinCall { .. } if !is_closure_wrap(expression) => self.expression(expression),
            _ => Ok(format!("({})", self.expression(expression)?)),
        }
    }

    /// An arrow is an AssignmentExpression, so as an operand it needs
    /// parentheses.
    fn binary_operand(&self, expression: &Expr) -> Printed {
        let printed = self.expression(expression)?;
        Ok(
            if matches!(expression, Expr::Function(_)) || is_closure_wrap(expression) {
                format!("({printed})")
            } else {
                printed
            },
        )
    }

    fn unary_operand(&self, expression: &Expr) -> Printed {
        match expression {
            // The unwrap has no TypeScript spelling, so its operand owns
            // precedence too: an awaited host call needs no parentheses.
            Expr::ResultUnwrap(value) => self.unary_operand(value),
            Expr::CoercingBinary { .. } | Expr::OperandLogical { .. } | Expr::If { .. } => {
                self.expression(expression)
            }
            _ => self.member_target(expression),
        }
    }

    fn assign_target(&self, target: &AssignTarget) -> Printed {
        let mut out = self.binding_identifier("assignment target", target.root.as_str())?;
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
        if is_lowered_binding(name) {
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

/// The lens owns the lowerer's private binding namespace. Re-sugaring uses
/// this only alongside the lowered IR shape, and otherwise refuses to spell
/// a private slot as an authored identifier.
fn is_lowered_binding(name: &str) -> bool {
    name.starts_with(LOWERED_BINDING_PREFIX)
}

/// The statements of a body, in authored order, without the structure that
/// carries them: a completion list contributes its statements and never its
/// completion value.
pub(super) fn statement_block_contents(expression: &Expr) -> Vec<&Expr> {
    match expression {
        // The unit value a missing branch is spelled as holds no statement.
        Expr::Absent => Vec::new(),
        expression => lash_vm::statement_list(expression)
            .into_iter()
            .map(|listed| listed.expr)
            .filter(|statement| !matches!(statement, Expr::Absent))
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
    let Expr::CoercingBinary { left, op, right } = expr.as_ref() else {
        return None;
    };
    (target.is_simple()
        && read.as_str() == target.root.as_str()
        && matches!(left.as_ref(), Expr::Variable(v) if v.as_str() == target.root.as_str()))
    .then(|| javascript_binary_op(*op))
    .filter(|op| !op.contains('=') && !matches!(*op, "<" | ">"))
    .map(|operator| (target.root.as_str(), operator, right.as_ref()))
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

/// The arguments of a `__lash_vm_stdlib` call with the given selector.
pub(super) fn stdlib_call<'a>(expression: &'a Expr, selector: &str) -> Option<&'a [Expr]> {
    let Expr::BuiltinCall { name, args } = expression else {
        return None;
    };
    if name.as_str() != "__lash_vm_stdlib" {
        return None;
    }
    let [Expr::String(found), rest @ ..] = args.as_slice() else {
        return None;
    };
    (found.as_str() == selector).then_some(rest)
}

/// The doc comment a label is spelled as, or the refusal when its text has no
/// spelling that would read back as the same label.
fn label_comment(label: &lash_vm::LabelMetadata) -> Printed {
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

pub(super) fn is_typescript_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        })
        && !name.is_reserved()
        && !name.is_reserved_in_strict_mode(true)
}

fn javascript_binary_op(op: CoercingBinaryOp) -> &'static str {
    match op {
        CoercingBinaryOp::Add => "+",
        CoercingBinaryOp::Subtract => "-",
        CoercingBinaryOp::Multiply => "*",
        CoercingBinaryOp::Divide => "/",
        CoercingBinaryOp::Remainder => "%",
        CoercingBinaryOp::StrictEqual => "===",
        CoercingBinaryOp::StrictNotEqual => "!==",
        CoercingBinaryOp::LooseEqual => "==",
        CoercingBinaryOp::LooseNotEqual => "!=",
        CoercingBinaryOp::Less => "<",
        CoercingBinaryOp::LessEqual => "<=",
        CoercingBinaryOp::Greater => ">",
        CoercingBinaryOp::GreaterEqual => ">=",
        CoercingBinaryOp::BitAnd => "&",
        CoercingBinaryOp::BitOr => "|",
        CoercingBinaryOp::BitXor => "^",
        CoercingBinaryOp::ShiftLeft => "<<",
        CoercingBinaryOp::ShiftRight => ">>",
        CoercingBinaryOp::ShiftRightUnsigned => ">>>",
    }
}
