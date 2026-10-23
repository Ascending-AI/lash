use super::*;
use lash_vocabulary::names::{PackagePath, QualifiedOperation, Segment};
use lash_vocabulary::package::{PackageAdvertisement, PublicationRecord};
use lash_vocabulary::tool_contract::ToolId;

/// version_surface = "coexist"
/// version_guard(items(ENVIRONMENT_DOMAIN, assemble))
const ENVIRONMENT_DOMAIN: &str = "lash-session-environment/v1";
/// version_surface = "coexist"
/// version_guard(items(FRAGMENT_DOMAIN, prepare))
const FRAGMENT_DOMAIN: &str = "lash-session-environment-fragment/v1";

/// Typed refusals are retained across source, actor and host boundaries.
#[derive(
    Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssemblyRefusal {
    #[error("source {source_id:?} supplied conflicting snapshots in one delta")]
    ConflictingSnapshot { source_id: SourceId },
    #[error("source {source_id:?} depends on missing source {dependency:?}")]
    MissingDependency {
        source_id: SourceId,
        dependency: SourceId,
    },
    #[error("fragment dependency cycle contains {source_id:?}")]
    DependencyCycle { source_id: SourceId },
    #[error("operation {operation:?} occurs in sources {first:?} and {second:?}")]
    OperationCollision {
        operation: QualifiedOperation,
        first: SourceId,
        second: SourceId,
    },
    #[error("tool {tool} occurs in sources {first:?} and {second:?}")]
    DuplicateTool {
        tool: ToolId,
        first: SourceId,
        second: SourceId,
    },
    #[error("source {source_id:?} has an inconsistent route or definition for {tool}")]
    InconsistentRoute { source_id: SourceId, tool: ToolId },
    #[error("namespace root {root} has competing publication owners")]
    NamespaceOwner {
        root: Segment,
        first: Box<PublicationContext>,
        second: Box<PublicationContext>,
    },
    #[error("publication domains {first:?} and {second:?} disagree")]
    NamespaceDomain {
        first: lash_vocabulary::package::PublicationDomain,
        second: lash_vocabulary::package::PublicationDomain,
    },
    #[error("namespace paths {path} and {other} conflict")]
    NamespaceConflict {
        path: PackagePath,
        other: PackagePath,
    },
    #[error("publication provenance disagrees with its advertisement at {path}")]
    Publication { path: PackagePath },
    #[error("deferred providers {first} and {second} disagree")]
    DeferredProvider {
        first: crate::tool_surface::ProviderKey,
        second: crate::tool_surface::ProviderKey,
    },
    #[error("fragment consumers disagree")]
    Consumer,
    #[error("stored environment identity does not match canonical content")]
    ContentIdentity,
    #[error("environment encoding failed: {message}")]
    Encoding { message: String },
}

