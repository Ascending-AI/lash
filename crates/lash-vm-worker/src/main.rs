#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[expect(
    clippy::disallowed_methods,
    reason = "helper entry reports bootstrap failure through its process exit"
)]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!(
            "{}",
            serde_json::json!({
                "protocol_version": lash_vm_protocol::WORKER_PROTOCOL_VERSION,
                "minimum_supported_protocol_version": lash_vm_protocol::MIN_SUPPORTED_WORKER_PROTOCOL_VERSION,
                "crate_version": env!("CARGO_PKG_VERSION"),
                "arch": std::env::consts::ARCH,
                "os": std::env::consts::OS,
                "debug": cfg!(debug_assertions),
                "testing": cfg!(feature = "testing"),
                "dialects": lash_vm_releases::DIALECTS
                    .iter()
                    .map(|built| built.dialect)
                    .collect::<Vec<_>>(),
            })
        );
        return;
    }
    // The helper executable installs every dialect its build names: the host
    // that ships it chose them as build features (FIG-5990).
    match lash_vm_worker::worker_entry(&lash_vm_worker::built_dialects) {
        Ok(true) => {}
        _ => std::process::exit(1),
    }
}
