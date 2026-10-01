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

#[derive(Default)]
pub(crate) struct TypeScriptFrontend {
    pub(crate) parser: std::sync::Mutex<lash_typescript::Parser>,
}

impl Frontend for TypeScriptFrontend {
    fn language_id(&self) -> &'static str {
        "typescript"
    }

    fn parse(
        &self,
        source: &str,
        cell_environment: Option<&LashlangHostEnvironment>,
    ) -> Result<Program, FrontendRefusal> {
        let mut parser = self
            .parser
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let parsed = parser.parse(source, cell_environment);
        parsed.map_err(|error| {
            let error = match cell_environment {
                Some(environment)
                    if error.code == lash_typescript::DiagnosticCode::MethodUnsupported =>
                {
                    match parser.link(source, environment) {
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