/// Replace dirty source fragments, recompute their roots and transitive
/// dependents, and check global invariants over the whole candidate body.
/// Notification order is irrelevant: one delta may repeat an identical snapshot
/// but must not contain two different observations of the same source. The actor
/// coalesces notifications and obtains one coherent snapshot first.
///
/// Revisions are deliberately not compared here: a new incarnation has no
/// ordering relation to the old one. The actor owns reconciliation and the
/// frontier. An empty fragment is a detach and removes the source. Dependencies
/// on a detached source refuse rather than silently adopting stale facts.
///
/// No I/O, providers, clocks, random numbers or stores are touched. Clean
/// fragments retain their Arc, including their precomputed content root.
pub fn assemble(
    previous: Option<&EnvironmentBody>,
    dirty: impl IntoIterator<Item = SourceSnapshot>,
) -> Result<EnvironmentBody, AssemblyRefusal> {
    let mut updates = BTreeMap::new();
    for snapshot in dirty {
        if let Some(first) = updates.get(&snapshot.source)
            && first != &snapshot
        {
            return Err(AssemblyRefusal::ConflictingSnapshot {
                source_id: snapshot.source,
            });
        }
        updates.insert(snapshot.source.clone(), snapshot);
    }
    let mut fragments = previous
        .map(|body| body.fragments.clone())
        .unwrap_or_default();
    let mut changed = BTreeMap::new();
    let mut affected: BTreeSet<_> = updates.keys().cloned().collect();
    for (source, snapshot) in updates {
        if snapshot.fragment == EnvironmentFragment::default() {
            fragments.remove(&source);
        } else {
            changed.insert(source, normalize(snapshot.fragment)?);
        }
    }
    // A reverse dependency index is projected from the sole authored membership.
    let sources: BTreeSet<_> = fragments.keys().chain(changed.keys()).cloned().collect();
    let mut dependents = BTreeMap::<SourceId, BTreeSet<SourceId>>::new();
    for source in &sources {
        let fragment = changed
            .get(source)
            .unwrap_or_else(|| &fragments[source].fragment);
        for dependency in &fragment.dependencies {
            if !sources.contains(dependency) {
                return Err(AssemblyRefusal::MissingDependency {
                    source_id: source.clone(),
                    dependency: dependency.clone(),
                });
            }
            dependents
                .entry(dependency.clone())
                .or_default()
                .insert(source.clone());
        }
    }
    let mut pending: Vec<_> = affected.iter().cloned().collect();
    while let Some(source) = pending.pop() {
        for dependent in dependents.get(&source).into_iter().flatten() {
            if affected.insert(dependent.clone()) {
                pending.push(dependent.clone());
            }
        }
    }
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for source in &affected {
        prepare(
            source,
            &affected,
            &changed,
            &mut fragments,
            &mut visiting,
            &mut done,
        )?;
    }
    validate_global(&fragments)?;
    let roots: BTreeMap<_, _> = fragments
        .iter()
        .map(|(source, prepared)| (source, &prepared.root))
        .collect();
    let id = EnvironmentId(crate::stable_hash::blake3_hex(
        ENVIRONMENT_DOMAIN,
        &bytes(&roots)?,
    ));
    Ok(EnvironmentBody { fragments, id })
}

fn prepare(
    source: &SourceId,
    affected: &BTreeSet<SourceId>,
    changed: &BTreeMap<SourceId, EnvironmentFragment>,
    fragments: &mut BTreeMap<SourceId, Arc<PreparedFragment>>,
    visiting: &mut BTreeSet<SourceId>,
    done: &mut BTreeSet<SourceId>,
) -> Result<(), AssemblyRefusal> {
    if done.contains(source)
        || (!fragments.contains_key(source) && !changed.contains_key(source))
        || !affected.contains(source)
    {
        return Ok(());
    }
    if !visiting.insert(source.clone()) {
        return Err(AssemblyRefusal::DependencyCycle {
            source_id: source.clone(),
        });
    }
    let dependencies = changed
        .get(source)
        .unwrap_or_else(|| &fragments[source].fragment)
        .dependencies
        .clone();
    for dependency in &dependencies {
        prepare(dependency, affected, changed, fragments, visiting, done)?;
    }
    let roots: BTreeMap<_, _> = dependencies
        .iter()
        .map(|id| (id, &fragments[id].root))
        .collect();
    let fragment = changed
        .get(source)
        .unwrap_or_else(|| &fragments[source].fragment);
    let root = FragmentId(crate::stable_hash::blake3_hex(
        FRAGMENT_DOMAIN,
        &bytes(&(fragment, roots))?,
    ));
    if changed.contains_key(source) || fragments.get(source).is_none_or(|old| old.root != root) {
        let prepared = PreparedFragment {
            fragment: fragment.clone(),
            root,
        };
        fragments.insert(source.clone(), Arc::new(prepared));
    }
    visiting.remove(source);
    done.insert(source.clone());
    Ok(())
}

