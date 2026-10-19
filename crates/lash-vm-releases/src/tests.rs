//! Laws of the current helper release and its checkpoint admission.

// FIG-2971: this file is test/tooling code; the generator's ambient fs/env
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

#[path = "helper_classes.rs"]
mod helper_classes;

fn helper(result: i64) -> lash_kernel_doc::FunctionDefinition {
    lash_kernel_doc::parse_definition(&format!(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
    ))
    .expect("a helper definition")
}

/// The releases this build holds for each dialect it is built with are the
/// ones it declares, and the release the tree builds writes exactly what the
/// build defines (FIG-5990: one release line a dialect).
#[test]
fn the_build_holds_the_releases_it_declares_for_each_dialect() {
    let named = |declarations: &[(&str, u32)]| -> Vec<(String, u32)> {
        declarations
            .iter()
            .map(|(name, ordinal)| ((*name).to_owned(), *ordinal))
            .collect()
    };
    let declared = |releases: &[HelperRelease]| -> Vec<(String, u32)> {
        releases
            .iter()
            .map(|release| (release.release.clone(), release.ordinal))
            .collect()
    };
    for built in DIALECTS {
        assert!(SHIPPED_DIALECTS.contains(&built.dialect));
        assert_eq!(
            dialect(built.dialect).map(|found| found.dialect),
            Some(built.dialect)
        );
        let held = built.held_releases().expect("the build's releases");
        assert_eq!(declared(&held.retained), named(&[("1.0", 1)]));
        let index: Vec<HelperReleaseIndex> =
            serde_json::from_slice(built.helper_release_index).expect("the index decodes");
        assert_eq!(
            index,
            held.retained
                .iter()
                .map(HelperRelease::index)
                .collect::<Vec<_>>()
        );
        let helpers =
            lash_kernel_doc::ReleasedFunctions::read(built.helpers).expect("the helpers read");
        let building = held.retained.last().expect("the release the tree builds");
        for function in helpers.ids() {
            assert!(
                building.writes.contains(&function),
                "{}'s release the tree builds writes the build's helper {function}",
                built.dialect
            );
        }
    }
}

/// FIG-5990: a dialect's release holds its own helpers and none of another
/// dialect's, so a host built with one dialect carries only that one's.
#[test]
fn a_dialects_release_holds_none_of_another_dialects_helpers() {
    let helpers = |built: &DialectReleases| -> std::collections::BTreeSet<_> {
        lash_kernel_doc::ReleasedFunctions::read(built.helpers)
            .expect("the helpers read")
            .ids()
            .collect()
    };
    for built in DIALECTS {
        let held = built.held_releases().expect("the build's releases");
        for other in DIALECTS
            .iter()
            .filter(|other| other.dialect != built.dialect)
        {
            let theirs = helpers(other);
            for release in &held.retained {
                assert!(
                    release
                        .functions
                        .iter()
                        .all(|held| !theirs.contains(&held.function)),
                    "{}'s release {} holds a helper of {}",
                    built.dialect,
                    release.release,
                    other.dialect
                );
            }
        }
    }
}

/// Stages nodes in memory: each distinct body is one reference.
#[derive(Default)]
struct Staged(std::collections::BTreeMap<(u32, Vec<u8>), lash_kernel_doc::graph_tokens::ChildRef>);

impl Staged {
    fn stage(
        &mut self,
        kind: u32,
        body: &[u8],
    ) -> Result<lash_kernel_doc::graph_tokens::ChildRef, std::convert::Infallible> {
        let next = self.0.len();
        Ok(*self.0.entry((kind, body.to_vec())).or_insert_with(|| {
            let mut digest = [0_u8; 32];
            digest[..8].copy_from_slice(&(next as u64).to_le_bytes());
            lash_kernel_doc::graph_tokens::ChildRef(digest)
        }))
    }
}

fn released(definition: lash_kernel_doc::FunctionDefinition) -> ReleasedFunction {
    ReleasedFunction {
        function: definition.identity(),
        definition,
    }
}

fn one_release(result: i64) -> HelperRelease {
    let native = lash_kernel_doc::parse_definition(
        "function num.neg(x: Int) -> Int\nkernel 1\ncharge 1\nnative\n",
    )
    .expect("a native definition");
    let functions = vec![released(native), released(helper(result))];
    HelperRelease {
        release: "1.0".into(),
        ordinal: 1,
        writes: functions.iter().map(|held| held.function).collect(),
        functions,
    }
}

