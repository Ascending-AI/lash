//! The build image is an immutable library, with no package publication work.
use super::*;
#[path = "helper_classes.rs"]
mod helper_classes;

/// Release metadata opens without decoding helper bodies or recomputing identity.
#[test]
fn release_metadata_opens_without_helper_body_work() {
    let _ = lash_kernel_doc::document_work::helper_work();
    let release = helper_release().expect("build helper library");
    assert!(!release.roots.is_empty());
    for root in &release.roots {
        assert!(release.helpers.contains(root));
    }
    assert_eq!(
        lash_kernel_doc::document_work::helper_work(),
        lash_kernel_doc::document_work::HelperWork::default()
    );
    assert_eq!(
        release.helpers.identity(),
        lash_kernel_value::HelperLibraryId::of_members(release.helpers.members().copied())
    );
}

use crate::archive::{Archive, ArchiveError, Binding, HelperId, Library, NativeIdentity, Release};
use std::collections::BTreeSet;

fn archive_fixture(definition: lash_kernel_doc::FunctionDefinition) -> Release {
    let function = definition.identity();
    let natives = if definition.has_native() {
        BTreeMap::from([(
            function,
            NativeIdentity {
                package: "fixture".to_owned(),
                name: definition.name.to_string(),
                native_version: definition.native_version,
            },
        )])
    } else {
        BTreeMap::new()
    };
    let definitions = BTreeMap::from([(function, definition)]);
    let contract =
        archive::contract(function, &definitions, &BTreeMap::new()).expect("closed fixture");
    let helper = HelperId(format!("fixture:{}", definitions[&function].name));
    Release {
        natives,
        library: Library {
            identity: lash_kernel_value::HelperLibraryId::of_members([function]),
            members: BTreeSet::from([function]),
            roots: BTreeSet::from([function]),
        },
        definitions,
        libraries: BTreeMap::from([(
            "fixture".to_owned(),
            Library {
                identity: lash_kernel_value::HelperLibraryId::of_members([function]),
                members: BTreeSet::from([function]),
                roots: BTreeSet::from([function]),
            },
        )]),
        bindings: BTreeMap::from([(
            helper,
            Binding {
                revision: contract.revision(),
                function,
            },
        )]),
        ..Release::default()
    }
}

fn definition_fixture() -> lash_kernel_doc::FunctionDefinition {
    lash_kernel_doc::parse_definition_with_locals(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return value }\n",
    )
    .expect("a fixture definition")
    .0
}

/// FIG-7504: changing the full guard limit, while keeping its unit and shape,
/// cannot reuse the old contract revision. Appending a revision admits it.
#[test]
fn changed_contract_requires_an_appended_revision() {
    let mut definition = definition_fixture();
    definition.implementation = lash_kernel_doc::Implementation::Native {
        charge: lash_kernel_doc::Formula::Constant(1),
    };
    definition.guard = Some(lash_kernel_doc::Guard {
        unit: "step".to_owned(),
        limit: lash_kernel_doc::Formula::Constant(10),
    });
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("first release");
    definition.guard.as_mut().expect("guard").limit = lash_kernel_doc::Formula::Constant(11);
    let next = first
        .append(archive_fixture(definition))
        .expect("new contract revision");
    first.check_append(&next).expect("append is permitted");
    let helper = HelperId("fixture:helper".to_owned());
    let mut violated = next;
    violated
        .ledger
        .get_mut(&helper)
        .expect("entry")
        .revisions
        .pop();
    assert_eq!(violated.validate(), Err(ArchiveError::Contract(helper)));
}

/// FIG-7504: a tombstone is permanent across nonadjacent releases. Reintroducing
/// the same contract is allowed; reusing the name for another contract refuses.
#[test]
fn tombstoned_helper_cannot_return_with_a_different_contract() {
    let definition = definition_fixture();
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("first release");
    let removed = first.append(Release::default()).expect("tombstone");
    removed
        .append(archive_fixture(definition.clone()))
        .expect("same contract can return");
    let mut changed = definition;
    changed.errors.insert("new_error".to_owned());
    assert_eq!(
        removed.append(archive_fixture(changed)),
        Err(ArchiveError::Contract(HelperId(
            "fixture:helper".to_owned()
        )))
    );
}

/// FIG-7504: keeping a native's current version does not satisfy an archived
/// declaration of the old version, even when package and name are identical.
#[test]
fn deleting_a_retained_native_version_fails_release_check() {
    let mut definition = definition_fixture();
    definition.implementation = lash_kernel_doc::Implementation::Native {
        charge: lash_kernel_doc::Formula::Constant(1),
    };
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("native v1");
    definition.native_version = 2;
    let next = first
        .append(archive_fixture(definition))
        .expect("native v2");
    let v1 = NativeIdentity {
        package: "fixture".to_owned(),
        name: "helper".to_owned(),
        native_version: 1,
    };
    let v2 = NativeIdentity {
        native_version: 2,
        ..v1.clone()
    };
    next.check_natives(&BTreeSet::from([v1.clone(), v2.clone()]))
        .expect("both implementations");
    assert_eq!(
        next.check_natives(&BTreeSet::from([v2])),
        Err(ArchiveError::Native(v1))
    );
}

