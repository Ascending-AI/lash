//! A single executable enters its worker before any host runtime or credentials.
use lash::rlm::lang::{LashlangHostEnvironment, ModuleCompileError, Program};
use lash::rlm::{
    WorkerEntry, WorkerFrontend, WorkerFrontendRefusal, WorkerPoolConfig, WorkerService,
    worker_build_identity, worker_entry_with_frontend,
};

struct TypeScript;
impl WorkerFrontend for TypeScript {
    fn language_id(&self) -> &'static str {
        "typescript"
    }
    fn parse(
        &self,
        source: &str,
        environment: Option<&LashlangHostEnvironment>,
    ) -> Result<Program, WorkerFrontendRefusal> {
        let parsed = match environment {
            Some(environment) => lash::typescript::parse_cell(source, environment),
            None => lash::typescript::parse(source),
        };
        parsed.map_err(|error| WorkerFrontendRefusal {
            policy: error.is_dialect_refusal(),
            error: ModuleCompileError::parse_failure(
                error.span.map(|span| lash::rlm::lang::Span {
                    start: span.start,
                    end: span.end,
                }),
                error.message.clone(),
                lash::typescript::format_diagnostic(source, &error),
            ),
        })
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This call precedes runtime creation, credential loading and store opening.
    let identity = worker_build_identity();
    if worker_entry_with_frontend(identity.clone(), &TypeScript)? {
        return Ok(());
    }
    let entry = WorkerEntry::reexec(identity)?;
    let service = WorkerService::new(WorkerPoolConfig::standard(entry));
    let pool = service.pool()?;
    println!("prewarmed {} credential-free worker", pool.stats().workers);
    // Pass this service to the host's RLM and process adapters.
    Ok(())
}
