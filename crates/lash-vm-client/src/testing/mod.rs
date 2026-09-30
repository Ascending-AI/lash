use std::path::PathBuf;

pub(crate) fn worker_executable(packaged: PathBuf) -> PathBuf {
    #[expect(
        clippy::disallowed_methods,
        reason = "test runner selects the worker runfile"
    )]
    std::env::var_os("LASH_VM_WORKER")
        .map(PathBuf::from)
        .unwrap_or(packaged)
}
