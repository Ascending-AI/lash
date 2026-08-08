use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ArtifactStoreError, HostRequirementsRef, LashlangArtifactStore, LashlangHostEnvironment,
    LinkError, LinkedModule, ModuleArtifact, ModuleIntrospection, ModuleIntrospectionError,
    ModuleRef, ParseError, Span, format_link_diagnostic, format_parse_diagnostic, parse,
};

pub struct ModuleCompileRequest<'a> {
    pub source: &'a str,
    pub environment: &'a LashlangHostEnvironment,
    pub artifact_store: Option<&'a dyn LashlangArtifactStore>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleCompileOutput {
    pub artifact: ModuleArtifact,
    pub module_ref: ModuleRef,
    pub host_requirements_ref: HostRequirementsRef,
    pub introspection: ModuleIntrospection,
}

/// Parse, link, inspect, and optionally persist a Lashlang module.
///
/// `parse` and `LinkedModule::link` remain public for tooling and low-level
/// tests. Host integrations should prefer this facade so diagnostics,
/// artifact identity, persistence, and introspection are produced consistently.
pub async fn compile_module(
    request: ModuleCompileRequest<'_>,
) -> Result<ModuleCompileOutput, ModuleCompileError> {
    let program =
        parse(request.source).map_err(|err| ModuleCompileError::parse(request.source, err))?;
    let linked = LinkedModule::link(program, request.environment)
        .map_err(|err| ModuleCompileError::link(request.source, err))?;
    let introspection = linked
        .artifact
        .introspect()
        .map_err(ModuleCompileError::introspection)?;
    if let Some(store) = request.artifact_store {
        store
            .put_module_artifact(&linked.artifact)
            .await
            .map_err(ModuleCompileError::persist)?;
    }
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
    Persist,
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
pub enum ModuleCompileError {
    #[error("{0}")]
    Parse(ModuleCompileDiagnostic),
    #[error("{0}")]
    Link(ModuleCompileDiagnostic),
    #[error("{0}")]
    Persist(ModuleCompileDiagnostic),
}

impl ModuleCompileError {
    fn parse(source: &str, err: ParseError) -> Self {
        let offset = err.offset();
        let (line, column) = source_location(source, offset);
        Self::Parse(ModuleCompileDiagnostic {
            stage: ModuleCompileStage::Parse,
            message: err.to_string(),
            offset: Some(offset),
            span: None,
            line: Some(line),
            column: Some(column),
            diagnostic: Some(format_parse_diagnostic(source, &err)),
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

    fn persist(err: ArtifactStoreError) -> Self {
        Self::Persist(ModuleCompileDiagnostic {
            stage: ModuleCompileStage::Persist,
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
            Self::Parse(diagnostic) | Self::Link(diagnostic) | Self::Persist(diagnostic) => {
                diagnostic
            }
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

    #[tokio::test(flavor = "current_thread")]
    async fn compile_module_facade_returns_artifact_and_introspection() {
        let environment = LashlangHostEnvironment::new(
            Default::default(),
            crate::LashlangAbilities::default()
                .with_processes()
                .with_process_signals(),
        );
        let output = compile_module(ModuleCompileRequest {
            source: "process echo(value: str) { finish value }",
            environment: &environment,
            artifact_store: None,
        })
        .await
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

    #[tokio::test(flavor = "current_thread")]
    async fn compile_module_facade_reports_parse_errors() {
        let environment = LashlangHostEnvironment::default();
        let err = compile_module(ModuleCompileRequest {
            source: "if true",
            environment: &environment,
            artifact_store: None,
        })
        .await
        .expect_err("parse should fail");

        let ModuleCompileError::Parse(diagnostic) = err else {
            panic!("expected parse error");
        };
        assert_eq!(diagnostic.stage, ModuleCompileStage::Parse);
        assert_eq!(diagnostic.line, Some(1));
        assert!(
            diagnostic
                .diagnostic
                .expect("diagnostic")
                .contains("line 1")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compile_module_facade_reports_link_errors() {
        let environment = LashlangHostEnvironment::default();
        let err = compile_module(ModuleCompileRequest {
            source: "process echo(value: str) { finish value }",
            environment: &environment,
            artifact_store: None,
        })
        .await
        .expect_err("link should fail");

        let ModuleCompileError::Link(diagnostic) = err else {
            panic!("expected link error");
        };
        assert_eq!(diagnostic.stage, ModuleCompileStage::Link);
        assert_eq!(diagnostic.line, Some(1));
        assert!(diagnostic.message.contains("processes"));
    }

    struct FailingStore;

    #[async_trait::async_trait]
    impl LashlangArtifactStore for FailingStore {
        async fn put_module_artifact(
            &self,
            _artifact: &ModuleArtifact,
        ) -> Result<(), ArtifactStoreError> {
            Err(ArtifactStoreError::Backend("disk full".to_string()))
        }

        async fn get_module_artifact(
            &self,
            _module_ref: &ModuleRef,
        ) -> Result<Option<std::sync::Arc<ModuleArtifact>>, ArtifactStoreError> {
            Ok(None)
        }

        async fn replace_current_trigger_manifest(
            &self,
            _owner_namespace: &str,
            _artifact: &ModuleArtifact,
        ) -> Result<crate::TriggerManifestReplacement, ArtifactStoreError> {
            Err(ArtifactStoreError::Backend("disk full".to_string()))
        }

        async fn get_current_trigger_manifest(
            &self,
            _owner_namespace: &str,
        ) -> Result<Option<crate::CurrentTriggerKeyManifest>, ArtifactStoreError> {
            Ok(None)
        }

        async fn put_artifact_bytes(
            &self,
            _artifact_ref: &str,
            _descriptor: &str,
            _bytes: &[u8],
        ) -> Result<(), ArtifactStoreError> {
            Ok(())
        }

        async fn get_artifact_bytes(
            &self,
            _artifact_ref: &str,
        ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
            Ok(None)
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compile_module_facade_reports_persistence_errors() {
        let environment = LashlangHostEnvironment::new(
            Default::default(),
            crate::LashlangAbilities::default().with_processes(),
        );
        let store = FailingStore;
        let err = compile_module(ModuleCompileRequest {
            source: "process echo(value: str) { finish value }",
            environment: &environment,
            artifact_store: Some(&store),
        })
        .await
        .expect_err("persist should fail");

        let ModuleCompileError::Persist(diagnostic) = err else {
            panic!("expected persist error");
        };
        assert_eq!(diagnostic.stage, ModuleCompileStage::Persist);
        assert!(diagnostic.message.contains("disk full"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compile_module_facade_reports_rich_introspection() {
        let mut resources = crate::LashlangHostCatalog::new();
        resources.add_module_operation(
            ["files"],
            "File",
            "read",
            "files.read",
            crate::TypeExpr::Ref("File".into()),
            crate::TypeExpr::Str,
        );
        resources.add_value_constructor(
            ["files", "Open"],
            crate::TypeExpr::Object(vec![crate::TypeField {
                name: "path".into(),
                ty: crate::TypeExpr::Str,
                optional: false,
            }]),
            crate::TypeExpr::Ref("File".into()),
        );
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
        let output = compile_module(ModuleCompileRequest {
            source: r#"
@label(title: "Watcher", description: "Tracks button presses")
process watch(event: ui.ButtonPressed, file: File) signals { done: str } -> str {
  opened = files.Open({ path: "inbox.txt" })
  text = await files.read(file)?
  finish event.color
}
source = ui.button({})
finish source
"#,
            environment: &environment,
            artifact_store: None,
        })
        .await
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
        assert!(process.canonical_source.contains("process watch"));
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
