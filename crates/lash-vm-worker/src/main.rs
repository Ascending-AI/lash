#[expect(
    clippy::disallowed_methods,
    reason = "helper entry reports bootstrap failure through its process exit"
)]
fn main() {
    match lash_vm_worker::worker_entry(lash_vm_worker::build_identity()) {
        Ok(true) => {}
        _ => std::process::exit(1),
    }
}
