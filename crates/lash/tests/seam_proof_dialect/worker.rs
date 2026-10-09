#![expect(
    clippy::expect_used,
    reason = "test worker bootstrap must start before any host initialization"
)]

fn main() {
    lash_vm_worker::worker_entry_with_frontend(&SeamProofFrontend)
        .expect("seam proof worker entry");
}

use lash_vm::LashVmHostEnvironment;
use lash_vm::{AssignTarget, Expr, Program, ResourceRefExpr, Span};
use lash_vm_worker::{Frontend as WorkerFrontend, FrontendRefusal as WorkerFrontendRefusal};

pub struct SeamProofFrontend;

impl WorkerFrontend for SeamProofFrontend {
    fn language_id(&self) -> &'static str {
        "seam-proof"
    }

    fn parse(
        &self,
        source: &str,
        _cell_environment: Option<&LashVmHostEnvironment>,
    ) -> Result<Program, WorkerFrontendRefusal> {
        let mut statements = Vec::new();
        for line in source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            statements.push(statement(line).ok_or_else(|| refusal(source, line))?);
        }
        Ok(Program::block(statements))
    }
}

fn statement(line: &str) -> Option<Expr> {
    if let Some(value) = line.strip_prefix("give ") {
        return Some(Expr::Finish(Box::new(value_expr(value.trim())?)));
    }
    let rest = line.strip_prefix("take ")?;
    let (name, rest) = rest.split_once(" from ")?;
    let (path, arguments) = rest.split_once(" WITH ")?;
    let (module, operation) = path.trim().rsplit_once('.')?;
    let name = name.trim();
    if !is_name(name) || !module.split('.').all(is_name) || !is_name(operation) {
        return None;
    }
    let call = Expr::ReceiverCall {
        receiver: Box::new(Expr::ResourceRef(ResourceRefExpr::unresolved(
            module.split('.').map(Into::into).collect(),
        ))),
        operation: operation.into(),
        args: vec![json_expr(&serde_json::from_str(arguments.trim()).ok()?)],
    };
    Some(Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(Expr::Await(Box::new(Expr::ResultUnwrap(Box::new(call))))),
    })
}

fn value_expr(text: &str) -> Option<Expr> {
    if let Ok(value) = serde_json::from_str(text) {
        return Some(json_expr(&value));
    }
    match text.split_once('.') {
        Some((name, field)) if is_name(name) && is_name(field) => Some(Expr::Field {
            target: Box::new(Expr::Variable(name.into())),
            field: field.into(),
        }),
        None if is_name(text) => Some(Expr::Variable(text.into())),
        _ => None,
    }
}

fn json_expr(value: &serde_json::Value) -> Expr {
    match value {
        serde_json::Value::Null => Expr::Null,
        serde_json::Value::Bool(value) => Expr::Bool(*value),
        serde_json::Value::Number(value) => Expr::Number(value.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(value) => Expr::String(value.as_str().into()),
        serde_json::Value::Array(items) => Expr::List(items.iter().map(json_expr).collect()),
        serde_json::Value::Object(fields) => Expr::Record(
            fields
                .iter()
                .map(|(key, value)| (key.as_str().into(), json_expr(value)))
                .collect(),
        ),
    }
}

fn is_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn refusal(source: &str, line: &str) -> WorkerFrontendRefusal {
    let start = source.find(line).unwrap_or(0);
    let message = format!("not a seam-proof statement: `{line}`");
    WorkerFrontendRefusal {
        error: lash_vm::ModuleCompileError::parse_failure(
            Some(Span {
                start,
                end: start + line.len(),
            }),
            message.clone(),
            message,
        ),
        policy: false,
    }
}
