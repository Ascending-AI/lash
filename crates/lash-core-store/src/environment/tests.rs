use super::*;
use crate::tool_surface::ProviderKey;
use lash_vocabulary::tool_contract::ToolDefinition;
use serde_json::json;
use std::time::Duration;
use {lash_kernel_value::names::PackagePath, lash_kernel_value::names::Segment};
use {lash_kernel_value::package::*, lash_vocabulary::package::*};
use {lash_kernel_value::providers::FormatVersion, lash_vocabulary::providers::ExecutionBinding};

fn source(name: &str) -> SourceId {
    SourceId(name.into())
}
fn snapshot(name: &str, fragment: EnvironmentFragment) -> SourceSnapshot {
    SourceSnapshot {
        source: source(name),
        revision: SourceRevision {
            incarnation: uuid::Uuid::nil(),
            counter: 1,
        },
        fragment,
    }
}
fn provider() -> ProviderKey {
    ProviderKey::from_name(
        RegistryKind::Session,
        &ProviderName {
            owner: "fixture".into(),
            key: "tools".into(),
        },
    )
    .unwrap()
}
fn tool(id: &str, operation: &str) -> SurfaceEntry {
    let definition = ToolDefinition::raw(
        id,
        operation,
        "fixture",
        json!({"type":"object"}),
        json!({"type":"null"}),
    )
    .unwrap()
    .with_execution(Duration::from_secs(1));
    SurfaceEntry::advertised(
        provider(),
        definition,
        Some(ExecutionBinding {
            codec: provider(),
            format: FormatVersion::ONE,
            payload: json!({"a":1,"b":2}),
        }),
    )
}
fn fragment(id: &str, operation: &str) -> EnvironmentFragment {
    EnvironmentFragment {
        tools: vec![tool(id, operation)],
        ..EnvironmentFragment::default()
    }
}
fn context(publisher: &str) -> PublicationContext {
    PublicationContext {
        domain: PublicationDomain("fixture".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance(publisher.into()),
        root: Segment::new("pkg").unwrap(),
    }
}
fn package(path: &str, publisher: &str, deferred: bool) -> AcceptedAdvertisement {
    let path = PackagePath::parse(path).unwrap();
    let advertisement = if deferred {
        PackageAdvertisement::Deferred(Box::new(DeferredPackage {
            path: path.clone(),
            visibility: Visibility::Public,
            provider: ProviderName {
                owner: "fixture".into(),
                key: "tools".into(),
            },
            config: ProviderConfig {
                component: None,
                format: FormatVersion::ONE,
                value: json!({}),
                payload: None,
            },
            resolution_policy: lash_kernel_value::providers::DeferredResolutionPolicy::default(),
        }))
    } else {
        PackageAdvertisement::Enumerated(Package {
            path: path.clone(),
            members: BTreeMap::new(),
        })
    };
    let content = match &advertisement {
        PackageAdvertisement::Deferred(p) => PublishedContent::Deferred(p.clone()),
        PackageAdvertisement::Enumerated(package) => PublishedContent::Package(package.identity()),
    };
    AcceptedAdvertisement {
        publication: PublicationRecord {
            context: context(publisher),
            path,
            content,
            code_links: BTreeSet::new(),
            imported_publications: vec![],
        },
        advertisement,
    }
}

/// ENV-IDENTITY: membership/notification order and revisions do not affect the
/// hash; config, payloads, grants, policy and serving identities do.
#[test]
fn env_identity() {
    let mut a = fragment("a", "alpha.a");
    a.tools.push(tool("b", "alpha.b"));
    a.package_roots = vec![
        package("pkg.a", "one", false),
        package("pkg.b", "one", false),
    ];
    a.routes = a
        .tools
        .iter()
        .map(|entry| AdvertisedRoute {
            operation: entry.definition.manifest.operation.clone(),
            definition: entry.definition.clone(),
            route: entry.link.clone(),
        })
        .collect();
    a.config.insert("selection".into(), json!({"x":1,"y":2}));
    let deferred = match package("pkg.dynamic", "one", true).advertisement {
        PackageAdvertisement::Deferred(p) => p,
        _ => unreachable!(),
    };
    a.deferred_grants.push(DeferredGrant {
        provider: deferred.provider.clone(),
        context: context("one"),
        path: deferred.path.clone(),
        visibility: deferred.visibility,
        signature: lash_kernel_value::types::Signature {
            params: vec![],
            result: lash_kernel_value::types::Type::Json,
        },
        config: deferred.config.clone(),
        resolution_policy: deferred.resolution_policy,
    });
    let b = fragment("c", "beta.c");
    let original = assemble(None, [snapshot("a", a.clone()), snapshot("b", b.clone())]).unwrap();
    let mut reordered = a.clone();
    reordered.tools.reverse();
    reordered.routes.reverse();
    reordered.package_roots.reverse();
    reordered
        .config
        .insert("selection".into(), json!({"y":2,"x":1}));
    let mut changed_revision = snapshot("a", reordered);
    changed_revision.revision = SourceRevision {
        incarnation: uuid::Uuid::new_v4(),
        counter: 0,
    };
    let permutation = assemble(None, [snapshot("b", b), changed_revision]).unwrap();
    assert_eq!(original.id(), permutation.id());
    for change in 0..10 {
        let mut modified = a.clone();
        match change {
            0 => {
                modified
                    .config
                    .insert("selection".into(), json!({"x":2,"y":2}));
            }
            1 => {
                modified.tools[0].link.payload.as_mut().unwrap().payload = json!({"a":2,"b":2});
                modified.routes[0].route = modified.tools[0].link.clone();
            }
            2 => {
                modified.deferred_grants[0].config.value = json!({"allow":true});
            }
            3 => {
                modified
                    .identities
                    .insert(RegistryKind::Session, ClaimRequirements::default());
            }
            4 => {
                let policy = lash_kernel_value::ExecutionPolicy::repeatable(
                    std::num::NonZeroU32::MIN,
                    100,
                    1_000,
                );
                modified.tools[0].link.policy = policy;
                modified.tools[0].definition.manifest.execution_policy = policy;
                modified.routes[0].route = modified.tools[0].link.clone();
                modified.routes[0].definition = modified.tools[0].definition.clone();
            }
            5 => {
                modified.package_roots[0]
                    .publication
                    .code_links
                    .insert(LinkId::from_bytes([2; 32]));
            }
            6 => {
                modified
                    .engines
                    .insert("typescript".into(), "semantic-fingerprint".into());
            }
            7 => {
                modified.namespace_claims.push(context("one"));
            }
            8 => {
                modified
                    .model_removals
                    .insert(modified.tools[0].definition.manifest.operation.clone());
            }
            _ => {
                modified.deferred_provider = Some(provider());
            }
        }
        assert_ne!(
            assemble(Some(&original), [snapshot("a", modified)])
                .unwrap()
                .id(),
            original.id(),
            "change {change}"
        );
    }
    let mut first = EnvironmentFragment {
        deferred_provider: Some(provider()),
        ..Default::default()
    };
    let mut second = first.clone();
    second.deferred_provider = Some(
        ProviderKey::from_name(
            RegistryKind::Session,
            &ProviderName {
                owner: "different".into(),
                key: "tools".into(),
            },
        )
        .unwrap(),
    );
    assert!(matches!(
        assemble(
            None,
            [snapshot("first", first.clone()), snapshot("second", second)]
        ),
        Err(AssemblyRefusal::DeferredProvider { .. })
    ));
    first
        .model_removals
        .insert(a.tools[0].definition.manifest.operation.clone());
    let projected = assemble(
        None,
        [snapshot("projection", first), snapshot("tools", a.clone())],
    )
    .unwrap()
    .recorded_surface()
    .unwrap();
    assert!(
        !projected
            .tools()
            .contains_key(&a.tools[0].definition.manifest.operation)
    );
    assert_eq!(projected.deferred(), Some(&provider()));
    let encoded = serde_json::to_value(&original).unwrap();
    let decoded: EnvironmentBody = serde_json::from_value(encoded.clone()).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded, original);
    let mut corrupt = encoded;
    corrupt["id"] = json!("incorrect");
    assert_eq!(
        serde_json::from_value::<EnvironmentBody>(corrupt)
            .unwrap()
            .validate(),
        Err(AssemblyRefusal::ContentIdentity)
    );
    let schema = schemars::schema_for!(EnvironmentBody);
    assert!(serde_json::to_value(schema).unwrap()["$defs"]["EnvironmentFragment"].is_object());
}

