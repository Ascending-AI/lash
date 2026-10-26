#[cfg(all(feature = "cell-memory", feature = "dhat-heap"))]
type BaseAllocator = lash_vm_worker::cell_memory::Alloc<dhat::Alloc>;
#[cfg(all(feature = "cell-memory", feature = "dhat-heap"))]
const BASE_ALLOCATOR: BaseAllocator = lash_vm_worker::cell_memory::Alloc(dhat::Alloc);

#[cfg(all(feature = "cell-memory", not(feature = "dhat-heap")))]
type BaseAllocator = lash_vm_worker::cell_memory::Alloc<std::alloc::System>;
#[cfg(all(feature = "cell-memory", not(feature = "dhat-heap")))]
const BASE_ALLOCATOR: BaseAllocator = lash_vm_worker::cell_memory::Alloc(std::alloc::System);

#[cfg(all(feature = "dhat-heap", not(feature = "cell-memory")))]
type BaseAllocator = dhat::Alloc;
#[cfg(all(feature = "dhat-heap", not(feature = "cell-memory")))]
const BASE_ALLOCATOR: BaseAllocator = dhat::Alloc;

#[cfg(not(any(feature = "dhat-heap", feature = "cell-memory")))]
type BaseAllocator = std::alloc::System;
#[cfg(not(any(feature = "dhat-heap", feature = "cell-memory")))]
const BASE_ALLOCATOR: BaseAllocator = std::alloc::System;

#[cfg(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "testing",
    feature = "dialect-typescript",
    feature = "dialect-python"
))]
#[global_allocator]
static ALLOCATOR: lash_vm_worker::core_support::cell_profile::CountingAllocator<BaseAllocator> =
    lash_vm_worker::core_support::cell_profile::CountingAllocator(BASE_ALLOCATOR);

#[cfg(not(all(
    target_os = "linux",
    target_arch = "x86_64",
    feature = "testing",
    feature = "dialect-typescript",
    feature = "dialect-python"
)))]
#[global_allocator]
static ALLOCATOR: BaseAllocator = BASE_ALLOCATOR;

#[expect(
    clippy::disallowed_methods,
    reason = "helper entry reports bootstrap failure through its process exit"
)]
fn main() {
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "testing",
        feature = "dialect-typescript",
        feature = "dialect-python"
    ))]
    if std::env::args().nth(1).as_deref() == Some("--profile-cell") {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        if args.len() != 2 {
            eprintln!("usage: --profile-cell FILE OUT");
            std::process::exit(2);
        }
        match lash_vm_worker::core_support::cell_profile::run(
            std::path::Path::new(&args[0]),
            std::path::Path::new(&args[1]),
        ) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("cell profile: {error}");
                std::process::exit(1);
            }
        }
    }

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
                "dialects": lash_kernel_releases::SHIPPED_DIALECTS
                    .iter()
                    .filter(|dialect| lash_kernel_releases::package(dialect).is_some())
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
