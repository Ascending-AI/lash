use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    HostRequirementsRef, LashlangHostEnvironment, LinkError, LinkedModule, ModuleArtifact,
    ModuleIntrospection, ModuleIntrospectionError, ModuleRef, Program, Span,
    format_link_diagnostic,
};

pub struct ModuleCompileRequest<'a> {
    /// The text `program` was authored in, used to render link diagnostics.
    pub source: &'a str,
    /// The lowered module. ADR 0096 leaves the dialect front-end owning the
    /// parse, so the caller supplies the program rather than the text alone.
    pub program: Program,
    pub environment: &'a LashlangHostEnvironment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleCompileOutput {
    pub artifact: ModuleArtifact,
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub introspection: ModuleIntrospection,
}

/// Link and inspect a lowered module without performing I/O.
///
/// `LinkedModule::link` remains public for tooling and low-level tests. Host
/// integrations should prefer this facade so diagnostics, artifact identity,
/// persistence, and introspection are produced consistently. A front-end
/// reports its own refusal through [`ModuleCompileError::parse_failure`], so
/// both stages reach a host as one serialized error shape.
#[allow(
    clippy::result_large_err,
    reason = "boxing ModuleCompileError would change this public serialized error API"
)]
pub fn compile_module(
    request: ModuleCompileRequest<'_>,
) -> Result<ModuleCompileOutput, ModuleCompileError> {
    let linked = LinkedModule::link(request.program, request.environment)
        .map_err(|err| ModuleCompileError::link(request.source, err))?;
    let introspection = linked
        .artifact
        .introspect()
        .map_err(ModuleCompileError::introspection)?;
    Ok(ModuleCompileOutput {
        module_ref: linked.module_ref,
        host_requirements_ref: linked.host_requirements_ref,
        artifact: linked.artifact,
        introspection,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleCompileStage {
    Parse,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleCompileDiagnostic {
    pub stage: ModuleCompileStage,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "stage", content = "error", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModuleCompileError {
    #[error("{0}")]
    Parse(ModuleCompileDiagnostic),
    #[error("{0}")]
    Link(ModuleCompileDiagnostic),
}

impl ModuleCompileError {
    /// Reports a dialect front-end's refusal as the `parse` stage of this
    /// facade's error.
    ///
    /// lashlang has no parser of its own (ADR 0096): whoever produced the
    /// `Program` also owns the refusal when there is no program to produce, and
    /// reports it here so a host reads one shape for both stages. `rendered` is
    /// the front-end's own rendering of the diagnostic, shown in preference to
    /// `message`.
    pub fn parse_failure(
        source: &str,
        offset: Option<usize>,
        message: String,
        rendered: String,
    ) -> Self {
        let (line, column) = match offset {
            Some(offset) => {
                let (line, column) = source_location(source, offset);
                (Some(line), Some(column))
            }
            None => (None, None),
        };
        Self::Parse(ModuleCompileDiagnostic {
            stage: ModuleCompileStage::Parse,
            message,
            offset,
            span: None,
            line,
            column,
            diagnostic: Some(rendered),
        })
    }

    fn link(source: &str, err: LinkError) -> Self {
        let span = err.span();
        let offset = span.map(|span| span.start);
        let (line, column) = offset
            .map(|offset| source_location(source, offset))
            .map(|(line, column)| (Some(line), Some(column)))
            .unwrap_or((None, None));
        Self::Link(ModuleCompileDiagnostic {
            stage: ModuleCompileStage::Link,
            message: err.to_string(),
            offset,
            span,
            line,
            column,
            diagnostic: Some(format_link_diagnostic(source, &err)),
        })
    }

    fn introspection(err: ModuleIntrospectionError) -> Self {
        Self::Link(ModuleCompileDiagnostic {
            stage: ModuleCompileStage::Link,
            message: err.to_string(),
            offset: None,
            span: None,
            line: None,
            column: None,
            diagnostic: Some(err.to_string()),
        })
    }

    pub fn diagnostic(&self) -> &ModuleCompileDiagnostic {
        match self {
            Self::Parse(diagnostic) | Self::Link(diagnostic) => diagnostic,
        }
    }
}

impl std::fmt::Display for ModuleCompileDiagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.diagnostic.as_deref().unwrap_or(self.message.as_str()))
    }
}

fn source_location(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1usize;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    let column = source[line_start..offset].chars().count() + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ast_builders as b;

    /// `process echo(value: str) { finish value }`
    fn echo_module(source: &str) -> Program {
        b::with_declaration_spans(
            b::module(
                vec![b::process(
                    "echo",
                    vec![b::param("value", crate::TypeExpr::Str)],
                    b::block(vec![b::finish(b::var("value"))]),
                )],
                Vec::new(),
            ),
            &[(0, source.len())],
        )
    }

    #[test]
    fn compile_module_facade_returns_artifact_and_introspection() {
        let environment = LashlangHostEnvironment::new(
            Default::default(),
            crate::LashlangAbilities::default()
                .with_processes()
                .with_process_signals(),
        );
        let source = "process echo(value: str) { finish value }";
        let output = compile_module(ModuleCompileRequest {
            source,
            program: echo_module(source),
            environment: &environment,
        })
        .expect("module should compile");

        assert_eq!(output.introspection.exported_processes.len(), 1);
        assert_eq!(
            output.introspection.exported_processes[0]
                .definition
                .process_name,
            "echo"
        );
        assert_eq!(output.module_ref, output.artifact.module_ref);
    }

    #[test]
    fn compile_module_facade_reports_parse_errors() {
        // The front-end owns the parse (ADR 0096) and reports its refusal
        // through the facade, so a host reads one shape for both stages.
        let err = ModuleCompileError::parse_failure(
            "if true",
            Some(3),
            "unexpected `true`".to_string(),
            "unexpected `true`\n--> line 1, column 4".to_string(),
        );

        let ModuleCompileError::Parse(diagnostic) = err else {
            panic!("expected parse error");
        };
        assert_eq!(diagnostic.stage, ModuleCompileStage::Parse);
        assert_eq!(diagnostic.line, Some(1));
        assert_eq!(diagnostic.column, Some(4));
        assert!(
            diagnostic
                .diagnostic
                .expect("diagnostic")
                .contains("line 1")
        );
    }

    #[test]
    fn compile_module_facade_reports_link_errors() {
        let environment = LashlangHostEnvironment::default();
        let source = "process echo(value: str) { finish value }";
        let err = compile_module(ModuleCompileRequest {
            source,
            program: echo_module(source),
            environment: &environment,
        })
        .expect_err("link should fail");

        let ModuleCompileError::Link(diagnostic) = err else {
            panic!("expected link error");
        };
        assert_eq!(diagnostic.stage, ModuleCompileStage::Link);
        assert_eq!(diagnostic.line, Some(1));
        assert!(diagnostic.message.contains("processes"));
    }

    #[test]
    fn compile_module_facade_reports_rich_introspection() {
        let mut resources = crate::LashlangHostCatalog::new();
        resources
            .add_module_operation(
                ["files"],
                "File",
                "read",
                "files.read",
                crate::TypeExpr::Ref("File".into()),
                crate::TypeExpr::Str,
            )
            .expect("host catalog operation must not conflict");
        resources
            .add_value_constructor(
                ["files", "Open"],
                crate::TypeExpr::Object(vec![crate::TypeField {
                    name: "path".into(),
                    ty: crate::TypeExpr::Str,
                    optional: false,
                }]),
                crate::TypeExpr::Ref("File".into()),
            )
            .expect("value constructor is unique");
        resources
            .add_trigger_source_constructor(
                ["ui", "button"],
                crate::TypeExpr::Object(Vec::new()),
                crate::NamedDataType::object(
                    "ui.ButtonPressed",
                    vec![crate::TypeField {
                        name: "color".into(),
                        ty: crate::TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("valid event type"),
            )
            .expect("valid trigger source");
        let environment = LashlangHostEnvironment::new(
            resources,
            crate::LashlangAbilities::default()
                .with_processes()
                .with_process_signals(),
        )
        .with_language_features(
            crate::LashlangLanguageFeatures::default().with_label_annotations(),
        );
        // @label(title: "Watcher", description: "Tracks button presses")
        // process watch(event: ui.ButtonPressed, file: File) signals { done: str } -> str {
        //   opened = files.Open({ path: "inbox.txt" })
        //   text = await files.read(file)?
        //   finish event.color
        // }
        // source = ui.button({})
        // finish source
        let watch = crate::Declaration::Process(crate::ProcessDecl {
            name: "watch".into(),
            params: vec![
                b::param("event", crate::TypeExpr::Ref("ui.ButtonPressed".into())),
                b::param("file", crate::TypeExpr::Ref("File".into())),
            ],
            signals: vec![b::signal("done", crate::TypeExpr::Str)],
            return_ty: Some(crate::TypeExpr::Str),
            label: Some(b::label("Watcher", Some("Tracks button presses"))),
            body: b::block(vec![
                b::assign(
                    "opened",
                    b::receiver_call(
                        b::resource(&["files"]),
                        "Open",
                        vec![b::record(vec![("path", b::string("inbox.txt"))])],
                    ),
                ),
                b::assign(
                    "text",
                    b::unwrap(b::await_expr(b::receiver_call(
                        b::resource(&["files"]),
                        "read",
                        vec![b::var("file")],
                    ))),
                ),
                b::finish(b::field(b::var("event"), "color")),
            ]),
        });
        let program = b::module(
            vec![watch],
            vec![
                b::assign(
                    "source",
                    b::receiver_call(b::resource(&["ui"]), "button", vec![b::record(Vec::new())]),
                ),
                b::finish(b::var("source")),
            ],
        );
        let output = compile_module(ModuleCompileRequest {
            source: "",
            program,
            environment: &environment,
        })
        .expect("module should compile");

        let process = output
            .introspection
            .exported_processes
            .iter()
            .find(|process| process.definition.process_name == "watch")
            .expect("watch process introspection");
        assert_eq!(process.label.as_ref().expect("label").title, "Watcher");
        assert_eq!(process.params.len(), 2);
        assert_eq!(process.signals[0].name, "done");
        assert_eq!(
            process.return_type.as_ref().expect("return type").display,
            "str"
        );
        assert!(
            output
                .introspection
                .required_module_instances
                .iter()
                .any(|module| module.alias == "files"
                    && module
                        .operations
                        .iter()
                        .any(|op| op.host_operation == "files.read"))
        );
        assert!(
            output
                .introspection
                .value_constructors
                .iter()
                .any(|constructor| constructor.key == "files.Open")
        );
        assert!(
            output
                .introspection
                .trigger_source_requirements
                .iter()
                .any(|source| source.source_type == "ui.button"
                    && source.event_type_name == "ui.ButtonPressed")
        );
        assert!(
            output
                .introspection
                .named_data_types
                .iter()
                .any(|ty| ty.name == "ui.ButtonPressed")
        );
    }
}
