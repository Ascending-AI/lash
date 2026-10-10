#![expect(
    clippy::expect_used,
    reason = "test worker bootstrap must start before any host initialization"
)]

//! The worker of the seam proof: the kernel library and one dialect
//! package, whose front end reads the two statement forms and writes a
//! kernel document.

use lash::dialect::{Diagnostic, DiagnosticKind, Environment, FrontEnd, Lowered, Package, Span};
use lash::workflow::document::{Annotations, EffectName, Name, NumberPolicy, parse_document};

const LANGUAGE_ID: &str = "seam-proof";

/// The names the front end binds for itself; no cell reads them.
const INPUT: &str = "seam_input";
const GIVEN: &str = "seam_given";

fn main() {
    lash::vm::worker_entry_with(&|_tuning| {
        let mut embedder = lash::vm::WorkerEmbedder::kernel()?;
        embedder.install(Package {
            dialect: LANGUAGE_ID.to_owned(),
            front_end: Box::new(SeamProofFrontEnd),
            printer: None,
            functions: Vec::new(),
        })?;
        embedder.finish()
    })
    .expect("seam proof worker entry");
}

struct SeamProofFrontEnd;

impl FrontEnd for SeamProofFrontEnd {
    fn lower(&self, source: &str, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
        let mut body = String::new();
        let mut performed = Vec::new();
        let mut private = Vec::new();
        for (index, line) in source
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            let statement = statement(line, index).ok_or_else(|| refusal(source, line))?;
            body.push_str(&statement.text);
            if statement.effect.as_ref().is_some_and(|path| {
                EffectName::new(path.as_str()).ok().is_some_and(|effect| {
                    environment
                        .controls
                        .get(&effect)
                        .is_some_and(|controls| !controls.is_empty())
                })
            }) {
                body.push_str("  finish null\n");
            }
            performed.extend(statement.effect);
            private.extend(statement.private);
        }
        let text = format!("kernel 1\nnumbers float\n\nmain {{\n{body}}}\n");
        let mut document = parse_document(&text).map_err(|error| defect(error.to_string()))?;
        document.manifest.numbers = NumberPolicy::Float;
        for path in performed {
            let effect = EffectName::new(path.as_str())
                .map_err(|_| defect(format!("`{path}` names no effect")))?;
            let signature = environment
                .effects
                .get(&effect)
                .ok_or_else(|| defect(format!("no tool answers `{path}`")))?;
            document.manifest.effects.insert(effect, signature.clone());
        }
        document
            .private_bindings
            .extend(private.into_iter().map(Name::new));
        let identity = document
            .identity()
            .map_err(|error| defect(error.to_string()))?;
        Ok(Lowered {
            document,
            annotations: Annotations {
                document: identity,
                dialect: Some(LANGUAGE_ID.to_owned()),
                source: Some(source.to_owned()),
                nodes: Vec::new(),
            },
        })
    }
}

/// One source line as kernel text, the tool it calls and the name the
/// front end bound for it.
struct Statement {
    text: String,
    effect: Option<String>,
    private: Vec<String>,
}

fn statement(line: &str, index: usize) -> Option<Statement> {
    if let Some(value) = line.strip_prefix("give ") {
        let private = format!("{GIVEN}_{index}");
        let control_result = format!("seam_control_{index}");
        return Some(Statement {
            text: format!(
                "  let {private} = {}\n  let {control_result} = perform control.finish({private}) as Any\n",
                value_text(value.trim())?
            ),
            effect: Some("control.finish".to_owned()),
            private: vec![private, control_result],
        });
    }
    let rest = line.strip_prefix("take ")?;
    let (name, rest) = rest.split_once(" from ")?;
    let (path, arguments) = rest.split_once(" WITH ")?;
    let (name, path) = (name.trim(), path.trim());
    if !is_name(name) || !path.split('.').all(is_name) || !path.contains('.') {
        return None;
    }
    let input = json_text(&serde_json::from_str(arguments.trim()).ok()?)?;
    let private = format!("{INPUT}_{index}");
    Some(Statement {
        text: format!(
            "  let {private} = {input}\n  let {name} = perform {path}({private}) as Any\n"
        ),
        effect: Some(path.to_owned()),
        private: vec![private],
    })
}

fn value_text(text: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str(text) {
        return json_text(&value);
    }
    match text.split_once('.') {
        Some((name, field)) if is_name(name) && is_name(field) => Some(format!("{name}.{field}")),
        None if is_name(text) => Some(text.to_owned()),
        _ => None,
    }
}

/// A JSON value as a kernel literal.
fn json_text(value: &serde_json::Value) -> Option<String> {
    Some(match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => format!("{:?}", value.as_f64()?),
        serde_json::Value::String(_) => value.to_string(),
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(json_text)
                .collect::<Option<Vec<_>>>()?
                .join(", ")
        ),
        serde_json::Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| {
                    is_name(key)
                        .then(|| json_text(value))
                        .flatten()
                        .map(|value| format!("{key}: {value}"))
                })
                .collect::<Option<Vec<_>>>()?
                .join(", ")
        ),
    })
}

fn is_name(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn refusal(source: &str, line: &str) -> Diagnostic {
    let start = source.find(line).unwrap_or(0);
    Diagnostic {
        code: "SEAM_NOT_A_STATEMENT".to_owned(),
        message: format!("not a seam-proof statement: `{line}`"),
        span: Some(Span {
            start,
            end: start + line.len(),
        }),
        kind: DiagnosticKind::ProgramDefect,
        repairs: Vec::new(),
    }
}

fn defect(message: String) -> Diagnostic {
    Diagnostic {
        code: "SEAM_NOT_A_PROGRAM".to_owned(),
        message,
        span: None,
        kind: DiagnosticKind::ProgramDefect,
        repairs: Vec::new(),
    }
}
