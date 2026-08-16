use thiserror::Error;

use crate::HostRequirements;
use crate::ast::{
    AssignPathStep, AssignTarget, BinaryOp, Declaration, Expr, LabelMetadata,
    ListComprehensionClause, ProcessDecl, Program, ResourceRefExpr, TypeExpr, TypeField, UnaryOp,
};

/// Error returned when canonical IR cannot be represented as Lashlang source.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CanonicalSourceError {
    #[error("invalid {context} identifier `{name}`")]
    InvalidIdentifier { context: &'static str, name: String },
    #[error("invalid {context} path `{path}`")]
    InvalidPath { context: &'static str, path: String },
    #[error("cannot render non-sourceable {kind} expression")]
    NonSourceableExpression { kind: &'static str },
    #[error("cannot render non-sourceable {kind} type")]
    NonSourceableType { kind: &'static str },
    #[error("cannot render number literal `{value}` as canonical Lashlang source")]
    UnsupportedNumber { value: String },
    #[error(
        "cannot render host descriptor constructor `{type_name}` without an unambiguous constructor path"
    )]
    UnknownHostDescriptorConstructor { type_name: String },
    #[error(
        "cannot render host descriptor constructor `{type_name}` because multiple constructor paths match: {paths:?}"
    )]
    AmbiguousHostDescriptorConstructor {
        type_name: String,
        paths: Vec<String>,
    },
}

/// Pretty-print a canonical Lashlang program as source.
///
/// This is a view of the IR, not the original authored text: comments and
/// formatting are not preserved. If the program was linked and contains host
/// descriptor constructors, use [`canonical_program_source_with_requirements`]
/// so constructor paths can be recovered from the saved host requirements.
pub fn canonical_program_source(program: &Program) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(None).program_source(program)
}

/// Pretty-print one expression as canonical Lashlang source.
///
/// This is the textual form used by editable workflow-graph expression fields.
pub fn canonical_expression_source(expression: &Expr) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(None).statement_expr_source(expression)
}

/// Pretty-print one assignment target as canonical Lashlang source.
///
/// This is the textual form used by editable workflow-graph binding and target fields.
pub fn canonical_assign_target_source(
    target: &AssignTarget,
) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(None).assign_target_source(target)
}

/// Pretty-print a canonical Lashlang program using the host requirements saved
/// with the artifact.
pub fn canonical_program_source_with_requirements(
    program: &Program,
    requirements: &HostRequirements,
) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(Some(requirements)).program_source(program)
}

/// Pretty-print one process definition as a focused source fragment.
///
/// The returned fragment is the process declaration itself. It may reference
/// type declarations, host constructors, resources, or other processes that are
/// only self-contained in the full module source.
pub fn canonical_process_source(process: &ProcessDecl) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(None).process_source(process)
}

/// Pretty-print one process definition using the host requirements saved with
/// the artifact.
pub fn canonical_process_source_with_requirements(
    process: &ProcessDecl,
    requirements: &HostRequirements,
) -> Result<String, CanonicalSourceError> {
    SourceFormatter::new(Some(requirements)).process_source(process)
}

struct SourceFormatter<'a> {
    requirements: Option<&'a HostRequirements>,
}

impl<'a> SourceFormatter<'a> {
    fn new(requirements: Option<&'a HostRequirements>) -> Self {
        Self { requirements }
    }

    fn program_source(&self, program: &Program) -> Result<String, CanonicalSourceError> {
        let mut sections = Vec::new();
        for declaration in &program.declarations {
            sections.push(self.declaration_source(declaration)?);
        }

        let main = self.main_source(&program.main)?;
        if !main.is_empty() {
            sections.push(main);
        }

        Ok(finish_source(sections))
    }

    fn process_source(&self, process: &ProcessDecl) -> Result<String, CanonicalSourceError> {
        let mut out = String::new();
        self.write_process(&mut out, process)?;
        if !out.is_empty() {
            out.push('\n');
        }
        Ok(out)
    }

    fn declaration_source(
        &self,
        declaration: &Declaration,
    ) -> Result<String, CanonicalSourceError> {
        match declaration {
            Declaration::Type(type_decl) => {
                let mut out = String::new();
                out.push_str("type ");
                out.push_str(&format_identifier("type name", type_decl.name.as_str())?);
                out.push_str(" = ");
                out.push_str(&self.type_source(&type_decl.ty)?);
                Ok(out)
            }
            Declaration::Process(process) => {
                let mut out = String::new();
                self.write_process(&mut out, process)?;
                Ok(out)
            }
        }
    }

