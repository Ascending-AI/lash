//! Paired optimized measurements of the production worker pool and protocol.
#[path = "../vm_worker_matrix/mod.rs"]
mod matrix;

fn main() -> anyhow::Result<()> {
    if lash_vm_worker::worker_entry()? {
        return Ok(());
    }
    if cfg!(debug_assertions) {
        anyhow::bail!("build this benchmark with kiln build --config=optimized");
    }
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--verify") {
        return matrix::verify();
    }
    if let Some(warm) = args.windows(2).find(|p| p[0] == "--exchanges") {
        return matrix::exchanges(warm[1].parse()?);
    }
    let directory = args
        .windows(2)
        .find(|p| p[0] == "--out")
        .map(|p| std::path::PathBuf::from(&p[1]))
        .ok_or_else(|| anyhow::anyhow!("pass --out DIRECTORY, --exchanges SAMPLES or --verify"))?;
    matrix::measure(&directory, 10_000, 200)
}