/// FIG-7354: pure validation may share immutable facts, but a cold decoded
/// body must complete without recursively awaiting its own preparation.
#[test]
fn cold_environment_validation_authenticates_current_content_without_reentry() {
    let original = assemble(
        None,
        [snapshot("cold-validation", fragment("cold", "cold.value"))],
    )
    .unwrap();
    let encoded = serde_json::to_value(&original).unwrap();
    drop(original);
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let decoded: EnvironmentBody = serde_json::from_value(encoded).unwrap();
        send.send(decoded.validate()).unwrap();
    });
    assert_eq!(
        receive
            .recv_timeout(Duration::from_secs(2))
            .expect("cold validation completes"),
        Ok(())
    );
}

/// ENV-DELTA: only dirty contributions and dependents recompute; global checks
/// still see retained fragments, and failed replacement leaves the old body intact.
#[test]
fn env_delta() {
    let mut dependent = fragment("derived", "derived.call");
    dependent.dependencies.insert(source("a"));
    let initial = assemble(
        None,
        [
            snapshot("a", fragment("a", "alpha.a")),
            snapshot("b", fragment("b", "beta.b")),
            snapshot("derived", dependent.clone()),
        ],
    )
    .unwrap();
    let mut replacement = fragment("a", "alpha.a");
    replacement.config.insert("changed".into(), json!(true));
    let next = assemble(Some(&initial), [snapshot("a", replacement.clone())]).unwrap();
    assert!(Arc::ptr_eq(
        &initial.fragments()[&source("b")],
        &next.fragments()[&source("b")]
    ));
    assert!(!Arc::ptr_eq(
        &initial.fragments()[&source("a")],
        &next.fragments()[&source("a")]
    ));
    assert_ne!(
        initial.fragments()[&source("derived")].root(),
        next.fragments()[&source("derived")].root()
    );
    let rebuilt = assemble(
        None,
        [
            snapshot("derived", dependent),
            snapshot("b", fragment("b", "beta.b")),
            snapshot("a", replacement),
        ],
    )
    .unwrap();
    assert_eq!(next, rebuilt);
    let clean = assemble(Some(&next), []).unwrap();
    assert!(
        next.fragments()
            .iter()
            .all(|(id, old)| Arc::ptr_eq(old, &clean.fragments()[id]))
    );
    assert!(matches!(
        assemble(Some(&initial), [snapshot("a", fragment("new", "beta.b"))]),
        Err(AssemblyRefusal::OperationCollision { .. })
    ));
    assert!(matches!(
        assemble(Some(&initial), [snapshot("a", fragment("b", "alpha.a"))]),
        Err(AssemblyRefusal::DuplicateTool { .. })
    ));
    let mut roots = EnvironmentFragment {
        package_roots: vec![
            package("pkg.a", "one", false),
            package("pkg.b", "two", false),
        ],
        ..EnvironmentFragment::default()
    };
    assert!(matches!(
        assemble(None, [snapshot("packages", roots.clone())]),
        Err(AssemblyRefusal::NamespaceOwner { .. })
    ));
    roots.package_roots = vec![
        package("pkg.a", "one", true),
        package("pkg.a.child", "one", false),
    ];
    assert!(matches!(
        assemble(None, [snapshot("packages", roots)]),
        Err(AssemblyRefusal::NamespaceConflict { .. })
    ));
    let parent_and_child = EnvironmentFragment {
        package_roots: vec![
            package("pkg", "one", false),
            package("pkg.child", "one", true),
        ],
        ..EnvironmentFragment::default()
    };
    assemble(None, [snapshot("packages", parent_and_child)]).unwrap();
    let mut cycle = fragment("a", "alpha.a");
    cycle.dependencies.insert(source("derived"));
    assert!(matches!(
        assemble(Some(&initial), [snapshot("a", cycle)]),
        Err(AssemblyRefusal::DependencyCycle { .. })
    ));
    assert!(matches!(
        assemble(
            Some(&initial),
            [snapshot("a", EnvironmentFragment::default())]
        ),
        Err(AssemblyRefusal::MissingDependency { .. })
    ));
    assert_eq!(
        initial.fragments()[&source("a")].fragment().config,
        BTreeMap::new()
    );
}

