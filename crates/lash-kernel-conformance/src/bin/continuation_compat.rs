//! `continuation-compat`: the cross-process continuation runner
//! (`r57/l7-runner-interface.md`, version 2). See
//! [`lash_kernel_conformance::compat`].

#[expect(
    clippy::disallowed_methods,
    reason = "the runner's entry reads its argv and reports failure by exit status"
)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = lash_kernel_conformance::compat::cli(&args) {
        eprintln!("continuation-compat: {error}");
        std::process::exit(1);
    }
}