/// FIG-7504: archive envelopes cannot authenticate corrupted definition bytes,
/// and an authentic member cannot call a missing exact callee.
#[test]
fn archive_authenticates_members_and_requires_closed_libraries() {
    let definition = definition_fixture();
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("valid archive");
    let mut corrupt = first.clone();
    corrupt.releases[0]
        .definitions
        .get_mut(&definition.identity())
        .expect("definition")
        .errors
        .insert("corrupt".to_owned());
    assert_eq!(
        corrupt.validate(),
        Err(ArchiveError::Hash(definition.identity()))
    );
    let mut dangling = definition;
    match &mut dangling.implementation {
        lash_kernel_doc::Implementation::Guest { body, .. } => {
            body.functions.insert(
                FunctionId::from_bytes([77; 32]),
                FunctionName::new("missing").expect("valid function name"),
            );
        }
        _ => unreachable!("guest fixture"),
    }
    let function = dangling.identity();
    let members = BTreeSet::from([function]);
    let violated = Archive {
        releases: vec![Release {
            definitions: BTreeMap::from([(function, dangling)]),
            libraries: BTreeMap::from([(
                "fixture".to_owned(),
                Library {
                    identity: lash_kernel_value::HelperLibraryId::of_members([function]),
                    members: members.clone(),
                    roots: members,
                },
            )]),
            ..Release::default()
        }],
        ..Archive::default()
    };
    assert!(matches!(violated.validate(), Err(ArchiveError::Closure(_))));
    let mut unlisted = definition_fixture();
    let lash_kernel_doc::Implementation::Guest { body, .. } = &mut unlisted.implementation else {
        unreachable!("guest fixture")
    };
    body.block = vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Call {
            function: FunctionId::from_bytes([78; 32]),
            args: vec![],
        },
    }];
    let function = unlisted.identity();
    let unlisted_archive = Archive {
        releases: vec![Release {
            definitions: BTreeMap::from([(function, unlisted)]),
            ..Release::default()
        }],
        ..Archive::default()
    };
    assert!(matches!(
        unlisted_archive.validate(),
        Err(ArchiveError::Closure(_))
    ));
    let mut wrong_members = first;
    wrong_members.releases[0]
        .libraries
        .get_mut("fixture")
        .expect("library")
        .identity = lash_kernel_value::HelperLibraryId::of_members([]);
    assert_eq!(
        wrong_members.validate(),
        Err(ArchiveError::Closure("fixture".to_owned()))
    );
}

/// FIG-7504: removing or rewriting old artifacts or ledger revisions is rejected,
/// even if the edited tree is internally self-consistent.
#[test]
fn release_history_can_only_append() {
    let first = Archive::default()
        .append(archive_fixture(definition_fixture()))
        .expect("release");
    assert_eq!(
        first.check_append(&Archive::default()),
        Err(ArchiveError::Rewritten)
    );
    let mut rewritten = first.clone();
    rewritten.ledger.clear();
    assert_eq!(first.check_append(&rewritten), Err(ArchiveError::Rewritten));
}

/// The release check consumes the entire checked-in history, not only current
/// hashes. Native availability is checked by the engine's actual Rust constructors.
#[test]
#[cfg(not(feature = "synthetic-next"))]
fn checked_archive_covers_current_release_and_native_implementations() {
    let archive = helper_archive().expect("checked archive");
    let candidate = archive_candidate().expect("build candidate");
    assert!(
        archive == candidate,
        "record the release with record-helper-data record_archive"
    );
    let definitions = archive.validate().expect("authenticated definitions");
    let bindings: BTreeMap<_, _> = archive
        .releases
        .iter()
        .flat_map(|release| release.natives.clone())
        .collect();
    let mut registry = FunctionRegistry::declarations();
    let mut pending = definitions;
    let mut available = BTreeSet::new();
    while !pending.is_empty() {
        let function = *pending
            .iter()
            .find(|(_, definition)| {
                definition
                    .body()
                    .is_none_or(|body| body.functions.keys().all(|id| registry.get(id).is_some()))
            })
            .expect("authenticated Merkle graph is acyclic")
            .0;
        let definition = pending.remove(&function).expect("pending definition");
        let native = if let Some(identity) = bindings.get(&function) {
            let callees: Vec<_> = definition
                .body()
                .into_iter()
                .flat_map(|body| body.functions.iter())
                .map(|(id, name)| (*id, name.as_str()))
                .collect();
            let native = lash_kernel_engine::embedding::native_implementation(
                (
                    identity.package.as_str(),
                    identity.name.as_str(),
                    identity.native_version,
                ),
                function,
                &callees,
                &registry,
            );
            if native.is_some() {
                available.insert(identity.clone());
            }
            native
        } else {
            None
        };
        registry
            .register(definition, native)
            .expect("archived definition registers");
    }
    archive
        .check_natives(&available)
        .expect("all archived native versions remain implemented in Rust");
}
