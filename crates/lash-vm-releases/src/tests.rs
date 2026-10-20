//! Laws of lash's packages as the build makes them.

// FIG-2971: this file is test/tooling code; the generator's ambient fs/env
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

#[path = "helper_classes.rs"]
mod helper_classes;

/// Records the kernel's closed names from the actual registry for review.
#[test]
#[ignore = "regenerates crates/lash-kernel-lib/src/helpers.rs"]
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn record_kernel_helper_catalog() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root =
        std::path::PathBuf::from(std::env::var("BUILD_WORKSPACE_DIRECTORY").expect("workspace"));
    std::fs::write(
        root.join("crates/lash-kernel-lib/src/helpers.rs"),
        include_str!(concat!(env!("OUT_DIR"), "/kernel_helpers.rs")),
    )
    .unwrap();
}

/// HELP-SET: each dialect's packages, its own and every package it
/// requires, name the closed kernel variants, the dialect's helpers and its
/// extensions' operations, each once.
#[test]
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn helper_sets_cover_every_dialects_packages() {
    use std::collections::BTreeSet;
    let kernel_names: BTreeSet<_> = lash_kernel_lib::KernelHelper::ALL
        .iter()
        .map(|helper| helper.name())
        .collect();
    assert_eq!(kernel_names.len(), lash_kernel_lib::KernelHelper::ALL.len());
    let families: BTreeSet<_> = kernel_names
        .iter()
        .map(|name| name.split_once('.').map_or("", |(family, _)| family))
        .collect();
    assert_eq!(
        families,
        lash_kernel_lib::KERNEL_NAME_FAMILIES
            .iter()
            .copied()
            .collect()
    );
    let names_of = |path: &str| -> Vec<String> {
        let code =
            lash_kernel_doc::ReleasedFunctions::read(package(path).expect("the package").image)
                .expect("the package reads");
        (0..code.len()).map(|at| code.name(at).to_owned()).collect()
    };
    let mut checked = 0;
    for dialect in SHIPPED_DIALECTS {
        if package(dialect).is_none() {
            continue;
        }
        checked += 1;
        let mut expected: BTreeSet<String> = lash_kernel_lib::KernelHelper::ALL
            .iter()
            .map(|helper| helper.name().to_owned())
            .collect();
        #[cfg(feature = "dialect-typescript")]
        if *dialect == TYPESCRIPT {
            for op in lash_ext_regex_ecma::Operation::ALL {
                assert!(expected.insert(op.name().to_owned()));
            }
            for op in lash_ext_date_ecma::Operation::ALL {
                assert!(expected.insert(op.name().to_owned()));
            }
            for op in lash_ext_url_whatwg::Operation::ALL {
                assert!(expected.insert(op.name().to_owned()));
            }
        }
        for name in names_of(dialect) {
            assert!(expected.insert(name));
        }
        let mut names = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = vec![(*dialect).to_owned()];
        while let Some(path) = pending.pop() {
            if !visited.insert(path.clone()) {
                continue;
            }
            let header: lash_kernel_package::PackageHeader =
                serde_json::from_str(package(&path).expect("a declared package").header)
                    .expect("the package header");
            pending.extend(header.requires.keys().map(ToString::to_string));
            for name in names_of(&path) {
                assert!(names.insert(name.clone()), "duplicate name {name}");
            }
        }
        assert_eq!(names, expected, "{dialect}'s packages");
    }
    assert!(checked > 0, "the write-set law needs a built dialect");
}

/// FIG-5821: concurrent cold readers assemble lash's packages once, rather
/// than each allocating the entire frozen catalog.
#[test]
fn concurrent_cold_readers_assemble_the_library_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    let slot = OnceLock::new();
    let assemblies = AtomicUsize::new(0);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (second_tx, second_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let empty = || Arc::new(HeldPackages::install([]).expect("an empty node"));
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
        assert!(Arc::ptr_eq(&first, &second));
    });
}

/// Records the reviewed current helper classification.
#[test]
#[ignore = "regenerates crates/lash-vm-releases/src/helper-classes.tsv"]
#[cfg(all(feature = "dialect-python", feature = "dialect-typescript"))]
fn record_helper_classes() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    let root =
        std::path::PathBuf::from(std::env::var("BUILD_WORKSPACE_DIRECTORY").expect("workspace"));
    std::fs::write(
        root.join("crates/lash-vm-releases/src/helper-classes.tsv"),
        helper_classes::current_census(),
    )
    .unwrap();
}