/// FIG-7354: a resent snapshot whose membership lists arrive in another order
/// keeps its prepared fragment; a changed or duplicated member does not.
#[test]
fn a_reordered_unchanged_snapshot_keeps_its_prepared_fragment() {
    let mut first = fragment("a", "alpha.a");
    first.tools.push(tool("b", "alpha.b"));
    first.package_roots = vec![
        package("pkg", "one", false),
        package("pkg.child", "one", true),
    ];
    let initial = assemble(None, [snapshot("a", first.clone())]).unwrap();
    let mut reordered = first.clone();
    reordered.tools.reverse();
    reordered.package_roots.reverse();
    let next = assemble(Some(&initial), [snapshot("a", reordered.clone())]).unwrap();
    assert!(Arc::ptr_eq(
        &initial.fragments()[&source("a")],
        &next.fragments()[&source("a")]
    ));
    assert_eq!(next, initial);
    let mut duplicated = first;
    duplicated.tools[1] = duplicated.tools[0].clone();
    assert!(matches!(
        assemble(Some(&initial), [snapshot("a", duplicated)]),
        Err(AssemblyRefusal::OperationCollision { .. })
    ));
    reordered.tools[0] = tool("c", "alpha.c");
    let changed = assemble(Some(&initial), [snapshot("a", reordered)]).unwrap();
    assert!(!Arc::ptr_eq(
        &initial.fragments()[&source("a")],
        &changed.fragments()[&source("a")]
    ));
    assert_ne!(changed.id(), initial.id());
}

