//! Explicit package publication for durable-law host fixtures.
#![allow(dead_code)]
use lash_core::plugin::packages::{PackageContribution, PackageProvider, operation_publication};
use lash_execution_contracts::process::linking::{AdmissionRefusal, EnginePortError};
use lash_vocabulary::{names::*, package::*, providers::*};
use std::sync::Arc;

pub fn tools(
    key: ToolProviderKey,
    provider: Arc<dyn lash_core::ToolProvider>,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(key.as_str()),
        lash_core::testing::tool_spec(key.as_str(), key, provider),
    ))
}

/// Project exactly this owner's runner into its explicitly owned namespace.
pub fn operation_provider(
    owner: &str,
    key: ToolProviderKey,
    provider: Arc<dyn lash_core::ToolProvider>,
) -> Arc<dyn PackageProvider> {
    let roots = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .expect("fixture manifests")
        .into_iter()
        .map(|manifest| manifest.operation.package.as_slice()[0].clone())
        .collect();
    Arc::new(lash::package::ToolPackageProvider::new(
        owner, key, roots, provider,
    ))
}

pub fn deferred_spec(
    owner: &str,
    key: ToolProviderKey,
    path: &str,
    resolver: Arc<dyn lash_core::plugin::packages::DeferredPackageProvider>,
) -> lash_core::plugin::PluginSpec {
    lash_core::plugin::PluginSpec::new()
        .with_package_provider(
            key,
            Arc::new(Deferred {
                owner: owner.to_owned(),
                key,
                path: PackagePath::parse(path).expect("fixture package path"),
            }),
        )
        .with_deferred_package_provider(key, resolver)
}
struct Deferred {
    owner: String,
    key: ToolProviderKey,
    path: PackagePath,
}
#[async_trait::async_trait]
impl PackageProvider for Deferred {
    fn roots(&self, _: &lash_core::OwnerConfigView) -> std::collections::BTreeSet<Segment> {
        [self.path.as_slice()[0].clone()].into()
    }
    fn advertise(
        &self,
        config: &lash_core::OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, lash_execution_contracts::PluginError> {
        let advertisement = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
            path: self.path.clone(),
            visibility: Visibility::Public,
            provider: ProviderName {
                owner: self.owner.clone(),
                key: self.key.as_str().to_owned(),
            },
            config: ProviderConfig {
                component: (config.registry_kind()
                    == lash_vocabulary::claims::RegistryKind::Process)
                    .then(|| ProcessComponentId::new(&self.owner).expect("fixture component")),
                format: FormatVersion::ONE,
                value: serde_json::Value::Null,
                payload: None,
            },
            resolution_policy: Default::default(),
        }));
        Ok(vec![PackageContribution {
            publication: operation_publication(context, &advertisement)?,
            advertisement,
            routes: Vec::new(),
        }])
    }
    async fn hold(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
        Ok(())
    }
}

pub fn validate_grant(
    owner: &str,
    key: &str,
    path: &str,
    grant: &DeferredGrant,
    registry: lash_vocabulary::claims::RegistryKind,
) -> Result<(), AdmissionRefusal> {
    if grant.provider.owner == owner
        && grant.provider.key == key
        && grant.path == PackagePath::parse(path).expect("fixture path")
        && grant.visibility == Visibility::Public
        && grant.config.format == FormatVersion::ONE
        && grant.config.value.is_null()
        && grant.config.payload.is_none()
        && grant
            .config
            .component
            .as_ref()
            .is_none_or(|component| component.as_str() == owner)
    {
        Ok(())
    } else {
        Err(AdmissionRefusal::ProviderNotHostable {
            provider: grant.provider.clone(),
            registry,
        })
    }
}

/// Capture linked operation uses through the production host policy, with
/// exactly the components this fixture explicitly configures.
pub fn run_policy_naming(
    keys: &[lash_core::ProcessConfigKey<()>],
) -> Arc<dyn lash_core::ProcessEnvPolicy> {
    let mut keys = keys.to_vec();
    if !keys
        .iter()
        .any(|key| key.component() == lash::vm::KERNEL_SERVICES_CONFIG.component())
    {
        keys.push(lash::vm::KERNEL_SERVICES_CONFIG);
    }
    lash::process::env::policy::restrict_or(lash::process::env::policy::from_run(move |_| {
        keys.iter()
            .map(|key| lash_core::ProcessComponentEntry::new(*key, &(), []))
            .collect()
    }))
}
