use serde::{Deserialize, Serialize, Serializer};
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
        module_ref: linked.artifact.module_ref.clone(),
        host_requirements_ref: linked.artifact.host_requirements_ref.clone(),
        artifact: linked.artifact,
        introspection,
    })
}

/// One compile-stage failure, for hosts and models.
///
/// `span` is the single location representation: `offset` is its start, and
/// `line`/`column` are derived on demand against the source via the methods
/// below, so no two copies of one position can disagree. The wire form keeps
/// the flat `offset` key hosts already read.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ModuleCompileDiagnostic {
    pub message: String,
    #[serde(default)]
    pub span: Option<Span>,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

impl ModuleCompileDiagnostic {
    /// The byte offset into the source the diagnostic's span starts at.
    pub fn offset(&self) -> Option<usize> {
        self.span.map(|span| span.start)
    }

    /// The 1-based line containing `offset()` within `source`.
    pub fn line(&self, source: &str) -> Option<usize> {
        self.offset()
            .map(|offset| source_location(source, offset).0)
    }

    /// The 1-based column of `offset()` within `source`.
    pub fn column(&self, source: &str) -> Option<usize> {
        self.offset()
            .map(|offset| source_location(source, offset).1)
    }
}

/// `ModuleCompileDiagnostic` keeps the flat `offset` key hosts read, derived
/// from `span` at write time. `line`/`column` need the source text the
/// diagnostic does not carry, so they are methods instead of fields; the
/// `stage` field is gone — the enum's `stage` tag already says it.
impl Serialize for ModuleCompileDiagnostic {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut entries = 2;
        entries += usize::from(self.span.is_some());
        entries += usize::from(self.offset().is_some());
        entries += usize::from(self.diagnostic.is_some());
        let mut map = serializer.serialize_map(Some(entries))?;
        map.serialize_entry("message", &self.message)?;
        if let Some(span) = self.span {
            map.serialize_entry("span", &span)?;
        }
        if let Some(offset) = self.offset() {
            map.serialize_entry("offset", &offset)?;
        }
        if let Some(diagnostic) = &self.diagnostic {
            map.serialize_entry("diagnostic", diagnostic)?;
        }
        map.end()
    }
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
    pub fn parse_failure(span: Option<Span>, message: String, rendered: String) -> Self {
        Self::Parse(ModuleCompileDiagnostic {
            message,
            span,
            diagnostic: Some(rendered),
        })
    }

    fn link(source: &str, err: LinkError) -> Self {
        Self::Link(ModuleCompileDiagnostic {
            message: err.to_string(),
            span: err.span(),
            diagnostic: Some(format_link_diagnostic(source, &err)),
        })
    }

    fn introspection(err: ModuleIntrospectionError) -> Self {
        Self::Link(ModuleCompileDiagnostic {
            message: err.to_string(),
            span: None,
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
        let environment =
            LashlangHostEnvironment::new(Default::default(), crate::LashlangAbilities::default());
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
        let source = "if true";
        let err = ModuleCompileError::parse_failure(
            Some(Span { start: 3, end: 7 }),
            "unexpected `true`".to_string(),
            "unexpected `true`\n--> line 1, column 4".to_string(),
        );

        let ModuleCompileError::Parse(diagnostic) = err else {
            panic!("expected parse error");
        };
        assert_eq!(diagnostic.offset(), Some(3));
        assert_eq!(diagnostic.line(source), Some(1));
        assert_eq!(diagnostic.column(source), Some(4));
        assert!(
            diagnostic
                .diagnostic
                .expect("diagnostic")
                .contains("line 1")
        );
    }

    #[test]
    fn compile_error_wire_form_keeps_flat_location_keys() {
        // FIG-3268: `span` is authoritative; `offset` remains on the wire as
        // the derived flat key hosts read, while the shadow `stage` field is
        // gone — the enum tag already carries it.
        let err = ModuleCompileError::parse_failure(
            Some(Span { start: 3, end: 7 }),
            "unexpected `true`".to_string(),
            "unexpected `true`".to_string(),
        );
        let value = serde_json::to_value(&err).expect("serialize");
        assert_eq!(value["stage"], "parse");
        let error = &value["error"];
        assert_eq!(error["offset"], 3);
        assert_eq!(error["span"], serde_json::json!({"start": 3, "end": 7}));
        assert!(error.get("stage").is_none());
        // Round-trip ignores the derived flat keys older payloads may carry.
        let decoded: ModuleCompileError = serde_json::from_value(serde_json::json!({
            "stage": "parse",
            "error": {
                "message": "unexpected `true`",
                "span": {"start": 3, "end": 7},
                "offset": 3,
                "line": 1,
                "column": 4,
                "diagnostic": "unexpected `true`"
            }
        }))
        .expect("deserialize");
        assert_eq!(decoded, err);
    }

    #[test]
    fn compile_module_facade_reports_link_errors() {
        // FIG-2999: declaring a process is no longer an ability the host can
        // withhold, so the withheld ability this fixture links against is
        // `sleep`, which is still one.
        let environment = LashlangHostEnvironment::default();
        let source = "process nap(value: str) { sleep 1 finish value }";
        let program = b::with_declaration_spans(
            b::module(
                vec![b::process(
                    "nap",
                    vec![b::param("value", crate::TypeExpr::Str)],
                    b::block(vec![b::sleep_for(b::num(1.0)), b::finish(b::var("value"))]),
                )],
                Vec::new(),
            ),
            &[(0, source.len())],
        );
        let err = compile_module(ModuleCompileRequest {
            source,
            program,
            environment: &environment,
        })
        .expect_err("link should fail");

        let ModuleCompileError::Link(diagnostic) = err else {
            panic!("expected link error");
        };
        assert_eq!(diagnostic.line(source), Some(1));
        assert!(diagnostic.message.contains("sleep"), "{diagnostic:?}");
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
        let environment =
            LashlangHostEnvironment::new(resources, crate::LashlangAbilities::default())
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
            origin: crate::ProcessOrigin::Declared,
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
