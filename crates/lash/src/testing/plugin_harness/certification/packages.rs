//! Package registration shared by the certification laws.
#![expect(
    clippy::unwrap_used,
    reason = "fixture package names are canonical literals"
)]
use lash_core::plugin::*;
use lash_execution_contracts::PluginError;
use std::sync::Arc;
mod api {
    pub(super) use lash_core::plugin::packages::*;
    pub(super) use lash_execution_contracts::process::linking::{
        AdmissionRefusal, EnginePortError,
    };
    pub(super) use lash_tool_support::ToolPackageProvider;
    pub(super) use lash_vocabulary::{claims::*, names::*, package::*};
}
pub(super) fn resolver_callback() -> PluginCallbackIdentity {
    PluginCallbackIdentity {
        owner: "resolver".into(),
        key: format!(
            "{}:deferred",
            CallbackSlot::DeferredPackageProvider.key_prefix()
        ),
    }
}

struct DeferredPackages;

#[async_trait::async_trait]
impl api::PackageProvider for DeferredPackages {
    fn roots(&self, _: &lash_core::OwnerConfigView) -> std::collections::BTreeSet<api::Segment> {
        [api::Segment::new("tools").unwrap()].into()
    }
    fn advertise(
        &self,
        _: &lash_core::OwnerConfigView,
        context: &api::PublicationContext,
    ) -> Result<Vec<api::PackageContribution>, PluginError> {
        use api::*;
        let package = Box::new(DeferredPackage {
            path: PackagePath::parse("tools").unwrap(),
            visibility: Visibility::Public,
            provider: ProviderName {
                owner: "resolver".into(),
                key: "deferred".into(),
            },
            config: ProviderConfig {
                component: None,
                format: FormatVersion::ONE,
                value: serde_json::Value::Null,
                payload: None,
            },
            resolution_policy: Default::default(),
        });
        Ok(vec![PackageContribution {
            publication: PublicationRecord {
                context: context.clone(),
                path: package.path.clone(),
                content: PublishedContent::Deferred(package.clone()),
                code_links: Default::default(),
                imported_publications: vec![],
            },
            advertisement: PackageAdvertisement::Deferred(package),
            routes: vec![],
        }])
    }
    async fn hold(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &[&api::PackageAdvertisement],
    ) -> Result<(), api::EnginePortError> {
        Ok(())
    }
}

pub(super) fn deferred_host(host: Arc<dyn lash_core::ToolProvider>) -> PluginSpec {
    PluginSpec::new()
        .with_tool_provider(lash_core::tool_provider_key!("tools"), host.clone())
        .with_package_provider(
            lash_core::tool_provider_key!("local"),
            Arc::new(api::ToolPackageProvider::new(
                "host",
                lash_core::tool_provider_key!("tools"),
                [api::Segment::new("local").unwrap()].into(),
                host,
            )),
        )
        .with_package_provider(
            lash_core::tool_provider_key!("packages"),
            Arc::new(DeferredPackages),
        )
}

pub(super) fn validate_resolver_grant(
    grant: &api::DeferredGrant,
    registry: RegistryKind,
) -> Result<(), api::AdmissionRefusal> {
    use api::*;
    if registry == RegistryKind::Session
        && grant.provider
            == (ProviderName {
                owner: "resolver".into(),
                key: "deferred".into(),
            })
        && grant.path == PackagePath::parse("tools").unwrap()
        && grant.visibility == Visibility::Public
        && grant.context.publisher == PublisherInstance("host/packages".into())
        && matches!(grant.context.scope, PublicationScope::Session(_))
        && grant.config
            == (ProviderConfig {
                component: None,
                format: FormatVersion::ONE,
                value: serde_json::Value::Null,
                payload: None,
            })
    {
        Ok(())
    } else {
        Err(AdmissionRefusal::ProviderNotHostable {
            provider: grant.provider.clone(),
            registry,
        })
    }
}
