//! The TypeScript spelling table for the shared imperative tree.
use lash_kernel_dialect::{
    Library, Printer, SourceExpr, SourcePlace, SourceStmt, Spelling, imperative_source,
    render_source,
};
use lash_kernel_doc::{Document, Name};

use crate::TypeScript;

/// Prints equivalent TypeScript, retaining every temporary and statement boundary.
pub fn print(document: &Document) -> Result<String, lash_kernel_dialect::Diagnostic> {
    let source = imperative_source(document).map_err(|error| lash_kernel_dialect::Diagnostic {
        code: "TS_KERNEL_SOURCE".into(),
        message: error.to_string(),
        span: None,
        kind: lash_kernel_dialect::DiagnosticKind::ProgramDefect,
        repairs: vec![],
    })?;
    Ok(render_source(&source, &TypeScript))
}

impl Printer for TypeScript {
    fn print(
        &self,
        document: &Document,
        _library: &dyn Library,
    ) -> Result<String, lash_kernel_dialect::Diagnostic> {
        print(document)
    }
}

/// An injective spelling of arbitrary kernel names, including keywords and `k`.
pub(crate) fn identifier(name: &Name) -> String {
    use std::fmt::Write;
    let mut out = String::from("v_");
    for byte in name.as_str().as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
fn quote(text: &str) -> String {
    serde_json::Value::String(text.into())
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}
fn braces(body: &str) -> String {
    let indented = body
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect::<String>();
    format!("{{\n{indented}}}")
}
impl Spelling for TypeScript {
    fn expression(&self, expression: &SourceExpr, operands: &[String]) -> String {
        match expression {
            SourceExpr::Null => "null".into(),
            SourceExpr::Bool(value) => value.to_string(),
            SourceExpr::Text(value) => quote(value),
            SourceExpr::Variable(name) => identifier(name),
            SourceExpr::Array(_) => format!("[{}]", operands.join(", ")),
            SourceExpr::Intrinsic("absent", _) => "k.absent".into(),
            SourceExpr::Intrinsic(form, _) => format!("k.{form}({})", operands.join(", ")),
            SourceExpr::Function { params, .. } => format!(
                "function({}) {}",
                params.iter().map(identifier).collect::<Vec<_>>().join(", "),
                braces(&operands[0])
            ),
        }
    }
    fn statement(&self, statement: &SourceStmt, operands: &[String]) -> String {
        match statement {
            SourceStmt::Let(name, _) => format!("let {} = {};", identifier(name), operands[0]),
            SourceStmt::Assign(place, _) => {
                let place = match place {
                    SourcePlace::Variable(_) => operands[0].clone(),
                    SourcePlace::Member(_) => format!("{}.value", operands[0]),
                };
                format!("{place} = {};", operands[1])
            }
            SourceStmt::Expression(_) => format!("{};", operands[0]),
            SourceStmt::If(_, _, no) => format!(
                "if ({}) {}{}",
                operands[0],
                braces(&operands[1]),
                if no.is_empty() {
                    String::new()
                } else {
                    format!(" else {}", braces(&operands[2]))
                }
            ),
            SourceStmt::For(name, _, _) => format!(
                "for (let {} of {}) {}",
                identifier(name),
                operands[0],
                braces(&operands[1])
            ),
            SourceStmt::While(_, _) => format!("while ({}) {}", operands[0], braces(&operands[1])),
            SourceStmt::Break => "break;".into(),
            SourceStmt::Continue => "continue;".into(),
            SourceStmt::Return(_) => format!("return {};", operands[0]),
            SourceStmt::Throw(_) => format!("throw {};", operands[0]),
            SourceStmt::Try { catch, finally, .. } => format!(
                "try {}{}{}",
                braces(&operands[0]),
                catch
                    .as_ref()
                    .map(|(name, _)| format!(
                        " catch ({}) {}",
                        identifier(name),
                        braces(&operands[1])
                    ))
                    .unwrap_or_default(),
                finally
                    .as_ref()
                    .map(|_| format!(" finally {}", braces(&operands[2])))
                    .unwrap_or_default()
            ),
        }
    }
}
