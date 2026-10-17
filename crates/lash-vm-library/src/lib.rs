//! The library functions a host's workers hold, as a parent knows them
//! (FIG-5812).
//!
//! A parent links, admits and migrates documents against exactly the
//! functions its workers hold, and runs none of them (ADR 0123). It reads
//! them from the helper releases the build holds for the dialects it was
//! built with (FIG-5799, FIG-5990, `lash-vm-releases`): each release holds
//! every function a build of it holds, by identity, and the release the
//! tree builds is the one the build defines from current sources. A worker
//! reads its helpers from the same build. So a parent
//! holds the library and each built dialect's helpers without linking a
//! dialect or an extension: the build defines them, and keeps their
//! definitions as data. A build with no dialect feature holds none.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use lash_kernel_doc::FunctionRegistry;

use lash_vm_releases::HeldReleases;
pub use lash_vm_releases::HelperReleaseIndex;

/// The current helper release ordinal.
pub const HELPER_RELEASE: u32 = 1;

/// Why the shipped library could not be assembled: a defect of the build,
/// never of a document.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the shipped library does not assemble: {message}")]
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

/// The shipped library as a parent holds it, assembled once in a process.
struct Standard {
    /// Every retained function and its redeclarations for each successor
    /// version the build interprets.
    interpreted: Arc<FunctionRegistry>,
    /// Each retained release, oldest first: what every dialect the build
    /// holds has of it, together.
    releases: Vec<HelperReleaseIndex>,
}

fn standard() -> Result<&'static Standard, LibraryError> {
    static STANDARD: OnceLock<Result<Standard, LibraryError>> = OnceLock::new();
    standard_in(&STANDARD, assemble)
}

fn standard_in(
    slot: &OnceLock<Result<Standard, LibraryError>>,
    assemble: impl FnOnce() -> Result<Standard, LibraryError>,
) -> Result<&Standard, LibraryError> {
    // Assembly decodes and registers the whole helper catalog. Cold readers
    // must wait for that work, rather than each allocating their own copy.
    // A failure is a fixed defect of this build, so retain it as well.
    slot.get_or_init(assemble).as_ref().map_err(Clone::clone)
}

fn assemble() -> Result<Standard, LibraryError> {
    let lines = lash_vm_releases::DIALECTS
        .iter()
        .map(|built| built.held_releases().map_err(LibraryError::new))
        .collect::<Result<Vec<_>, _>>()?;
    assemble_releases(lines)
}

/// Assembles what a parent holds from `lines`, the helper releases of each
/// dialect the build holds: the union of their functions, and of each
/// release's index across the dialects. A build with no dialect holds
/// nothing.
fn assemble_releases(lines: Vec<HeldReleases>) -> Result<Standard, LibraryError> {
    // Each release lists every function after the functions its body
    // calls, and holds those callees itself.
    let mut written = FunctionRegistry::declarations();
    let mut names = BTreeMap::new();
    let mut releases: BTreeMap<u32, HelperReleaseIndex> = BTreeMap::new();
    for line in &lines {
        let building = line
            .retained
            .last()
            .ok_or_else(|| LibraryError::new("no helper release is retained"))?;
        for release in &line.retained {
            for released in &release.functions {
                if written.get(&released.function).is_some() {
                    continue;
                }
                let function = written
                    .register(released.definition.clone(), None)
                    .map_err(LibraryError::new)?;
                if function != released.function {
                    return Err(LibraryError::new(format!(
                        "helper release {} holds `{}` as {}, and its definition is {function}",
                        release.release, released.definition.name, released.function
                    )));
                }
            }
            merge(&mut releases, release.index())?;
        }
        let mut line_names = BTreeMap::new();
        for function in &building.writes {
            let registered = written.get(function).ok_or_else(|| {
                LibraryError::new(format!(
                    "helper release {} writes {function}, which it does not hold",
                    building.release
                ))
            })?;
            if let Some(other) = line_names.insert(registered.definition().name.clone(), *function)
            {
                return Err(LibraryError::new(format!(
                    "helper release {} writes `{}` as {other} and {function}",
                    building.release,
                    registered.definition().name
                )));
            }
        }
        // Two dialects share the kernel library, by identity; a name two of
        // them resolve otherwise would make a parent's answer depend on
        // which it read first.
        for (name, function) in line_names {
            if let Some(other) = names.insert(name.clone(), function)
                && other != function
            {
                return Err(LibraryError::new(format!(
                    "two dialects write `{name}`, as {other} and {function}"
                )));
            }
        }
    }
    let mut interpreted = written.clone();
    let mut redeclared = false;
    for version in lash_kernel_doc::KernelVersion::ALL {
        if let Some(migration) = lash_kernel_migrate::migration_from(*version) {
            lash_kernel_migrate::migrate_registry(&mut interpreted, migration)
                .map_err(LibraryError::new)?;
            redeclared = true;
        }
    }
    let written = Arc::new(written);
    let interpreted = if redeclared {
        Arc::new(interpreted)
    } else {
        Arc::clone(&written)
    };
    Ok(Standard {
        interpreted,
        releases: releases.into_values().collect(),
    })
}

/// Joins `index`, one dialect's release, to what the other dialects hold of
/// the release of its ordinal: a release is one line, and each dialect has
/// its own functions under it.
fn merge(
    releases: &mut BTreeMap<u32, HelperReleaseIndex>,
    index: HelperReleaseIndex,
) -> Result<(), LibraryError> {
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
    Ok(())
}

/// The function registry a parent links, admits and migrates documents
/// against for workers assembled as lash ships them: every function of
/// the current helper release, each once for every kernel version the build
/// interprets. It holds definitions alone, and is assembled once in a
/// process.
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_functions() -> Result<Arc<FunctionRegistry>, LibraryError> {
    Ok(Arc::clone(&standard()?.interpreted))
}

/// The current helper release across the build's installed dialects.
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_helper_releases() -> Result<Vec<HelperReleaseIndex>, LibraryError> {
    Ok(standard()?.releases.clone())
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
        let empty = || {
            let functions = Arc::new(FunctionRegistry::declarations());
            Standard {
                interpreted: functions,
                releases: Vec::new(),
            }
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
