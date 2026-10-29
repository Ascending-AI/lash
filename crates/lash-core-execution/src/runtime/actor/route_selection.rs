//! Select serving routes for the exact reached use/contract pair.
use crate::plugin::packages::AdvertisedRoute;
use lash_core_store::tool_surface::{ExecutionEnvironment, RoutedOperation, UseBinding};
use lash_execution_contracts::process::linking::{
    AdmissionRefusal, EnginePortError, LinkInspection, OperationNeed,
};
use lash_kernel_value::names::OperationUse;
use {lash_kernel_value::package::*, lash_vocabulary::claims::*};

/// Availability checks supplied by the registered execution composition.
trait RouteEligibility {
    fn hooks(&self) -> Vec<lash_vocabulary::providers::PluginCallbackIdentity>;
    fn deferred(
        &self,
        grant: &DeferredGrant,
        registry: RegistryKind,
    ) -> Result<ClaimRequirements, EnginePortError>;
    fn fixed(
        &self,
        route: &AdvertisedRoute,
        mode: OperationMode,
    ) -> Result<ClaimRequirements, EnginePortError>;
}

/// Candidate order has no authority. Distinct serving candidates for one pin
/// are ambiguous; equal advertisements are harmless duplicates.
pub fn select_routes(
    inspection: &LinkInspection,
    registry: RegistryKind,
    candidates: &[AdvertisedRoute],
    selected_routes: &std::collections::BTreeMap<
        OperationUse,
        (
            lash_vocabulary::providers::ToolProviderHandle,
            lash_vocabulary::ToolId,
        ),
    >,
    publications: Vec<lash_vocabulary::package::PublicationRecord>,
    composition: &crate::plugin::ExecutionComposition,
) -> Result<ExecutionEnvironment, EnginePortError> {
    select_with(
        inspection,
        registry,
        candidates,
        selected_routes,
        publications,
        &CompositionEligibility(composition),
    )
}

