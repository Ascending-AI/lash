//! A single executable enters its worker before any host runtime or
//! credentials, configured with natives and dialects only: its registry is
//! derived from the packages its parent holds.
use lash::vm::{
    WorkerEmbedder, WorkerEntry, WorkerPoolConfig, WorkerService, typescript_worker_dialect,
    worker_entry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // This call precedes runtime creation, credential loading and store
    // opening. The host names the dialects its worker installs: here
    // TypeScript, built with `dialect-typescript`, and the natives of its
    // own packages by function identity (none here). There is no embedding
    // a host gets without naming a dialect, and none names a package.
    if worker_entry(&|tuning| WorkerEmbedder::new().dialect(typescript_worker_dialect(tuning)?))? {
        return Ok(());
    }
    // The parent and workers share the immutable build helper image.
    let release = lash_vm_releases::helper_release()?;
    println!("installs {} build helpers", release.helpers.members().len());
    let entry = WorkerEntry::reexec()?;
    let service = WorkerService::new(WorkerPoolConfig::standard(entry));
    let pool = service.pool()?;
    println!("prewarmed {} credential-free worker", pool.stats().workers);
    // Pass this service to the host's RLM and process adapters.
    Ok(())
}
