//! What a parent holds of lash's own packages (FIG-5812, FIG-6917).
//!
//! A parent links, admits and migrates documents against exactly the
//! functions its workers hold, and runs none of them (ADR 0123). lash's six
//! packages are made once, by `lash-vm-releases`' build, for the dialects
//! the build was built with (FIG-5990): a parent decodes them as data,
//! without linking a dialect or an extension, and registers them once as a
//! [`HeldPackages`], from which its admission and every worker's registry
//! are derived (V1). A build with no dialect feature holds `kernel` alone.
//!
//! Each dialect's helper releases are still read here until package-set
//! pinning replaces them (FIG-6918).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_package::{HeldPackages, Package, PackageHeader};

use lash_vm_releases::HeldReleases;
pub use lash_vm_releases::HelperReleaseIndex;

/// The current helper release ordinal.
pub const HELPER_RELEASE: u32 = 1;

/// Why lash's packages could not be assembled: a defect of the build,
/// never of a document.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("lash's packages do not assemble: {message}")]
pub struct LibraryError {
    message: String,
}

impl LibraryError {
    fn new(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// lash's packages as a parent holds them, assembled once in a process.
struct Standard {
    /// lash's packages, the node's own set.
    packages: Arc<HeldPackages>,
}

fn standard() -> Result<&'static Standard, LibraryError> {
    static STANDARD: OnceLock<Result<Standard, LibraryError>> = OnceLock::new();
    standard_in(&STANDARD, assemble)
}

fn standard_in(
    slot: &OnceLock<Result<Standard, LibraryError>>,
    assemble: impl FnOnce() -> Result<Standard, LibraryError>,
) -> Result<&Standard, LibraryError> {
    // Assembly decodes and registers every package. Cold readers must
    // wait for that work, rather than each allocating their own copy. A
    // failure is a fixed defect of this build, so retain it as well.
    slot.get_or_init(assemble).as_ref().map_err(Clone::clone)
}

fn assemble() -> Result<Standard, LibraryError> {
    let packages = Arc::new(
        HeldPackages::install(built().map_err(LibraryError::new)?).map_err(LibraryError::new)?,
    );
    Ok(Standard { packages })
}

/// lash's packages, decoded from the build's images in dependency order.
///
/// # Errors
///
/// The decoder's message: a defect of the build.
pub fn built() -> Result<Vec<Package>, String> {
    lash_vm_releases::PACKAGES
        .iter()
        .map(|built| {
            let header: PackageHeader =
                serde_json::from_str(built.header).map_err(|error| error.to_string())?;
            let image = lash_kernel_doc::ReleasedFunctions::read(built.image)
                .map_err(|error| error.to_string())?;
            let package = Package::from_image(header, image).map_err(|error| error.to_string())?;
            let display = lash_kernel_package::shipped_display::of(package.path())
                .map_err(|error| error.to_string())?;
            Ok(package.with_display(display))
        })
        .collect()
}

/// Joins each dialect's releases into one per ordinal: a release is one
/// line, and each dialect has its own functions under it.
fn merged(lines: Vec<HeldReleases>) -> Result<Vec<HelperReleaseIndex>, LibraryError> {
    let mut releases: BTreeMap<u32, HelperReleaseIndex> = BTreeMap::new();
    for line in lines {
        for release in &line.retained {
            let index = release.index();
            match releases.entry(index.ordinal) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let held = entry.get_mut();
                    if held.release != index.release {
                        return Err(LibraryError::new(format!(
                            "helper release {} is `{}` for one dialect and `{}` for another",
                            index.ordinal, held.release, index.release
                        )));
                    }
                    held.writes.extend(index.writes);
                    held.functions.extend(index.functions);
                }
            }
        }
    }
    Ok(releases.into_values().collect())
}

/// lash's own packages as this build holds them, registered once in a
/// process: the set a host that installs no package of its own runs on,
/// and the requirements a host's package is built against.
///
/// # Errors
///
/// [`LibraryError`].
pub fn lash_packages() -> Result<Arc<HeldPackages>, LibraryError> {
    Ok(Arc::clone(&standard()?.packages))
}

/// The current helper release across the build's installed dialects.
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_helper_releases() -> Result<Vec<HelperReleaseIndex>, LibraryError> {
    // A worker already holds its parent's frame. Reading retained-release
    // metadata must not install the parent's packages again in that worker.
    static RELEASES: OnceLock<Result<Vec<HelperReleaseIndex>, LibraryError>> = OnceLock::new();
    RELEASES
        .get_or_init(|| {
            let lines = lash_vm_releases::DIALECTS
                .iter()
                .map(|built| built.held_releases().map_err(LibraryError::new))
                .collect::<Result<Vec<_>, _>>()?;
            merged(lines)
        })
        .as_ref()
        .cloned()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// FIG-5821: concurrent cold readers assemble the shipped library once,
    /// rather than each allocating the entire frozen helper catalog.
    #[test]
    fn concurrent_cold_readers_assemble_the_library_once() {
        let slot = OnceLock::new();
        let assemblies = AtomicUsize::new(0);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let empty = || Standard {
            packages: Arc::new(HeldPackages::install([]).expect("an empty node")),
        };
        std::thread::scope(|threads| {
            let slot = &slot;
            let assemblies = &assemblies;
            let first = threads.spawn(move || {
                standard_in(slot, || {
                    assemblies.fetch_add(1, Ordering::SeqCst);
                    entered_tx.send(()).expect("assembly enters");
                    release_rx.recv().expect("release the first assembly");
                    Ok(empty())
                })
                .expect("the library assembles")
            });
            entered_rx.recv().expect("the first assembly is in flight");
            let second = threads.spawn(move || {
                started_tx.send(()).expect("the second reader starts");
                standard_in(slot, || {
                    assemblies.fetch_add(1, Ordering::SeqCst);
                    second_tx.send(()).expect("a duplicate assembly enters");
                    Ok(empty())
                })
                .expect("the concurrent reader gets the library")
            });
            started_rx
                .recv()
                .expect("the second reader reaches the slot");
            let duplicate = second_rx.recv_timeout(Duration::from_secs(1)).is_ok();
            release_tx.send(()).expect("finish the first assembly");
            let first = first.join().expect("the first reader exits");
            let second = second.join().expect("the second reader exits");
            assert!(!duplicate, "a cold reader started a duplicate assembly");
            assert_eq!(assemblies.load(Ordering::SeqCst), 1);
            assert!(std::ptr::eq(first, second));
        });
    }
}
