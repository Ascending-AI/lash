//! The library functions lash's shipped worker holds, as a parent knows
//! them (FIG-5812).
//!
//! A parent links, admits and migrates documents against exactly the
//! functions its workers hold, and runs none of them (ADR 0123). It reads
//! them from the helper releases the build retains (FIG-5799): each release
//! freezes every function a build of it holds, by identity, and the release
//! the tree builds is held equal to what the worker's build defines
//! (`lash-vm-worker`'s `the_tree_builds_the_helper_release_it_froze`). So a
//! parent holds the shipped library, the TypeScript helpers included,
//! without linking a dialect or an extension: the worker's own build
//! defines them, and this crate keeps their definitions as data.

#[path = "generated/helpers_1_0.rs"]
mod helpers_1_0;
mod releases;
#[cfg(feature = "synthetic-next")]
mod synthetic;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use lash_kernel_doc::{FunctionId, FunctionName, FunctionRegistry};

pub use releases::{FREEZE, HelperRelease, HelperReleaseIndex, ReleasedFunction};

/// The helper release the standard embedding writes: the version of the
/// `kernel-helpers` format surface a state its cells and processes write
/// holds (FIG-5799). The synthetic successor changes a helper, so it writes
/// the release after the one this tree builds, and retains that one.
#[cfg(not(feature = "synthetic-next"))]
pub const HELPER_RELEASE: u32 = 1;
/// The synthetic successor's helper release.
#[cfg(feature = "synthetic-next")]
pub const HELPER_RELEASE: u32 = 2;

/// The name and ordinal of each helper release the standard embedding
/// retains, oldest first: the last is the release the tree builds
/// (FIG-5799).
pub const RETAINED_HELPER_RELEASES: &[(&str, u32)] = &[("1.0", 1)];

/// Each retained release as `src/generated/` keeps it, in the order of
/// [`RETAINED_HELPER_RELEASES`].
const RELEASES: &[&str] = &[helpers_1_0::RELEASE];

/// Artifacts removed from the runnable union, kept for the startup retirement
/// survey. Moving an artifact here declares retirement; it never expires by time.
/// The retained declarations above must remove the same release.
const RETIRING_RELEASES: &[&str] = &[];

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

/// The helper releases the build retains, oldest first, as the repository
/// keeps them: the last is the release the tree builds.
///
/// # Errors
///
/// A release that does not decode, or that is not the one
/// [`RETAINED_HELPER_RELEASES`] names in its place.
pub fn helper_releases() -> Result<Vec<HelperRelease>, LibraryError> {
    decode_releases(RELEASES, RETAINED_HELPER_RELEASES)
}

fn decode_releases(
    texts: &[&str],
    declarations: &[(&str, u32)],
) -> Result<Vec<HelperRelease>, LibraryError> {
    if texts.len() != declarations.len() {
        return Err(LibraryError::new(format!(
            "{} helper release declarations have {} artifacts",
            declarations.len(),
            texts.len()
        )));
    }
    texts
        .iter()
        .zip(declarations)
        .map(|(text, (name, ordinal))| {
            let release = HelperRelease::decode(text).map_err(LibraryError::new)?;
            if release.release != *name || release.ordinal != *ordinal {
                return Err(LibraryError::new(format!(
                    "helper release {} ({}) is kept where {name} ({ordinal}) is named",
                    release.release, release.ordinal
                )));
            }
            Ok(release)
        })
        .collect()
}

/// The helpers this build changes over the release it retains, each with
/// the function it replaces, after the functions it calls: in the synthetic
/// successor, one helper and every function that calls it (FIG-5799); none
/// otherwise. `named` are the functions `registry` resolves names to. A
/// worker and its parent make the same change, registering each under its
/// name and keeping the one it replaces under none.
///
/// # Errors
///
/// [`LibraryError`]: a defect of the build.
pub fn changed_helpers(
    named: Vec<FunctionId>,
    registry: &FunctionRegistry,
) -> Result<Vec<lash_kernel_migrate::Redeclared>, LibraryError> {
    #[cfg(feature = "synthetic-next")]
    {
        synthetic::redeclare(named, registry).map_err(LibraryError::new)
    }
    #[cfg(not(feature = "synthetic-next"))]
    {
        let _ = (named, registry);
        Ok(Vec::new())
    }
}

/// The shipped library as a parent holds it, assembled once in a process.
struct Standard {
    /// Every retained function and its redeclarations for each successor
    /// version the build interprets.
    interpreted: Arc<FunctionRegistry>,
    /// What the build's own release resolves each name to.
    names: BTreeMap<FunctionName, FunctionId>,
    releases: Vec<HelperReleaseIndex>,
    retiring: Vec<HelperRelease>,
    surveyed: Arc<FunctionRegistry>,
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
    assemble_releases(helper_releases()?, retiring_releases()?)
}

