//! Select serving routes for the exact reached use/contract pair.
use crate::plugin::packages::AdvertisedRoute;
use lash_core_store::tool_surface::{ExecutionEnvironment, RoutedOperation, UseBinding};
use lash_execution_contracts::process::linking::{
    AdmissionRefusal, EnginePortError, LinkInspection, OperationNeed,
};
use lash_vocabulary::{claims::*, names::*, package::*};

/// Candidate order has no authority. Distinct serving candidates for one pin
/// are ambiguous; equal advertisements are harmless duplicates.
pub fn select_routes(
    inspection: &LinkInspection,
    registry: RegistryKind,
    candidates: &[AdvertisedRoute],
    publications: Vec<lash_vocabulary::package::PublicationRecord>,
    mut deferred: impl FnMut(&DeferredGrant, RegistryKind) -> Result<ClaimRequirements, EnginePortError>,
) -> Result<ExecutionEnvironment, EnginePortError> {
    let mut environment = ExecutionEnvironment::empty(registry);
    environment.publications = publications;
    environment.roots.insert(inspection.link);
    environment.requirements.machine = inspection.machine.clone();
    for (use_, need) in &inspection.operations {
        let binding = match need {
            OperationNeed::Deferred { grant, .. } => {
                environment.requirements.extend(&deferred(grant, registry)?);
                UseBinding::Deferred(grant.as_ref().clone())
            }
            OperationNeed::Fixed { name, requirement } => {
                let mut matching = candidates.iter().filter(|candidate| {
                    name.package == candidate.operation.package
                        && name.member == candidate.operation.name
                        && name.type_member.is_none()
                        && candidate.definition.contract_id() == requirement.contract
                        && candidate.route.provider.registry() == registry
                });
                let selected = matching.next().ok_or(EnginePortError::Refused(
                    AdmissionRefusal::MissingRoute { use_: *use_ },
                ))?;
                if matching.any(|candidate| candidate != selected) {
                    return Err(EnginePortError::Refused(
                        AdmissionRefusal::InvalidDocument {
                            message: format!("ambiguous serving routes for {use_:?} ({name})"),
                        },
                    ));
                }
                let declaration = selected.definition.manifest.declaration();
                let operation = RoutedOperation {
                    requirement: requirement.clone(),
                    offer: OperationOffer {
                        requirement: lash_vocabulary::operations::OperationRequirement {
                            signature: lash_vocabulary::json_schema::tool_signature(
                                &selected.definition.contract.input_schema.canonical,
                                &selected.definition.contract.output_schema.canonical,
                            ),
                            contract: selected.definition.contract_id(),
                        },
                        intents: declaration
                            .intents
                            .iter()
                            .map(|intent| Intent(intent.as_str().into()))
                            .collect(),
                        controls: declaration.controls.clone(),
                        mode: if selected.route.policy == lash_vocabulary::ExecutionPolicy::Read {
                            OperationMode::Read
                        } else {
                            OperationMode::Waited
                        },
                    },
                    route: selected.route.clone(),
                };
                operation.validate().map_err(|message| {
                    EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message })
                })?;
                operation.route.require(&mut environment.requirements);
                UseBinding::Fixed(operation)
            }
        };
        environment.uses.insert(*use_, binding);
    }
    Ok(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L-DISPATCH: one diagnostic name can require two independent contracts.
    #[test]
    fn same_name_uses_select_their_own_contract_and_refuse_ambiguous_candidates() {
        let make = |id: &str, output| {
            let definition = lash_vocabulary::ToolDefinition::raw(
                id,
                "dataframe.explain",
                "",
                serde_json::json!({"type":"object"}),
                output,
            )
            .unwrap();
            let provider = lash_vocabulary::providers::ToolProviderHandle::from_name(
                RegistryKind::Process,
                &ProviderName {
                    owner: "fixture".into(),
                    key: id.into(),
                },
            )
            .unwrap();
            AdvertisedRoute {
                operation: definition.manifest.operation.clone(),
                route: lash_core_store::tool_surface::OperationRoute::admitted(
                    provider,
                    &definition,
                    None,
                ),
                definition,
            }
        };
        let one = make("one", serde_json::json!({"type":"string"}));
        let two = make("two", serde_json::json!({"type":"integer"}));
        let use_one = OperationUse {
            link: LinkId::from_bytes([1; 32]),
            slot: ImportSlot(4),
        };
        let use_two = OperationUse {
            link: LinkId::from_bytes([2; 32]),
            slot: ImportSlot(9),
        };
        let need = |candidate: &AdvertisedRoute| OperationNeed::Fixed {
            name: QualifiedName {
                package: candidate.operation.package.clone(),
                member: candidate.operation.name.clone(),
                type_member: None,
            },
            requirement: lash_vocabulary::operations::OperationRequirement {
                signature: lash_vocabulary::json_schema::tool_signature(
                    &candidate.definition.contract.input_schema.canonical,
                    &candidate.definition.contract.output_schema.canonical,
                ),
                contract: candidate.definition.contract_id(),
            },
        };
        let inspection = LinkInspection {
            link: LinkId::from_bytes([3; 32]),
            document: DocumentId::from_bytes([4; 32]),
            selector: lash_execution_contracts::process::linking::ReachSelector::Start(
                StartSelector::Main,
            ),
            operations: [(use_one, need(&one)), (use_two, need(&two))].into(),
            machine: NodeRequirements::default(),
            artifacts: Vec::new(),
            signature: None,
        };
        let selected = select_routes(
            &inspection,
            RegistryKind::Process,
            &[two.clone(), one.clone()],
            Vec::new(),
            |_, _| unreachable!(),
        )
        .unwrap();
        assert_eq!(
            selected.fixed(&use_one).unwrap().route.tool,
            one.definition.manifest.id
        );
        assert_eq!(
            selected.fixed(&use_two).unwrap().route.tool,
            two.definition.manifest.id
        );
        let mut duplicate = one.clone();
        duplicate.route.provider = two.route.provider.clone();
        assert!(
            select_routes(
                &inspection,
                RegistryKind::Process,
                &[one, two, duplicate],
                Vec::new(),
                |_, _| unreachable!()
            )
            .is_err()
        );
    }
}
