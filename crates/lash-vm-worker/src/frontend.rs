use lashlang::{LashlangHostEnvironment, ModuleCompileError, Program};

/// A source frontend compiled into a worker entry. The parent sends source
/// text and host vocabulary; lowering and diagnostics run in the child.
pub trait Frontend: Send + Sync {
    fn language_id(&self) -> &'static str;

    fn parse(
        &self,
        source: &str,
        cell_environment: Option<&LashlangHostEnvironment>,
    ) -> Result<Program, FrontendRefusal>;
}

pub struct FrontendRefusal {
    pub error: ModuleCompileError,
    pub policy: bool,
}

pub(crate) struct TypeScriptFrontend;

impl Frontend for TypeScriptFrontend {
    fn language_id(&self) -> &'static str {
        "typescript"
    }

    fn parse(
        &self,
        source: &str,
        cell_environment: Option<&LashlangHostEnvironment>,
    ) -> Result<Program, FrontendRefusal> {
        let parsed = match cell_environment {
            Some(environment) => lash_typescript::parse_cell(source, environment),
            None => lash_typescript::parse(source),
        };
        parsed.map_err(|error| {
            let error = match cell_environment {
                Some(environment)
                    if error.code == lash_typescript::DiagnosticCode::MethodUnsupported =>
                {
                    match lash_typescript::link(source, environment) {
                        Err(contextual) if contextual.code == error.code => contextual,
                        _ => error,
                    }
                }
                _ => error,
            };
            let policy = error.is_dialect_refusal();
            let rendered = lash_typescript::format_diagnostic(source, &error);
            FrontendRefusal {
                error: ModuleCompileError::parse_failure(
                    error.span.map(|span| lashlang::Span {
                        start: span.start,
                        end: span.end,
                    }),
                    error.message,
                    rendered,
                ),
                policy,
            }
        })
    }
}
