//! The laws of the helper releases a build holds, and the generator that
//! seals the release the tree builds.

// FIG-2971: this file is test/tooling code; the generator's ambient fs/env
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::fmt::Write as _;

use super::*;

#[path = "build/release.rs"]
mod release;

fn helper(result: i64) -> lash_kernel_doc::FunctionDefinition {
    lash_kernel_doc::parse_definition(&format!(
        "function helper() -> Int\nkernel 1\ncharge 1\nbody {{ return {result} }}\n"
    ))
    .expect("a helper definition")
}

fn helper_registry(result: i64) -> lash_kernel_doc::FunctionRegistry {
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    registry
        .register(helper(result), None)
        .expect("register the helper");
    registry
}

/// FIG-5821: the unsealed 1.0 baseline holds only today's functions;
/// superseded pre-release helpers must not grow every worker's catalog.
#[test]
fn unsealed_helper_baseline_contains_only_current_functions() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let previous = release::freeze("1.0", 1, None, &old, &fingerprints);
    let frozen = release::freeze("1.0", 1, Some(&previous), &current, &fingerprints);
    assert_eq!(frozen.functions.len(), 1, "no superseded unsealed helper");
    assert_eq!(frozen.index().functions, frozen.writes);
    assert!(
        release::keep(&[frozen], &current, &fingerprints)
            .expect("the baseline matches the build")
            .is_empty()
    );
}

/// FIG-5799: a sealed release still protects runnable functions a run pins.
#[test]
fn sealed_helper_freezes_keep_runnable_functions() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let mut previous = release::freeze("1.0", 1, None, &old, &fingerprints);
    previous.seal().expect("seal the shipped release");
    let building = release::freeze("2.0", 2, Some(&previous), &current, &fingerprints);
    assert_eq!(
        building.functions.len(),
        2,
        "the sealed helper remains held"
    );
    assert_eq!(building.writes.len(), 1, "only the current helper is named");
    let kept = release::keep(&[previous, building], &current, &fingerprints)
        .expect("the sealed helper remains runnable");
    assert_eq!(kept.len(), 1);
}

/// V17: a sealed building release cannot silently write changed helpers.
#[test]
fn a_sealed_building_release_rejects_changed_writes() {
    let old = helper_registry(1);
    let current = helper_registry(2);
    let fingerprints = Default::default();
    let mut sealed = release::freeze("1.0", 1, None, &old, &fingerprints);
    sealed.seal().expect("seal the baseline");
    assert!(
        sealed.seal().is_err(),
        "sealing is explicit and happens once"
    );
    assert!(release::keep(&[sealed], &current, &fingerprints).is_err());
}