/// FIG-5887 §9: a run resumes only on the exact manifest it pinned. A
/// release rebuilt under the same name and ordinal with one helper changed
/// is another manifest, and finding the name again does not repair it.
#[test]
fn a_run_resumes_only_on_the_exact_release_manifest_it_pinned() {
    let mut staged = Staged::default();
    let (_, pinned) =
        stage_release(&one_release(1), &mut |kind, body| staged.stage(kind, body)).unwrap();
    let held = HeldReleases {
        retained: vec![one_release(2)],
    };
    let mut manifests = HeldManifests::default();
    manifests
        .stage(TYPESCRIPT, &held, |kind, body| staged.stage(kind, body))
        .unwrap();
    let current = manifests
        .current(TYPESCRIPT)
        .expect("the release the tree builds");
    assert_eq!(manifests.dialect_of(&current), Some(TYPESCRIPT));
    assert_eq!(
        manifests.current(PYTHON),
        None,
        "a dialect the build does not hold has no release to pin"
    );
    assert_ne!(pinned, current);
    let required = std::collections::BTreeSet::new();
    assert_eq!(
        manifests.admit(&pinned, &required, |_| Some(true)),
        Err(ReleaseRefusal::ReleaseMismatch { pinned })
    );
    let manifest = manifests
        .admit(&current, &required, |_| Some(true))
        .expect("the exact manifest admits");
    assert_eq!((manifest.release.as_str(), manifest.ordinal), ("1.0", 1));
    // The manifest reads back from its one spelling.
    let body = lash_kernel_doc::graph_tokens::encode_body(manifest);
    assert_eq!(
        &lash_kernel_doc::graph_tokens::decode_body::<ReleaseManifest>(&body).unwrap(),
        manifest
    );
}

/// FIG-5887 §9: holding a definition is not holding the function. A worker
/// without the native implementation the manifest records, or without a
/// function the run calls, refuses resume.
#[test]
fn a_missing_native_or_helper_refuses_resume() {
    let release = one_release(1);
    let native = release.functions[0].function;
    let body = release.functions[1].function;
    let mut staged = Staged::default();
    let mut manifests = HeldManifests::default();
    manifests
        .stage(
            TYPESCRIPT,
            &HeldReleases {
                retained: vec![release],
            },
            |kind, body| staged.stage(kind, body),
        )
        .unwrap();
    let pinned = manifests.current(TYPESCRIPT).unwrap();
    let required: std::collections::BTreeSet<_> = [native, body].into();
    assert_eq!(
        manifests.admit(&pinned, &required, |function| Some(*function != native)),
        Err(ReleaseRefusal::MissingNative { function: native })
    );
    assert_eq!(
        manifests.admit(&pinned, &required, |function| {
            (*function != body).then_some(true)
        }),
        Err(ReleaseRefusal::MissingHelper { function: body })
    );
    let stranger = helper(9).identity();
    assert_eq!(
        manifests.admit(&pinned, &[stranger].into(), |_| Some(true)),
        Err(ReleaseRefusal::MissingHelper { function: stranger })
    );
    // A body-only helper needs no native.
    assert!(
        manifests
            .admit(&pinned, &required, |function| Some(*function == native))
            .is_ok()
    );
}

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

/// HELP-SET / HELP-LABEL: release writes have the closed variants and one label.
#[test]
#[cfg(any(feature = "dialect-typescript", feature = "dialect-python"))]
fn helper_sets_and_labels_cover_every_release_write_set() {
    use std::collections::{BTreeMap, BTreeSet};
    let labels = lash_helper_labels::labels().unwrap();
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
    let mut installed = BTreeMap::new();
    for dialect in DIALECTS {
        let held = dialect.held_releases().unwrap();
        let mut expected: BTreeSet<_> = lash_kernel_lib::KernelHelper::ALL
            .iter()
            .map(|h| h.name())
            .collect();
        match dialect.dialect {
            #[cfg(feature = "dialect-typescript")]
            TYPESCRIPT => {
                for helper in TypeScriptHelper::ALL {
                    assert!(expected.insert(helper.name()));
                }
                for op in lash_ext_regex_ecma::Operation::ALL {
                    assert!(expected.insert(op.name()));
                }
                for op in lash_ext_date_ecma::Operation::ALL {
                    assert!(expected.insert(op.name()));
                }
                for op in lash_ext_url_whatwg::Operation::ALL {
                    assert!(expected.insert(op.name()));
                }
            }
            #[cfg(feature = "dialect-python")]
            PYTHON => {
                for helper in PythonHelper::ALL {
                    assert!(expected.insert(helper.name()));
                }
            }
            other => panic!("undeclared dialect {other}"),
        }
        for release in &held.retained {
            let mut names = BTreeSet::new();
            for id in &release.writes {
                let f = release
                    .functions
                    .iter()
                    .find(|f| &f.function == id)
                    .unwrap();
                let name = &f.definition.name;
                assert!(names.insert(name.as_str()), "duplicate write name {name}");
                assert!(
                    !labels
                        .get(name)
                        .unwrap_or_else(|| panic!("unlabelled {name}"))
                        .title
                        .trim()
                        .is_empty()
                );
                let key = (release.ordinal, name.clone());
                if let Some(other) = installed.insert(key, *id) {
                    assert_eq!(other, *id, "installed sets disagree on {name}");
                }
            }
            // Retained versions may write older sets; the current definitions
            // must be in bijection with the current generated variants.
            if release.ordinal == held.retained.last().unwrap().ordinal {
                assert_eq!(names, expected);
            }
        }
    }
    assert!(
        !DIALECTS.is_empty(),
        "the write-set law needs installed releases"
    );
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
