//! The benchmarks use the same token bootstrap as production workers.
use lash_kernel_doc::FunctionRegistry;
use std::sync::Arc;

#[path = "../../lash-vm-worker/src/token_bootstrap.rs"]
mod token_bootstrap;

#[expect(
    clippy::expect_used,
    reason = "the shipped token library assembles by construction"
)]
pub(crate) fn registry() -> Arc<FunctionRegistry> {
    let mut registry = token_bootstrap::kernel().expect("kernel token bootstrap");
    let extensions =
        token_bootstrap::register(&mut registry, lash_vm_releases::TYPESCRIPT_EXTENSIONS)
            .expect("extension tokens");
    lash_ext_regex_ecma::implement(
        &mut registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(256)),
    )
    .expect("regex extension");
    lash_ext_date_ecma::implement(&mut registry, extensions.iter().copied()).expect("date natives");
    lash_ext_url_whatwg::implement(&mut registry, extensions.iter().copied()).expect("URL natives");
    token_bootstrap::implemented(&registry, &extensions).expect("extension natives");
    for dialect in lash_vm_releases::DIALECTS {
        let helpers =
            token_bootstrap::register(&mut registry, dialect.helpers).expect("helper tokens");
        if dialect.dialect == lash_vm_releases::TYPESCRIPT {
            lash_ext_ts_core::implement(&mut registry, helpers).expect("TypeScript natives");
        } else {
            lash_ext_py_core::implement(&mut registry, helpers).expect("Python natives");
        }
    }
    Arc::new(registry)
}
