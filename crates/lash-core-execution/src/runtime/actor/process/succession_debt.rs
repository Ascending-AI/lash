//! The owner projects live creation-code debt from retained predecessor facts.
use lash_core_store::process_env::ProcessEnv;
use lash_execution_contracts::process::linking::{
    AdmissionRefusal, EnginePortError, LinkInspection, ReachSelector,
};
use lash_execution_contracts::process::{ProcessEngine, SuccessionContext};
use lash_kernel_value::CallableId;
use std::collections::{BTreeMap, BTreeSet};

fn refused(message: impl Into<String>) -> EnginePortError {
    EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
        message: message.into(),
    })
}

pub(crate) async fn merge(
    engine: &dyn ProcessEngine,
    packages: &dyn lash_durable::packages::PackageStore,
    context: &SuccessionContext,
    successor: &ProcessEnv,
    successor_inspection: &LinkInspection,
    code: &BTreeSet<CallableId>,
) -> Result<(ProcessEnv, LinkInspection), EnginePortError> {
    let mut environment = successor.execution().clone();
    let mut inspection = successor_inspection.clone();
    let mut allowed = BTreeSet::new();
    for link in &context.predecessor.roots {
        allowed.extend(
            engine
                .inspect_link(*link, &ReachSelector::AllExports)
                .await?
                .artifacts,
        );
    }
    let mut uses = BTreeMap::new();
    let mut code_artifacts = BTreeSet::new();
    for callable in code {
        let retained = engine
            .inspect_link(
                callable.link,
                &ReachSelector::Callable {
                    function: callable.function,
                },
            )
            .await?;
        if retained
            .artifacts
            .iter()
            .any(|artifact| !allowed.contains(artifact))
        {
            return Err(refused(
                "carried code is outside retained predecessor roots",
            ));
        }
        environment.roots.insert(callable.link);
        if context.predecessor.authored.contains(&callable.link) {
            environment.authored.insert(callable.link);
        }
        environment.requirements.machine.extend(&retained.machine);
        inspection.machine.extend(&retained.machine);
        code_artifacts.extend(retained.artifacts.iter().cloned());
        inspection.artifacts.extend(retained.artifacts);
        uses.extend(retained.operations);
    }
    for (use_, need) in uses {
        let binding = context
            .predecessor
            .uses
            .get(&use_)
            .ok_or_else(|| refused("live predecessor code has no retained binding"))?;
        if let Some(authored) = environment.uses.get(&use_) {
            if authored != binding {
                return Err(refused("code debt changes an authored successor use"));
            }
        } else {
            environment.uses.insert(use_, binding.clone());
        }
        let name = match &need {
            lash_execution_contracts::process::linking::OperationNeed::Fixed { name, .. }
            | lash_execution_contracts::process::linking::OperationNeed::Deferred {
                name, ..
            } => name,
        };
        for publication in &context.predecessor.publications {
            if publication.path == name.package && !environment.publications.contains(publication) {
                environment.publications.push(publication.clone());
            }
        }
        match binding {
            lash_core_store::tool_surface::UseBinding::Fixed(fixed) => {
                fixed.route.require(&mut environment.requirements)
            }
            // The ordinary link check derives unresolved resolver requirements.
            lash_core_store::tool_surface::UseBinding::Deferred(_) => {}
        }
        inspection.operations.insert(use_, need);
    }
    retain_code_publications(
        engine,
        packages,
        context,
        &mut environment,
        &code_artifacts,
        code,
    )
    .await?;
    environment.retained_code.extend(code.iter().copied());
    Ok((successor.clone().with_execution(environment), inspection))
}

/// Publication provenance is independent of operations. A pure callback can
/// originate in a public code member or in that member's immutable fill DAG.
async fn retain_code_publications(
    engine: &dyn ProcessEngine,
    packages: &dyn lash_durable::packages::PackageStore,
    context: &SuccessionContext,
    environment: &mut lash_core_store::tool_surface::ExecutionEnvironment,
    artifacts: &BTreeSet<lash_core_store::artifact_referrer::ArtifactName>,
    code: &BTreeSet<CallableId>,
) -> Result<(), EnginePortError> {
    use lash_core_store::artifact_referrer::{ArtifactName, ArtifactStoreId};
    use {
        lash_kernel_value::package::MemberKind, lash_kernel_value::package::Visibility,
        lash_vocabulary::package::PublishedContent,
    };
    if code.is_empty() {
        return Ok(());
    }
    let owed: BTreeSet<_> = code
        .iter()
        .map(|callable| ArtifactName {
            store: ArtifactStoreId::KernelDocument,
            artifact_ref: lash_durable::packages::link_reference(&callable.link),
        })
        .collect();
    let mut reaches = BTreeMap::new();
    for publication in &context.predecessor.publications {
        if environment.publications.contains(publication) {
            continue;
        }
        let PublishedContent::Package(id) = &publication.content else {
            continue;
        };
        let package = packages
            .package(id)
            .await
            .map_err(crate::plugin::packages::package_store_error)?
            .ok_or_else(|| {
                EnginePortError::PackageRefused(lash_core_store::packages::PackageRefusal::Missing(
                    lash_durable::packages::package_reference(id),
                ))
            })?;
        if package.id != *id
            || package.package.identity() != *id
            || package.package.path != publication.path
        {
            return Err(EnginePortError::PackageRefused(
                lash_core_store::packages::PackageRefusal::Publication,
            ));
        }
        let mut needed = false;
        for member in package.package.members.values() {
            let (Visibility::Public, MemberKind::Export { link, .. }) =
                (&member.visibility, &member.kind)
            else {
                continue;
            };
            let root = ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref: lash_durable::packages::link_reference(link),
            };
            if artifacts.contains(&root) {
                needed = true;
                break;
            }
            if !reaches.contains_key(link) {
                reaches.insert(
                    *link,
                    engine
                        .inspect_link(*link, &ReachSelector::AllExports)
                        .await?
                        .artifacts,
                );
            }
            if reaches[link].iter().any(|artifact| owed.contains(artifact)) {
                needed = true;
                break;
            }
        }
        if needed {
            environment.publications.push(publication.clone());
        }
    }
    Ok(())
}
