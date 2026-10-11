//! A single executable enters its worker before any host runtime or credentials.
use lash::vm::{
    WorkerEmbedder, WorkerEntry, WorkerPoolConfig, WorkerService, typescript_worker_dialect,
    worker_entry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This call precedes runtime creation, credential loading and store
    // opening. The host names the dialects its worker installs: here the
    // kernel library and TypeScript, built with `dialect-typescript`. There
    // is no embedding a host gets without naming one.
    if worker_entry(&|tuning| {
        WorkerEmbedder::kernel()?
            .dialect(typescript_worker_dialect(tuning))?
            .finish()
    })? {
        return Ok(());
    }
    let entry = WorkerEntry::reexec()?;
    let service = WorkerService::new(WorkerPoolConfig::standard(entry));
    let pool = service.pool()?;
    println!("prewarmed {} credential-free worker", pool.stats().workers);
    // Pass this service to the host's RLM and process adapters.
    Ok(())
}