fn bytes(value: &impl Serialize) -> Result<Vec<u8>, AssemblyRefusal> {
    let value = serde_json::to_value(value).map_err(|error| AssemblyRefusal::Encoding {
        message: error.to_string(),
    })?;
    Ok(crate::identity_json::payload_leaf(&value))
}
fn order<T: Serialize>(values: Vec<T>) -> Result<Vec<T>, AssemblyRefusal> {
    let mut keyed = values
        .into_iter()
        .map(|value| Ok((bytes(&value)?, value)))
        .collect::<Result<Vec<_>, AssemblyRefusal>>()?;
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(keyed.into_iter().map(|(_, value)| value).collect())
}
fn normalize(mut fragment: EnvironmentFragment) -> Result<EnvironmentFragment, AssemblyRefusal> {
    fragment.tools = order(fragment.tools)?;
    fragment.routes = order(fragment.routes)?;
    fragment.deferred_grants = order(fragment.deferred_grants)?;
    fragment.namespace_claims = order(fragment.namespace_claims)?;
    // These are also membership lists, unlike ordered parameter or payload arrays.
    for root in &mut fragment.package_roots {
        if !publication_matches(&root.publication, &root.advertisement) {
            return Err(AssemblyRefusal::Publication {
                path: root.publication.path.clone(),
            });
        }
        root.publication.imported_publications.sort();
    }
    fragment.package_roots = order(fragment.package_roots)?;
    Ok(fragment)
}

fn validate_global(
    fragments: &BTreeMap<SourceId, Arc<PreparedFragment>>,
) -> Result<(), AssemblyRefusal> {
    let mut operations = BTreeMap::new();
    let mut tools = BTreeMap::new();
    let mut owners: BTreeMap<&Segment, &PublicationContext> = BTreeMap::new();
    let mut packages = BTreeMap::<&PackagePath, &PackageAdvertisement>::new();
    let mut consumer = None;
    let mut deferred: Option<&crate::tool_surface::ProviderKey> = None;
    for (source, prepared) in fragments {
        let fragment = &prepared.fragment;
        if let Some(provider) = &fragment.deferred_provider {
            if let Some(first) = deferred
                && first != provider
            {
                return Err(AssemblyRefusal::DeferredProvider {
                    first: first.clone(),
                    second: provider.clone(),
                });
            }
            deferred = Some(provider);
        }
        if let Some(context) = &fragment.consumer {
            if consumer.is_some_and(|old| old != context) {
                return Err(AssemblyRefusal::Consumer);
            }
            consumer = Some(context);
        }
        // Tools and package candidates may project the same route once each.
        // Deduplicate that one shared fact only within its coherent source.
        let mut source_routes = BTreeMap::new();
        for entry in &fragment.tools {
            let operation = &entry.definition.manifest.operation;
            if let Some(first) = operations.insert(operation, source) {
                return Err(AssemblyRefusal::OperationCollision {
                    operation: operation.clone(),
                    first: first.clone(),
                    second: source.clone(),
                });
            }
            if let Some(first) = tools.insert(entry.tool(), source) {
                return Err(AssemblyRefusal::DuplicateTool {
                    tool: entry.tool().clone(),
                    first: first.clone(),
                    second: source.clone(),
                });
            }
            validate_route(source, operation, &entry.definition, &entry.link)?;
            source_routes.insert(entry.tool(), (operation, &entry.definition, &entry.link));
        }
        for candidate in &fragment.routes {
            validate_route(
                source,
                &candidate.operation,
                &candidate.definition,
                &candidate.route,
            )?;
            let fact = (
                &candidate.operation,
                &candidate.definition,
                &candidate.route,
            );
            if let Some(first) = source_routes.insert(&candidate.route.tool, fact) {
                if first != fact {
                    return Err(AssemblyRefusal::InconsistentRoute {
                        source_id: source.clone(),
                        tool: candidate.route.tool.clone(),
                    });
                }
                continue;
            }
            if let Some(first) = operations.insert(&candidate.operation, source) {
                return Err(AssemblyRefusal::OperationCollision {
                    operation: candidate.operation.clone(),
                    first: first.clone(),
                    second: source.clone(),
                });
            }
            if let Some(first) = tools.insert(&candidate.route.tool, source) {
                return Err(AssemblyRefusal::DuplicateTool {
                    tool: candidate.route.tool.clone(),
                    first: first.clone(),
                    second: source.clone(),
                });
            }
        }
        for accepted in &fragment.package_roots {
            let publication = &accepted.publication;
            let path = advertisement_path(&accepted.advertisement);
            if path != &publication.path || path.as_slice()[0] != publication.context.root {
                return Err(AssemblyRefusal::Publication { path: path.clone() });
            }
            if let Some(first) = owners.insert(&publication.context.root, &publication.context)
                && first != &publication.context
            {
                return Err(AssemblyRefusal::NamespaceOwner {
                    root: publication.context.root.clone(),
                    first: Box::new(first.clone()),
                    second: Box::new(publication.context.clone()),
                });
            }
            if packages.insert(path, &accepted.advertisement).is_some() {
                return Err(AssemblyRefusal::NamespaceConflict {
                    path: path.clone(),
                    other: path.clone(),
                });
            }
        }
        for context in fragment
            .namespace_claims
            .iter()
            .chain(fragment.deferred_grants.iter().map(|grant| &grant.context))
        {
            if let Some(first) = owners.insert(&context.root, context)
                && first != context
            {
                return Err(AssemblyRefusal::NamespaceOwner {
                    root: context.root.clone(),
                    first: Box::new(first.clone()),
                    second: Box::new(context.clone()),
                });
            }
        }
        for grant in &fragment.deferred_grants {
            if grant.path.as_slice()[0] != grant.context.root {
                return Err(AssemblyRefusal::Publication {
                    path: grant.path.clone(),
                });
            }
        }
    }
    let mut domain = consumer.map(|context| &context.domain);
    for context in owners.values() {
        if let Some(first) = domain
            && first != &context.domain
        {
            return Err(AssemblyRefusal::NamespaceDomain {
                first: first.clone(),
                second: context.domain.clone(),
            });
        }
        domain = Some(&context.domain);
    }
    let mut index = super::NamespaceIndex::default();
    for (path, advertisement) in packages {
        index
            .insert(advertisement)
            .map_err(|other| AssemblyRefusal::NamespaceConflict {
                path: path.clone(),
                other,
            })?;
    }
    Ok(())
}