fn assemble_releases(
    releases: Vec<HelperRelease>,
    retiring: Vec<HelperRelease>,
) -> Result<Standard, LibraryError> {
    let building = releases
        .last()
        .ok_or_else(|| LibraryError::new("no helper release is retained"))?;
    // Each release lists every function after the functions its body
    // calls, and holds those callees itself.
    let mut written = FunctionRegistry::declarations();
    for release in &releases {
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
    }
    let mut names = BTreeMap::new();
    for function in &building.writes {
        let registered = written.get(function).ok_or_else(|| {
            LibraryError::new(format!(
                "helper release {} writes {function}, which it does not hold",
                building.release
            ))
        })?;
        if let Some(other) = names.insert(registered.definition.name.clone(), *function) {
            return Err(LibraryError::new(format!(
                "helper release {} writes `{}` as {other} and {function}",
                building.release, registered.definition.name
            )));
        }
    }
    for function in changed_helpers(names.values().copied().collect(), &written)? {
        let name = function.definition.name.clone();
        written
            .register(function.definition, None)
            .map_err(LibraryError::new)?;
        names.insert(name, function.to);
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
    let surveyed = if retiring.is_empty() {
        Arc::clone(&interpreted)
    } else {
        let mut surveyed = FunctionRegistry::clone(&interpreted);
        let mut retired_ordinals = BTreeSet::new();
        let mut retired_names = BTreeSet::new();
        for release in &retiring {
            if !retired_ordinals.insert(release.ordinal) || !retired_names.insert(&release.release)
            {
                return Err(LibraryError::new(
                    "a retiring helper release is declared twice",
                ));
            }
            if releases
                .iter()
                .any(|kept| kept.ordinal == release.ordinal || kept.release == release.release)
            {
                return Err(LibraryError::new(
                    "a helper release cannot be retained and retiring",
                ));
            }
            if release.ordinal >= building.ordinal {
                return Err(LibraryError::new(
                    "a retiring helper release must precede the building release",
                ));
            }
            let index = release.index();
            if !index.writes.is_subset(&index.functions) {
                return Err(LibraryError::new(
                    "a retiring release writes a function it does not hold",
                ));
            }
            for released in &release.functions {
                if released.definition.identity().map_err(LibraryError::new)? != released.function {
                    return Err(LibraryError::new(
                        "a retiring helper identity differs from its definition",
                    ));
                }
                if surveyed.get(&released.function).is_none() {
                    surveyed
                        .register(released.definition.clone(), None)
                        .map_err(LibraryError::new)?;
                }
            }
        }
        for version in lash_kernel_doc::KernelVersion::ALL {
            if let Some(migration) = lash_kernel_migrate::migration_from(*version) {
                lash_kernel_migrate::migrate_registry(&mut surveyed, migration)
                    .map_err(LibraryError::new)?;
            }
        }
        Arc::new(surveyed)
    };
    Ok(Standard {
        interpreted,
        names,
        releases: releases.iter().map(HelperRelease::index).collect(),
        retiring,
        surveyed,
    })
}

/// The function registry a parent links, admits and migrates documents
/// against for workers assembled as lash ships them: every function of
/// every helper release the build retains, its own and the earlier ones a
/// parked run may still pin, each once for every kernel version the build
/// interprets. It holds definitions alone, and is assembled once in a
/// process.
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_functions() -> Result<Arc<FunctionRegistry>, LibraryError> {
    Ok(Arc::clone(&standard()?.interpreted))
}

/// The helper releases the standard embedding retains, oldest first: the
/// last is the release the tree builds (FIG-5799).
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_helper_releases() -> Result<Vec<HelperReleaseIndex>, LibraryError> {
    Ok(standard()?.releases.clone())
}

fn retiring_releases() -> Result<Vec<HelperRelease>, LibraryError> {
    RETIRING_RELEASES
        .iter()
        .map(|text| HelperRelease::decode(text).map_err(LibraryError::new))
        .collect()
}

/// Releases explicitly removed from the runnable union, whose identity indexes
/// remain available until every dependent has ended or been adopted.
///
/// # Errors
/// [`LibraryError`] when a declaration or artifact is invalid.
pub fn retiring_helper_releases() -> Result<Vec<HelperReleaseIndex>, LibraryError> {
    Ok(standard()?
        .retiring
        .iter()
        .map(HelperRelease::index)
        .collect())
}

/// The declaration catalog used only to survey retirement: includes removed
/// definitions for transitive reachability, without making them runnable.
///
/// # Errors
/// [`LibraryError`].
pub fn helper_survey_functions() -> Result<Arc<FunctionRegistry>, LibraryError> {
    Ok(Arc::clone(&standard()?.surveyed))
}

/// The functions a build of the standard embedding would stop holding were
/// it to stop retaining helper release `release`, each with its
/// counterpart: the function of the same name and kernel version the
/// embedding writes, when it has one (FIG-5799). A function no newer
/// retained release holds and the embedding does not write is listed, and
/// so is each function a kernel version this build interprets redeclares it
/// as. An unknown release is a build error, never an empty survey.
///
/// # Errors
///
/// [`LibraryError`].
pub fn standard_retired_helpers(
    release: u32,
) -> Result<BTreeMap<FunctionId, Option<FunctionId>>, LibraryError> {
    retired_helpers_in(standard()?, release)
}

fn retired_helpers_in(
    standard: &Standard,
    release: u32,
) -> Result<BTreeMap<FunctionId, Option<FunctionId>>, LibraryError> {
    let retired = standard
        .releases
        .iter()
        .cloned()
        .chain(standard.retiring.iter().map(HelperRelease::index))
        .find(|index| index.ordinal == release)
        .ok_or_else(|| {
            LibraryError::new(format!("helper release {release} has no retirement index"))
        })?;
    let mut kept: BTreeSet<FunctionId> = standard.names.values().copied().collect();
    for newer in standard
        .releases
        .iter()
        .filter(|index| index.ordinal > release)
    {
        kept.extend(newer.functions.iter().copied());
    }
    let mut level: BTreeMap<FunctionId, Option<FunctionId>> = retired
        .functions
        .iter()
        .filter(|function| !kept.contains(function))
        .map(|function| {
            let counterpart = standard
                .surveyed
                .get(function)
                .and_then(|registered| standard.names.get(&registered.definition.name).copied());
            (*function, counterpart)
        })
        .collect();
    let mut dropped = level.clone();
    for version in lash_kernel_doc::KernelVersion::ALL {
        let Some(migration) = lash_kernel_migrate::migration_from(*version) else {
            continue;
        };
        let redeclared = |functions: Vec<FunctionId>| {
            lash_kernel_migrate::redeclare(migration.definition, functions, &*standard.surveyed)
                .map(|all| {
                    all.into_iter()
                        .map(|function| (function.from, function.to))
                        .collect::<BTreeMap<_, _>>()
                })
                .map_err(LibraryError::new)
        };
        let old = redeclared(level.keys().copied().collect())?;
        let new = redeclared(level.values().flatten().copied().collect())?;
        level = level
            .iter()
            .filter_map(|(function, counterpart)| {
                let to = *old.get(function)?;
                Some((
                    to,
                    counterpart.and_then(|counterpart| new.get(&counterpart).copied()),
                ))
            })
            .collect();
        dropped.extend(
            level
                .iter()
                .map(|(function, counterpart)| (*function, *counterpart)),
        );
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// V18: every advertised release has a corresponding artifact; zip must not discard it.
    #[test]
    fn a_declared_helper_release_cannot_lack_its_artifact() {
        assert!(decode_releases(&[], &[("1.0", 1)]).is_err());
        assert!(decode_releases(&[helpers_1_0::RELEASE], &[]).is_err());
    }

    /// V09: removal from the runnable union must not erase the retirement survey's identities.
    #[test]
    fn a_removed_helper_release_keeps_its_retirement_index() {
        let release = |result, ordinal| {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
            ))
            .expect("a helper");
            let function = definition.identity().expect("identity");
            HelperRelease {
                release: format!("{ordinal}.0"),
                ordinal,
                sealed: true,
                writes: BTreeSet::from([function]),
                functions: vec![ReleasedFunction {
                    function,
                    definition,
                    fingerprint: None,
                }],
            }
        };
        let removed = release(1, 1);
        let current = release(2, 2);
        let old = removed.functions[0].function;
        let new = current.functions[0].function;
        let standard = assemble_releases(vec![current], vec![removed]).expect("the build");
        assert!(
            standard.interpreted.get(&old).is_none(),
            "retired is not runnable"
        );
        assert!(
            standard.surveyed.get(&old).is_some(),
            "the survey holds removed definitions"
        );
        assert!(
            retired_helpers_in(&standard, 99).is_err(),
            "unknown is not an empty survey"
        );
        assert_eq!(
            retired_helpers_in(&standard, 1)
                .expect("the survey")
                .get(&old),
            Some(&Some(new))
        );
    }

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
                names: BTreeMap::new(),
                releases: Vec::new(),
                retiring: Vec::new(),
                surveyed: Arc::new(FunctionRegistry::declarations()),
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