/// ENV-DELTA / L-STANDARD: package-only operations never enter the model catalog.
#[test]
fn env_package_only_delta_preserves_model_tools() {
    let before = assemble(None, [snapshot("tools", fragment("kept", "model.work"))]).unwrap();
    let mut accepted = package("pkg", "service", false);
    let PackageAdvertisement::Enumerated(package) = &mut accepted.advertisement else {
        unreachable!()
    };
    package.members.insert(
        Segment::new("read").unwrap(),
        Member {
            visibility: Visibility::Internal,
            kind: MemberKind::Operation(OperationOffer {
                requirement: lash_kernel_value::OperationRequirement {
                    signature: lash_kernel_value::Signature {
                        params: vec![],
                        result: lash_kernel_value::Type::Null,
                    },
                    contract: lash_kernel_value::ContractId::new("service.read"),
                },
                intents: Default::default(),
                controls: Default::default(),
                mode: OperationMode::Read,
            }),
        },
    );
    accepted.publication.content = PublishedContent::Package(package.identity());
    let after = assemble(
        Some(&before),
        [snapshot(
            "services",
            EnvironmentFragment {
                package_roots: vec![accepted],
                ..Default::default()
            },
        )],
    )
    .unwrap();
    assert_ne!(before.id(), after.id());
    let before = before.recorded_surface().unwrap();
    let after = after.recorded_surface().unwrap();
    assert_eq!(
        before.definitions().collect::<Vec<_>>(),
        after.definitions().collect::<Vec<_>>()
    );
    assert!(before.packages().snapshot.advertisements.is_empty());
    assert_eq!(after.packages().snapshot.advertisements.len(), 1);
}

/// ENV-REINCARNATE: content identity can repeat; a retired owner identity cannot.
#[test]
fn env_reincarnate() {
    let body = assemble(None, [snapshot("a", fragment("a", "alpha.a"))]).unwrap();
    let mut first = VersionOwner::preparing(body.id().clone());
    first.transition(VersionOwnerState::Published).unwrap();
    let head = EnvironmentHead {
        version: body.id().clone(),
        owner: first.id().clone(),
        input_frontier: BTreeMap::from([(
            source("a"),
            snapshot("a", EnvironmentFragment::default()).revision,
        )]),
    };
    let cell = head.round();
    first.transition(VersionOwnerState::Retired).unwrap();
    assert!(first.transition(VersionOwnerState::Published).is_err());
    assert!(first.transition(VersionOwnerState::Preparing).is_err());
    let mut second = VersionOwner::preparing(body.id().clone());
    assert_ne!(first.id(), second.id());
    assert_eq!(first.version(), second.version());
    assert_eq!(cell.owner, *first.id());
    second.transition(VersionOwnerState::Retired).unwrap();
    assert!(second.transition(VersionOwnerState::Published).is_err());
    let encoded = serde_json::to_string(&head).unwrap();
    assert_eq!(
        serde_json::from_str::<EnvironmentHead>(&encoded).unwrap(),
        head
    );
    let restored: VersionOwner =
        serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
    assert_eq!(restored.state(), VersionOwnerState::Retired);
    for change in [
        EnvironmentChange::Refresh { sources: None },
        EnvironmentChange::ConfigSelection {
            sources: BTreeSet::from([source("a")]),
        },
        EnvironmentChange::SourceRevision {
            source: source("a"),
            revision: head.input_frontier[&source("a")].clone(),
        },
    ] {
        assert_eq!(
            serde_json::from_value::<EnvironmentChange>(serde_json::to_value(&change).unwrap())
                .unwrap(),
            change
        );
    }
}
