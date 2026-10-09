//! A single executable enters its worker before any host runtime or credentials.
use lash::vm::{WorkerEntry, WorkerPoolConfig, WorkerService, worker_entry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This call precedes runtime creation, credential loading and store
    // opening. The worker registers the kernel library, lash's extensions
    // and the TypeScript dialect; a host with functions or dialects of its
    // own calls `worker_entry_with` instead.
    if worker_entry()? {
        return Ok(());
    }
    let entry = WorkerEntry::reexec()?;
    let service = WorkerService::new(WorkerPoolConfig::standard(entry));
    let pool = service.pool()?;
    println!("prewarmed {} credential-free worker", pool.stats().workers);
    // Pass this service to the host's RLM and process adapters.
    Ok(())
}
