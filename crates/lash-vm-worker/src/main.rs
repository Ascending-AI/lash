#[expect(
    clippy::disallowed_methods,
    reason = "helper entry reports bootstrap failure through its process exit"
)]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--build-identity") {
        println!("{}", lash_vm_worker::build_identity());
        return;
    }
    match lash_vm_worker::worker_entry(lash_vm_worker::build_identity()) {
        Ok(true) => {}
        _ => std::process::exit(1),
    }
}