    fn write_process(
        &self,
        out: &mut String,
        process: &ProcessDecl,
    ) -> Result<(), CanonicalSourceError> {
        if let Some(label) = &process.label {
            out.push_str(&label_source(label));
            out.push('\n');
        }
        out.push_str("process ");
        out.push_str(&format_identifier("process name", process.name.as_str())?);
        out.push('(');
        for (index, param) in process.params.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format_identifier(
                "process parameter",
                param.name.as_str(),
            )?);
            out.push_str(": ");
            out.push_str(&self.type_source(&param.ty)?);
        }
        out.push(')');
        if !process.signals.is_empty() {
            out.push_str(" signals { ");
            for (index, signal) in process.signals.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format_key_name(signal.name.as_str()));
                out.push_str(": ");
                out.push_str(&self.type_source(&signal.ty)?);
            }
            out.push_str(" }");
        }
        if let Some(return_ty) = &process.return_ty {
            out.push_str(" -> ");
            out.push_str(&self.type_source(return_ty)?);
        }
        out.push(' ');
        out.push_str(&self.block_source(&process.body, 0)?);
        Ok(())
    }

    fn main_source(&self, main: &Expr) -> Result<String, CanonicalSourceError> {
        match main {
            Expr::Block(expressions) => {
                let mut out = String::new();
                self.write_statements(&mut out, expressions, 0)?;
                Ok(trim_trailing_newline(out))
            }
            expr => self.statement_line(expr, 0).map(trim_trailing_newline),
        }
    }

    fn block_source(&self, expr: &Expr, indent: usize) -> Result<String, CanonicalSourceError> {
        let Expr::Block(expressions) = expr else {
            let mut out = String::new();
            out.push_str("{\n");
            out.push_str(&self.statement_line(expr, indent + 1)?);
            out.push_str(&indent_string(indent));
            out.push('}');
            return Ok(out);
        };
        if expressions.is_empty() {
            return Ok("{}".to_string());
        }
        let mut out = String::new();
        out.push_str("{\n");
        self.write_statements(&mut out, expressions, indent + 1)?;
        out.push_str(&indent_string(indent));
        out.push('}');
        Ok(out)
    }

    fn write_statements(
        &self,
        out: &mut String,
        expressions: &[Expr],
        indent: usize,
    ) -> Result<(), CanonicalSourceError> {
        for expr in expressions {
            out.push_str(&self.statement_line(expr, indent)?);
        }
        Ok(())
    }

    fn statement_line(&self, expr: &Expr, indent: usize) -> Result<String, CanonicalSourceError> {
        let prefix = indent_string(indent);
        let mut out = String::new();
        match expr {
            Expr::LabelAnnotated { label, expr } => {
                out.push_str(&prefix);
                out.push_str(&label_source(label));
                out.push('\n');
                out.push_str(&self.statement_line(expr, indent)?);
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } if is_statement_block(then_block) => {
                out.push_str(&prefix);
                out.push_str(&self.if_statement_source(condition, then_block, else_block, indent)?);
                out.push('\n');
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                out.push_str(&prefix);
                out.push_str("for ");
                out.push_str(&format_identifier("for binding", binding.as_str())?);
                out.push_str(" in ");
                out.push_str(&self.expr_source(iterable)?);
                out.push(' ');
                out.push_str(&self.block_source(body, indent)?);
                out.push('\n');
            }
            Expr::While { condition, body } => {
                out.push_str(&prefix);
                out.push_str("while ");
                out.push_str(&self.expr_source(condition)?);
                out.push(' ');
                out.push_str(&self.block_source(body, indent)?);
                out.push('\n');
            }
            _ => {
                out.push_str(&prefix);
                out.push_str(&self.statement_expr_source(expr)?);
                out.push('\n');
            }
        }
        Ok(out)
    }

    fn if_statement_source(
        &self,
        condition: &Expr,
        then_block: &Expr,
        else_block: &Expr,
        indent: usize,
    ) -> Result<String, CanonicalSourceError> {
        let mut out = String::new();
        out.push_str("if ");
        out.push_str(&self.expr_source(condition)?);
        out.push(' ');
        out.push_str(&self.block_source(then_block, indent)?);
        match else_block {
            Expr::Block(expressions) if expressions.is_empty() => {}
            Expr::If {
                condition,
                then_block,
                else_block,
            } if is_statement_block(then_block) => {
                out.push_str(" else ");
                out.push_str(&self.if_statement_source(condition, then_block, else_block, indent)?);
            }
            _ => {
                out.push_str(" else ");
                out.push_str(&self.block_source(else_block, indent)?);
            }
        }
        Ok(out)
    }

    fn statement_expr_source(&self, expr: &Expr) -> Result<String, CanonicalSourceError> {
        match expr {
            Expr::Assign { target, expr } => {
                let mut out = self.assign_target_source(target)?;
                out.push_str(" = ");
                out.push_str(&self.expr_source(expr)?);
                Ok(out)
            }
            Expr::Break => Ok("break".to_string()),
            Expr::Continue => Ok("continue".to_string()),
            Expr::Cancel(expr) => Ok(format!("cancel {}", self.expr_source(expr)?)),
            Expr::Print(expr) => Ok(format!("print {}", self.expr_source(expr)?)),
            Expr::Yield(expr) => Ok(format!("yield {}", self.expr_source(expr)?)),
            Expr::Wake(expr) => Ok(format!("wake {}", self.expr_source(expr)?)),
            Expr::Finish(expr) => Ok(format!("finish {}", self.expr_source(expr)?)),
            Expr::Fail(expr) => Ok(format!("fail {}", self.expr_source(expr)?)),
            Expr::Block(_) => {
                Err(CanonicalSourceError::NonSourceableExpression { kind: "bare block" })
            }
            expr => self.expr_source(expr),
        }
    }

    fn expr_source(&self, expr: &Expr) -> Result<String, CanonicalSourceError> {
        match expr {
            Expr::Block(_) => Err(CanonicalSourceError::NonSourceableExpression { kind: "block" }),
            Expr::LabelAnnotated { .. } => Err(CanonicalSourceError::NonSourceableExpression {
                kind: "label-annotated expression",
            }),
            Expr::Null => Ok("null".to_string()),
            Expr::Bool(value) => Ok(value.to_string()),
            Expr::Number(value) => format_number(*value),
            Expr::String(value) => Ok(format_string(value.as_str())),
            Expr::Variable(name) => format_identifier("variable", name.as_str()),
            Expr::Tuple(items) => Ok(self.tuple_source(items)?),
            Expr::List(items) => {
                let items = items
                    .iter()
                    .map(|item| self.expr_source(item))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!("[{}]", items.join(", ")))
            }
            Expr::ListComprehension { element, clauses } => {
                let mut out = String::new();
                out.push('[');
                out.push_str(&self.expr_source(element)?);
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            out.push_str(" for ");
                            out.push_str(&format_identifier("comprehension binding", binding)?);
                            out.push_str(" in ");
                            out.push_str(&self.expr_source(iterable)?);
                        }
                        ListComprehensionClause::If { condition } => {
                            out.push_str(" if ");
                            out.push_str(&self.expr_source(condition)?);
                        }
                    }
                }
                out.push(']');
                Ok(out)
            }
            Expr::Record(entries) if is_trigger_event_placeholder(entries) => {
                Ok("trigger.event".to_string())
            }
            Expr::Record(entries) => {
                let entries = entries
                    .iter()
                    .map(|(key, value)| {
                        Ok(format!(
                            "{}: {}",
                            format_key_name(key.as_str()),
                            self.expr_source(value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, CanonicalSourceError>>()?;
                Ok(format!("{{ {} }}", entries.join(", ")))
            }
            Expr::Assign { .. } => {
                Err(CanonicalSourceError::NonSourceableExpression { kind: "assignment" })
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                if is_statement_block(then_block) || is_statement_block(else_block) {
                    return Err(CanonicalSourceError::NonSourceableExpression {
                        kind: "statement if",
                    });
                }
                Ok(format!(
                    "({} ? {} : {})",
                    self.expr_source(condition)?,
                    self.expr_source(then_block)?,
                    self.expr_source(else_block)?
                ))
            }
            Expr::For { .. } => Err(CanonicalSourceError::NonSourceableExpression { kind: "for" }),
            Expr::While { .. } => {
                Err(CanonicalSourceError::NonSourceableExpression { kind: "while" })
            }
            Expr::Break => Err(CanonicalSourceError::NonSourceableExpression { kind: "break" }),
            Expr::Continue => {
                Err(CanonicalSourceError::NonSourceableExpression { kind: "continue" })
            }
            Expr::StartProcess(start) => {
                let args = start
                    .args
                    .iter()
                    .map(|(key, value)| {
                        Ok(format!(
                            "{}: {}",
                            format_key_name(key.as_str()),
                            self.expr_source(value)?
                        ))
                    })
                    .collect::<Result<Vec<_>, CanonicalSourceError>>()?;
                Ok(format!(
                    "start {}({})",
                    format_identifier("process name", start.process.as_str())?,
                    args.join(", ")
                ))
            }
            Expr::ProcessRef { process } => format_identifier("process name", process.as_str()),
            Expr::HostDescriptorConstructor { type_name, input } => {
                let path = self.constructor_path(type_name.as_str())?;
                Ok(format!(
                    "{}({})",
                    format_receiver_path("constructor", &path)?,
                    self.expr_source(input)?
                ))
            }
            Expr::ResourceRef(resource) => self.resource_ref_source(resource),
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr_source(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}.{}({})",
                    self.postfix_target_source(receiver)?,
                    format_key_name(operation.as_str()),
                    args.join(", ")
                ))
            }
            Expr::Await(expr) => Ok(format!("await {}", self.unary_operand_source(expr)?)),
            Expr::SleepFor(expr) => Ok(format!("sleep for {}", self.expr_source(expr)?)),
            Expr::SleepUntil(expr) => Ok(format!("sleep until {}", self.expr_source(expr)?)),
            Expr::WaitSignal { name } => Ok(format!("wait_signal({})", format_string(name))),
            Expr::SignalRun { run, name, payload } => Ok(format!(
                "signal_run({}, {}, {})",
                self.expr_source(run)?,
                format_string(name),
                self.expr_source(payload)?
            )),
            Expr::ResultUnwrap(expr) => Ok(format!("{}?", self.postfix_target_source(expr)?)),
            Expr::Cancel(expr) => Ok(format!("cancel {}", self.expr_source(expr)?)),
            Expr::Print(expr) => Ok(format!("print {}", self.expr_source(expr)?)),
            Expr::Yield(expr) => Ok(format!("yield {}", self.expr_source(expr)?)),
            Expr::Wake(expr) => Ok(format!("wake {}", self.expr_source(expr)?)),
            Expr::Finish(expr) => Ok(format!("finish {}", self.expr_source(expr)?)),
            Expr::Fail(expr) => Ok(format!("fail {}", self.expr_source(expr)?)),
            Expr::BuiltinCall { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr_source(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "{}({})",
                    format_identifier("builtin", name.as_str())?,
                    args.join(", ")
                ))
            }
            Expr::Function(_) => Err(CanonicalSourceError::NonSourceableExpression {
                kind: "AST-only function",
            }),
            Expr::Call { .. } => Err(CanonicalSourceError::NonSourceableExpression {
                kind: "AST-only function call",
            }),
            Expr::Map { .. } => Err(CanonicalSourceError::NonSourceableExpression {
                kind: "AST-only function map",
            }),
            Expr::Field { target, field } => Ok(format!(
                "{}.{}",
                self.postfix_target_source(target)?,
                format_key_name(field.as_str())
            )),
            Expr::Index { target, index } => Ok(format!(
                "{}[{}]",
                self.postfix_target_source(target)?,
                self.expr_source(index)?
            )),
            Expr::Unary { op, expr } => {
                let op = match op {
                    UnaryOp::Negate => "-",
                    UnaryOp::Not => "not ",
                };
                Ok(format!("{op}{}", self.unary_operand_source(expr)?))
            }
            Expr::Binary { left, op, right } => Ok(format!(
                "({} {} {})",
                self.expr_source(left)?,
                binary_op_source(*op),
                self.expr_source(right)?
            )),
            Expr::TypeLiteral(ty) => Ok(format!("Type {}", self.type_source(ty)?)),
        }
    }

    fn postfix_target_source(&self, expr: &Expr) -> Result<String, CanonicalSourceError> {
        match expr {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::Tuple(_)
            | Expr::List(_)
            | Expr::ListComprehension { .. }
            | Expr::Record(_)
            | Expr::StartProcess(_)
            | Expr::ProcessRef { .. }
            | Expr::HostDescriptorConstructor { .. }
            | Expr::ResourceRef(_)
            | Expr::ReceiverCall { .. }
            | Expr::BuiltinCall { .. }
            | Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::ResultUnwrap(_)
            | Expr::TypeLiteral(_) => self.expr_source(expr),
            _ => Ok(format!("({})", self.expr_source(expr)?)),
        }
    }

    fn unary_operand_source(&self, expr: &Expr) -> Result<String, CanonicalSourceError> {
        match expr {
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Variable(_)
            | Expr::Tuple(_)
            | Expr::List(_)
            | Expr::ListComprehension { .. }
            | Expr::Record(_)
            | Expr::StartProcess(_)
            | Expr::ProcessRef { .. }
            | Expr::HostDescriptorConstructor { .. }
            | Expr::ResourceRef(_)
            | Expr::ReceiverCall { .. }
            | Expr::BuiltinCall { .. }
            | Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::ResultUnwrap(_)
            | Expr::Unary { .. }
            | Expr::TypeLiteral(_) => self.expr_source(expr),
            _ => Ok(format!("({})", self.expr_source(expr)?)),
        }
    }

    fn assign_target_source(&self, target: &AssignTarget) -> Result<String, CanonicalSourceError> {
        let mut out = format_identifier("assignment target", target.root.as_str())?;
        for step in &target.steps {
            match step {
                AssignPathStep::Field(field) => {
                    out.push('.');
                    out.push_str(&format_key_name(field.as_str()));
                }
                AssignPathStep::Index(index) => {
                    out.push('[');
                    out.push_str(&self.expr_source(index)?);
                    out.push(']');
                }
            }
        }
        Ok(out)
    }

    fn tuple_source(&self, items: &[Expr]) -> Result<String, CanonicalSourceError> {
        let items = items
            .iter()
            .map(|item| self.expr_source(item))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(match items.as_slice() {
            [] => "()".to_string(),
            [item] => format!("({item},)"),
            items => format!("({})", items.join(", ")),
        })
    }

    fn type_source(&self, ty: &TypeExpr) -> Result<String, CanonicalSourceError> {
        match ty {
            TypeExpr::Any => Ok("any".to_string()),
            TypeExpr::Str => Ok("str".to_string()),
            TypeExpr::Int => Ok("int".to_string()),
            TypeExpr::Float => Ok("float".to_string()),
            TypeExpr::Bool => Ok("bool".to_string()),
            TypeExpr::Dict => Ok("dict".to_string()),
            TypeExpr::Null => Ok("null".to_string()),
            TypeExpr::Enum(values) if values.is_empty() => {
                Err(CanonicalSourceError::NonSourceableType { kind: "empty enum" })
            }
            TypeExpr::Enum(values) => Ok(format!(
                "enum[{}]",
                values
                    .iter()
                    .map(|value| format_string(value.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            TypeExpr::List(item) => Ok(format!("list[{}]", self.type_source(item)?)),
            TypeExpr::Object(fields) => self.object_type_source(fields),
            TypeExpr::Ref(name) => format_type_ref(name.as_str()),
            TypeExpr::Process { input_count, .. } if *input_count != 1 => {
                Err(CanonicalSourceError::NonSourceableType {
                    kind: "multi-input process",
                })
            }
            TypeExpr::Process { input, output, .. } => Ok(format!(
                "Process<{}, {}>",
                self.type_source(input)?,
                self.type_source(output)?
            )),
            TypeExpr::TriggerHandle(event) => {
                Ok(format!("TriggerHandle<{}>", self.type_source(event)?))
            }
            TypeExpr::Union(items) if items.len() < 2 => {
                Err(CanonicalSourceError::NonSourceableType {
                    kind: "single-variant union",
                })
            }
            TypeExpr::Union(items) => {
                let items = items
                    .iter()
                    .map(|item| self.type_source(item))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(items.join(" | "))
            }
        }
    }

    fn object_type_source(&self, fields: &[TypeField]) -> Result<String, CanonicalSourceError> {
        let fields = fields
            .iter()
            .map(|field| {
                let optional = if field.optional { "?" } else { "" };
                Ok(format!(
                    "{}: {}{}",
                    format_key_name(field.name.as_str()),
                    self.type_source(&field.ty)?,
                    optional
                ))
            })
            .collect::<Result<Vec<_>, CanonicalSourceError>>()?;
        Ok(format!("{{ {} }}", fields.join(", ")))
    }

    fn resource_ref_source(
        &self,
        resource: &ResourceRefExpr,
    ) -> Result<String, CanonicalSourceError> {
        if !resource.path.is_empty() {
            return format_receiver_path(
                "resource",
                &resource
                    .path
                    .iter()
                    .map(|segment| segment.to_string())
                    .collect::<Vec<_>>(),
            );
        }
        if resource.alias.is_empty() {
            return Err(CanonicalSourceError::InvalidPath {
                context: "resource",
                path: String::new(),
            });
        }
        format_receiver_path(
            "resource",
            &resource
                .alias
                .split('.')
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        )
    }

    fn constructor_path(&self, type_name: &str) -> Result<Vec<String>, CanonicalSourceError> {
        let Some(requirements) = self.requirements else {
            return Err(CanonicalSourceError::UnknownHostDescriptorConstructor {
                type_name: type_name.to_string(),
            });
        };
        let paths = requirements
            .resources
            .value_constructors()
            .filter(|(_, constructor)| constructor.type_name == type_name)
            .map(|(_, constructor)| constructor.path.clone())
            .collect::<Vec<_>>();
        match paths.as_slice() {
            [path] => Ok(path.clone()),
            [] => Err(CanonicalSourceError::UnknownHostDescriptorConstructor {
                type_name: type_name.to_string(),
            }),
            paths => Err(CanonicalSourceError::AmbiguousHostDescriptorConstructor {
                type_name: type_name.to_string(),
                paths: paths.iter().map(|path| path.join(".")).collect::<Vec<_>>(),
            }),
        }
    }
}

fn finish_source(sections: Vec<String>) -> String {
    let mut out = sections.join("\n\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn trim_trailing_newline(mut out: String) -> String {
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn indent_string(indent: usize) -> String {
    "  ".repeat(indent)
}

fn is_statement_block(expr: &Expr) -> bool {
    matches!(expr, Expr::Block(_))
}

fn is_trigger_event_placeholder(entries: &[(crate::ast::AstString, Expr)]) -> bool {
    matches!(
        entries,
        [(key, Expr::Bool(true))] if key.as_str() == crate::trigger::LASH_TRIGGER_EVENT_KEY
    )
}

fn label_source(label: &LabelMetadata) -> String {
    let mut out = String::from("@label(title: ");
    out.push_str(&format_string(label.title.as_str()));
    if let Some(description) = &label.description {
        out.push_str(", description: ");
        out.push_str(&format_string(description.as_str()));
    }
    out.push(')');
    out
}

fn format_number(value: f64) -> Result<String, CanonicalSourceError> {
    if !value.is_finite() || value.is_sign_negative() {
        return Err(CanonicalSourceError::UnsupportedNumber {
            value: value.to_string(),
        });
    }
    Ok(value.to_string())
}

fn format_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
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

fn format_identifier(context: &'static str, name: &str) -> Result<String, CanonicalSourceError> {
    if is_identifier(name) {
        return Ok(name.to_string());
    }
    Err(CanonicalSourceError::InvalidIdentifier {
        context,
        name: name.to_string(),
    })
}

fn format_key_name(name: &str) -> String {
    if is_bare_key(name) {
        name.to_string()
    } else {
        format_string(name)
    }
}

fn format_type_ref(name: &str) -> Result<String, CanonicalSourceError> {
    let segments = name.split('.').collect::<Vec<_>>();
    if segments.iter().all(|segment| is_identifier(segment)) {
        return Ok(name.to_string());
    }
    Err(CanonicalSourceError::InvalidPath {
        context: "type reference",
        path: name.to_string(),
    })
}

fn format_receiver_path(
    context: &'static str,
    path: &[String],
) -> Result<String, CanonicalSourceError> {
    let Some((root, rest)) = path.split_first() else {
        return Err(CanonicalSourceError::InvalidPath {
            context,
            path: String::new(),
        });
    };
    let mut out = format_identifier(context, root)?;
    for segment in rest {
        out.push('.');
        out.push_str(&format_key_name(segment));
    }
    Ok(out)
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) && !is_hard_keyword(name)
}

fn is_bare_key(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_hard_keyword(name: &str) -> bool {
    matches!(
        name,
        "if" | "else"
            | "for"
            | "in"
            | "await"
            | "cancel"
            | "submit"
            | "print"
            | "call"
            | "and"
            | "or"
            | "not"
            | "true"
            | "false"
            | "null"
    )
}

fn binary_op_source(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Equal => "==",
        BinaryOp::NotEqual => "!=",
        BinaryOp::Less => "<",
        BinaryOp::LessEqual => "<=",
        BinaryOp::Greater => ">",
        BinaryOp::GreaterEqual => ">=",
        BinaryOp::In => "in",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
    }
}

#[cfg(test)]
mod tests;