fn select_with(
    inspection: &LinkInspection,
    registry: RegistryKind,
    candidates: &[AdvertisedRoute],
    selected_routes: &std::collections::BTreeMap<
        OperationUse,
        (
            lash_vocabulary::providers::ToolProviderHandle,
            lash_vocabulary::ToolId,
        ),
    >,
    publications: Vec<lash_vocabulary::package::PublicationRecord>,
    eligibility: &dyn RouteEligibility,
) -> Result<ExecutionEnvironment, EnginePortError> {
    let mut environment = ExecutionEnvironment::empty(registry);
    environment.publications = publications;
    environment.roots.insert(inspection.link);
    environment.requirements.machine = inspection.machine.clone();
    let hooks = eligibility.hooks();
    for (use_, need) in &inspection.operations {
        let binding = match need {
            OperationNeed::Deferred { grant, .. } => {
                // Per-use registration owns discovery debt until its winner
                // commits. Static environment debt must not retain a resolver.
                eligibility.deferred(grant, registry)?;
                UseBinding::Deferred(grant.as_ref().clone())
            }
            OperationNeed::Fixed { name, requirement } => {
                let mut matching = candidates.iter().filter(|candidate| {
                    name.package == candidate.operation.package
                        && name.member == candidate.operation.name
                        && name.type_member.is_none()
                        && candidate.definition.contract_id() == requirement.contract
                        && candidate.route.provider.registry() == registry
                        && selected_routes.get(use_).is_none_or(|(provider, tool)| {
                            *provider == candidate.route.provider && *tool == candidate.route.tool
                        })
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
                        requirement: lash_kernel_value::operations::OperationRequirement {
                            signature: lash_kernel_value::json_schema::tool_signature(
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
                        mode: if selected.route.policy == lash_kernel_value::ExecutionPolicy::Read {
                            OperationMode::Read
                        } else {
                            OperationMode::Waited
                        },
                    },
                    mode: if selected.route.policy == lash_kernel_value::ExecutionPolicy::Read
                        && hooks.is_empty()
                    {
                        OperationMode::Read
                    } else {
                        OperationMode::Waited
                    },
                    route: selected.route.clone(),
                };
                operation.validate().map_err(|message| {
                    EnginePortError::Refused(AdmissionRefusal::InvalidDocument { message })
                })?;
                environment
                    .requirements
                    .extend(&eligibility.fixed(selected, operation.mode)?);
                if operation.mode == OperationMode::Waited {
                    environment.requirements.callbacks.extend(
                        hooks
                            .iter()
                            .cloned()
                            .map(|callback| CallbackRequirement { registry, callback }),
                    );
                }
                UseBinding::Fixed(operation)
            }
        };
        environment.uses.insert(*use_, binding);
    }
    Ok(environment)
}

struct CompositionEligibility<'a>(&'a crate::plugin::ExecutionComposition);

impl RouteEligibility for CompositionEligibility<'_> {
    fn hooks(&self) -> Vec<lash_vocabulary::providers::PluginCallbackIdentity> {
        self.0.tool_call_hooks()
    }

    fn deferred(
        &self,
        grant: &DeferredGrant,
        registry: RegistryKind,
    ) -> Result<lash_vocabulary::claims::ClaimRequirements, EnginePortError> {
        self.0.validate_deferred_grant(grant, registry)
    }

    fn fixed(
        &self,
        candidate: &AdvertisedRoute,
        mode: OperationMode,
    ) -> Result<lash_vocabulary::claims::ClaimRequirements, EnginePortError> {
        use lash_vocabulary::claims::{CallbackRequirement, ClaimRequirements};
        let route = &candidate.route;
        let registry = route.provider.registry();
        if !self.0.tool_registry().serves(&route.provider) {
            return Err(EnginePortError::Refused(
                AdmissionRefusal::ProviderNotHostable {
                    provider: ProviderName {
                        owner: route.provider.owner().into(),
                        key: route.provider.key().into(),
                    },
                    registry,
                },
            ));
        }
        if mode == OperationMode::Read && self.0.tool_registry().reads(&route.provider).is_none() {
            return Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument {
                    message: format!("provider `{}` registers no reader", route.provider),
                },
            ));
        }
        let mut requirements = ClaimRequirements::default();
        requirements.providers.insert(route.provider.clone());
        requirements.callbacks.insert(CallbackRequirement {
            registry,
            callback: if mode == OperationMode::Read {
                route.provider.reads_identity()
            } else {
                route.provider.callback_identity()
            },
        });
        if let Some(payload) = &route.payload {
            if payload.codec.registry() != registry
                || !self.0.tool_registry().serves(&payload.codec)
            {
                return Err(EnginePortError::Refused(
                    AdmissionRefusal::InvalidDocument {
                        message: format!("route codec `{}` is unavailable", payload.codec),
                    },
                ));
            }
            requirements
                .codecs
                .insert((payload.codec.clone(), payload.format));
            requirements.callbacks.insert(CallbackRequirement {
                registry,
                callback: payload.codec.callback_identity(),
            });
        }
        Ok(requirements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_value::claims::NodeRequirements;
    use lash_kernel_value::names::*;

    struct Available;
    impl RouteEligibility for Available {
        fn hooks(&self) -> Vec<lash_vocabulary::providers::PluginCallbackIdentity> {
            Vec::new()
        }
        fn deferred(
            &self,
            _: &DeferredGrant,
            _: RegistryKind,
        ) -> Result<ClaimRequirements, EnginePortError> {
            unreachable!()
        }
        fn fixed(
            &self,
            route: &AdvertisedRoute,
            mode: OperationMode,
        ) -> Result<ClaimRequirements, EnginePortError> {
            let mut requirements = ClaimRequirements::default();
            let route = route.route.clone();
            route.require(&mut requirements);
            let _ = mode;
            Ok(requirements)
        }
    }

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
            .unwrap()
            .with_execution(std::time::Duration::from_secs(30));
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
                route: lash_core_store::tool_surface::OperationRoute {
                    provider,
                    tool: definition.manifest.id.clone(),
                    policy: definition.manifest.execution_policy,
                    projection: definition.manifest.argument_projection.clone(),
                    payload: None,
                    source: lash_core_store::tool_surface::RouteSource::Advertised,
                },
                definition,
            }
        };
        let one = make("one", serde_json::json!({"type":"string"}));
        let two = make("two", serde_json::json!({"type":"integer"}));
        let use_one = OperationUse {
            link: LinkId::from_bytes([1; 32]),
            slot: (ImportSlot(4)).into(),
        };
        let use_two = OperationUse {
            link: LinkId::from_bytes([2; 32]),
            slot: (ImportSlot(9)).into(),
        };
        let need = |candidate: &AdvertisedRoute| OperationNeed::Fixed {
            name: QualifiedName {
                package: candidate.operation.package.clone(),
                member: candidate.operation.name.clone(),
                type_member: None,
            },
            requirement: lash_kernel_value::operations::OperationRequirement {
                signature: lash_kernel_value::json_schema::tool_signature(
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
        let selected = select_with(
            &inspection,
            RegistryKind::Process,
            &[two.clone(), one.clone()],
            &Default::default(),
            Vec::new(),
            &Available,
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
            select_with(
                &inspection,
                RegistryKind::Process,
                &[one, two, duplicate],
                &Default::default(),
                Vec::new(),
                &Available
            )
            .is_err()
        );
    }
}