fn validate_route(
    source: &SourceId,
    operation: &QualifiedOperation,
    definition: &lash_vocabulary::tool_contract::ToolDefinition,
    route: &crate::tool_surface::OperationRoute,
) -> Result<(), AssemblyRefusal> {
    if operation != &definition.manifest.operation
        || route.tool != definition.manifest.id
        || route.policy != definition.manifest.execution_policy
        || route.projection != definition.manifest.argument_projection
        || route.admitted(Some(definition)).as_ref() != Some(definition)
    {
        return Err(AssemblyRefusal::InconsistentRoute {
            source_id: source.clone(),
            tool: route.tool.clone(),
        });
    }
    Ok(())
}
fn advertisement_path(advertisement: &PackageAdvertisement) -> &PackagePath {
    match advertisement {
        PackageAdvertisement::Enumerated(package) => &package.path,
        PackageAdvertisement::Deferred(package) => &package.path,
    }
}
fn publication_matches(
    publication: &PublicationRecord,
    advertisement: &PackageAdvertisement,
) -> bool {
    use lash_vocabulary::package::PublishedContent;
    match (&publication.content, advertisement) {
        (PublishedContent::Package(id), PackageAdvertisement::Enumerated(package)) => {
            *id == package.identity()
        }
        (PublishedContent::Deferred(recorded), PackageAdvertisement::Deferred(advertised)) => {
            recorded == advertised
        }
        _ => false,
    }
}