/// FIG-5839: the build defines the release the tree builds only while no
/// sealed release is kept for it; once sealed, the build holds it as it
/// shipped, never defined again (V17). Every other declared release is a
/// sealed one the repository keeps, and every sealed one is declared, so a
/// declaration and its artifact cannot drift apart (V18).
#[test]
fn only_the_release_the_tree_builds_is_defined_by_the_build() {
    let fingerprints = Default::default();
    let mut shipped = release::freeze("1.0", 1, None, &helper_registry(1), &fingerprints);
    shipped.seal().expect("seal 1.0");
    let shipped_text = serde_json::to_string(&shipped).expect("1.0's JSON");
    let unsealed_text = serde_json::to_string(&release::freeze(
        "1.0",
        1,
        None,
        &helper_registry(1),
        &fingerprints,
    ))
    .expect("an unsealed 1.0's JSON");
    let define = |name: &str, ordinal: u32, previous: Option<&HelperRelease>| {
        release::freeze(name, ordinal, previous, &helper_registry(2), &fingerprints)
    };

    let unsealed = release::resolve(&[("1.0", 1)], &[], &[], define).expect("the build's 1.0");
    assert!(!unsealed.retained[0].sealed, "the build defines 1.0");

    let sealed = release::resolve(
        &[("1.0", 1)],
        &[],
        &[&shipped_text],
        |_, _, _| -> HelperRelease { panic!("a sealed release is never defined again") },
    )
    .expect("the sealed 1.0");
    assert_eq!(sealed.retained, vec![shipped.clone()]);

    let next = release::resolve(&[("1.0", 1), ("2.0", 2)], &[], &[&shipped_text], define)
        .expect("the build's 2.0 after the sealed 1.0");
    assert_eq!(
        next.retained[1].functions.len(),
        2,
        "2.0 keeps 1.0's helper"
    );

    let retiring = release::resolve(&[("2.0", 2)], &[("1.0", 1)], &[&shipped_text], define)
        .expect("1.0 retiring");
    assert_eq!(retiring.retiring, vec![shipped]);

    for (retained, retiring, kept, refusal) in [
        (
            &[("1.0", 1), ("2.0", 2)][..],
            &[][..],
            &[][..],
            "an earlier release the repository does not keep",
        ),
        (
            &[("2.0", 2)],
            &[],
            &[&shipped_text[..]][..],
            "an undeclared sealed release",
        ),
        (
            &[("2.0", 2)],
            &[("1.0", 1)],
            &[],
            "a retiring release the repository does not keep",
        ),
        (
            &[("1.0", 2)],
            &[],
            &[&shipped_text],
            "a sealed release kept under another ordinal",
        ),
        (
            &[("1.0", 1)],
            &[],
            &[&unsealed_text],
            "an unsealed release kept as shipped",
        ),
        (&[], &[], &[], "no retained release"),
    ] {
        assert!(
            release::resolve(retained, retiring, kept, define).is_err(),
            "{refusal} is refused"
        );
    }
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
        assert_eq!(declared(&held.retained), named(RETAINED_HELPER_RELEASES));
        assert_eq!(declared(&held.retiring), named(RETIRING_HELPER_RELEASES));
        let index: Vec<HelperReleaseIndex> =
            serde_json::from_slice(built.helper_release_index).expect("the index decodes");
        assert_eq!(
            index,
            held.retained
                .iter()
                .map(HelperRelease::index)
                .collect::<Vec<_>>()
        );
        let helpers = lash_kernel_doc::ValidatedFunctions::from_json(built.helpers)
            .expect("the helpers decode");
        let building = held.retained.last().expect("the release the tree builds");
        for (function, _) in helpers.iter() {
            assert!(
                building.writes.contains(function),
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
        lash_kernel_doc::ValidatedFunctions::from_json(built.helpers)
            .expect("the helpers decode")
            .iter()
            .map(|(function, _)| *function)
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

/// Seals each built dialect's release the tree builds, and keeps every
/// sealed release, in `src/sealed.rs`: the one act of the cut that ships
/// them (`SEAL`), run in a build with every shipped dialect. A build after
/// it holds each release as it shipped, and changes a helper by declaring
/// the next release. Run again, it keeps what is sealed and seals a
/// dialect's release the tree builds if it is new; a release no longer
/// declared is dropped.
#[test]
#[ignore = "regenerates crates/lash-vm-releases/src/sealed.rs"]
fn seal_helper_release() {
    assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
    assert_eq!(
        DIALECTS
            .iter()
            .map(|built| built.dialect)
            .collect::<Vec<_>>(),
        SHIPPED_DIALECTS,
        "the cut seals every shipped dialect: build with each dialect feature"
    );
    let mut sealed = Vec::new();
    for built in DIALECTS {
        let held = built.held_releases().expect("the build's releases");
        let mut releases: Vec<HelperRelease> =
            held.retiring.into_iter().chain(held.retained).collect();
        releases.sort_by_key(|release| release.ordinal);
        for release in &mut releases {
            if !release.sealed {
                release.seal().expect("seal the release the tree builds");
            }
        }
        sealed.push((built.dialect, releases));
    }
    let workspace =
        std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("the regeneration workspace");
    std::fs::write(
        std::path::Path::new(&workspace).join("crates/lash-vm-releases/src/sealed.rs"),
        sealed_source(&sealed).expect("the sealed releases' source"),
    )
    .expect("the sealed releases are written");
}

/// The source `src/sealed.rs` keeps each dialect's `releases` in, oldest
/// first: each release's JSON, one function to a line, as a string
/// constant. A dialect with no sealed release has no entry.
fn sealed_source(dialects: &[(&str, Vec<HelperRelease>)]) -> Result<String, String> {
    let mut text = format!(
        "// @generated by `{SEAL}`; do not edit.\n\
         //! The helper releases that have shipped, by dialect, oldest first, each as\n\
         //! it was sealed: every function a build of it holds (FIG-5799, FIG-5990).\n\
         //! Until the 1.0 cut none has, and the build defines each dialect's release\n\
         //! the tree builds (FIG-5839).\n\n\
         pub(crate) const SEALED: &[(&str, &[&str])] = &["
    );
    let mut any = false;
    for (dialect, releases) in dialects {
        if releases.is_empty() {
            continue;
        }
        any = true;
        let _ = write!(text, "\n    ({}, &[", json(dialect)?);
        for release in releases {
            let mut body = format!(
                "{{\"release\":{},\"ordinal\":{},",
                json(&release.release)?,
                release.ordinal
            );
            if release.sealed {
                body.push_str("\"sealed\":true,");
            }
            let _ = write!(
                body,
                "\"writes\":{},\"functions\":[",
                json(&release.writes)?
            );
            for (index, function) in release.functions.iter().enumerate() {
                body.push_str(if index == 0 { "\n" } else { ",\n" });
                body.push_str(&json(function)?);
            }
            body.push_str("\n]}\n");
            let mut hashes = String::from("#");
            while body.contains(&format!("\"{hashes}")) {
                hashes.push('#');
            }
            let _ = write!(text, "\n        r{hashes}\"{body}\"{hashes},");
        }
        text.push_str("\n    ]),");
    }
    if any {
        text.push('\n');
    }
    text.push_str("];\n");
    Ok(text)
}

/// The source the sealed releases are kept in decodes to them, and the
/// source of none is the one the repository keeps before the 1.0 cut.
#[test]
fn the_sealed_source_keeps_the_releases_it_is_given() {
    assert_eq!(
        sealed_source(&[]).expect("no release"),
        include_str!("sealed.rs")
    );
    assert_eq!(
        sealed_source(&[(TYPESCRIPT, Vec::new())]).expect("a dialect with none"),
        include_str!("sealed.rs")
    );
    let mut shipped = release::freeze("1.0", 1, None, &helper_registry(1), &Default::default());
    shipped.seal().expect("seal 1.0");
    let source = sealed_source(&[(PYTHON, vec![shipped.clone()])]).expect("1.0's source");
    assert!(source.contains("(\"python\", &["), "{source}");
    let start = source.find("r#\"").expect("a raw string") + 3;
    let end = source.rfind("\"#").expect("its end");
    assert_eq!(
        HelperRelease::decode(&source[start..end]).expect("1.0 decodes"),
        shipped
    );
}

fn json(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
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

fn released(definition: lash_kernel_doc::FunctionDefinition, native: bool) -> ReleasedFunction {
    ReleasedFunction {
        function: definition.identity().expect("an identity"),
        definition,
        fingerprint: native.then(|| "f".repeat(64)),
    }
}

fn one_release(result: i64) -> HelperRelease {
    let native = lash_kernel_doc::parse_definition(
        "function num.neg(x: Int) -> Int\nkernel 1\ncharge 1\nnative\n",
    )
    .expect("a native definition");
    let functions = vec![released(native, true), released(helper(result), false)];
    HelperRelease {
        release: "1.0".into(),
        ordinal: 1,
        sealed: false,
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
        retiring: Vec::new(),
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
                retiring: Vec::new(),
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
    let stranger = helper(9).identity().unwrap();
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

#[path = "build/probe.rs"]
mod probe;

/// K-LIB-011: changing only the prepared answer changes the release digest.
#[test]
fn native_fingerprint_covers_the_prepared_entry() {
    use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};
    struct Native(bool);
    impl NativeFunction for Native {
        fn call(&self, _: NativeCall<'_>) -> Result<Value, NativeError> {
            Ok(Value::Bool(true))
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                operation: "probe / Bool",
                entry: if self.0 {
                    |_| Some(Value::Bool(true))
                } else {
                    |_| Some(Value::Bool(false))
                },
            })
        }
    }
    let registry = |answer| {
        let mut registry = lash_kernel_doc::FunctionRegistry::new();
        let definition = lash_kernel_doc::parse_definition(
            "function probe() -> Bool\nkernel 1\ncharge 1\nnative\n",
        )
        .unwrap();
        registry
            .register(definition, Some(std::sync::Arc::new(Native(answer))))
            .unwrap();
        registry
    };
    assert_ne!(
        probe::fingerprints(&registry(true)),
        probe::fingerprints(&registry(false))
    );
}
