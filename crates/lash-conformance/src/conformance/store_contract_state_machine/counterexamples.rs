use super::*;

fn counterexample_path(backend: &str) -> PathBuf {
    let root = std::env::var_os("LASH_STORE_CONTRACT_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LASH_CONFIDENCE_OUT_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("store-contract-counterexamples"))
        })
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("store-contract-counterexamples"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("lash-store-contract-counterexamples"));
    root.join(format!("{backend}.txt"))
}
pub(super) fn persist_counterexample(
    backend: &str,
    runner_seed: u64,
    error: &TestError<GeneratedCase>,
) {
    let path = counterexample_path(backend);
    if let Some(parent) = path.parent()
        && let Err(write_error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "could not create store-contract counterexample directory {}: {write_error}",
            parent.display()
        );
        return;
    }
    let (case_seed, operations) = match error {
        TestError::Fail(_, case) => (Some(case.seed), Some(&case.operations)),
        TestError::Abort(_) => (None, None),
    };
    let body = format!(
        "backend: {backend}\nproptest_runner_seed: {runner_seed}\ncase_seed: {case_seed:?}\nminimal_operations: {operations:#?}\nfailure: {error}\n"
    );
    match std::fs::write(&path, body) {
        Ok(()) => eprintln!(
            "persisted minimized store-contract counterexample to {}",
            path.display()
        ),
        Err(write_error) => eprintln!(
            "could not persist store-contract counterexample to {}: {write_error}",
            path.display()
        ),
    }
}
