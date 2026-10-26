//! The benchmarks use the same package install as production workers.
use lash_kernel_doc::FunctionRegistry;
use std::sync::Arc;

#[expect(
    clippy::expect_used,
    reason = "lash's packages install by construction"
)]
pub(crate) fn registry() -> Arc<FunctionRegistry> {
    let embedding =
        lash_vm_worker::built_embedding(&Default::default()).expect("lash's packages install");
    Arc::clone(embedding.registry().expect("the installed registry"))
}
