//! Counting adapters on the typed recipe's executable binders.
use crate::ToolCall;
use crate::ToolOutcome;
use crate::ToolPrepareCall;
use crate::ToolProvider;
use crate::plugin::PluginContributions;
use crate::plugin::*;
use lash_execution_contracts::PluginError;
use lash_protocol_types::sync::MutexExt;
use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::tool_contract::ToolId;
use lash_vocabulary::tool_contract::ToolManifest;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Counts callback entries, not factory construction or metadata inspection.
#[derive(Clone, Default)]
pub struct CallbackCounts(Arc<Counters>);
#[derive(Default)]
struct Counters {
    entries: Mutex<Entries>,
    lifecycle_completed: tokio::sync::Notify,
}
#[derive(Default)]
struct LifecycleCompletion {
    finalized: bool,
    error: Option<String>,
}
#[derive(Default)]
struct Entries {
    callbacks: BTreeMap<PluginCallbackIdentity, u64>,
    resolutions: BTreeMap<PluginCallbackIdentity, u64>,
    providers: BTreeMap<(PluginCallbackIdentity, String), u64>,
    lifecycle: BTreeMap<PluginCallbackIdentity, LifecycleCompletion>,
}
impl CallbackCounts {
    pub fn snapshot(&self) -> BTreeMap<PluginCallbackIdentity, u64> {
        self.0.entries.lock_recover().callbacks.clone()
    }
    /// Every method entered through a counted tool or package provider.
    pub fn provider_snapshot(&self) -> BTreeMap<(PluginCallbackIdentity, String), u64> {
        self.0.entries.lock_recover().providers.clone()
    }
    fn provider_entered(&self, callback: &PluginCallbackIdentity, method: &str) {
        *self
            .0
            .entries
            .lock_recover()
            .providers
            .entry((callback.clone(), method.to_owned()))
            .or_default() += 1;
    }
    /// Share the counted runner with a bound package provider as well as its registration.
    pub fn tool_provider(
        &self,
        callback: PluginCallbackIdentity,
        inner: Arc<dyn ToolProvider>,
    ) -> Arc<dyn ToolProvider> {
        Arc::new(CountedProvider {
            inner,
            callback,
            counts: self.clone(),
        })
    }
    /// Observe package assembly without changing its implementation or defaults.
    pub fn package_provider(
        &self,
        callback: PluginCallbackIdentity,
        inner: Arc<dyn packages::PackageProvider>,
    ) -> Arc<dyn packages::PackageProvider> {
        Arc::new(CountedPackageProvider {
            inner,
            callback,
            counts: self.clone(),
        })
    }
    /// Resolution entries, separate from prepare/execute on the selected provider.
    /// A registered resolver has a zero entry even before it is exercised.
    pub fn resolution_snapshot(&self) -> BTreeMap<PluginCallbackIdentity, u64> {
        self.0.entries.lock_recover().resolutions.clone()
    }
    /// Wait for this observer to complete a finalized-turn notification, or
    /// return its first lifecycle failure. Entry alone is not completion;
    /// advisory observers may still be running after the caller's answer.
    pub async fn lifecycle_completed(
        &self,
        callback: &PluginCallbackIdentity,
    ) -> Result<(), String> {
        loop {
            let notified = self.0.lifecycle_completed.notified();
            if let Some(completion) = self.0.entries.lock_recover().lifecycle.get(callback) {
                if let Some(error) = &completion.error {
                    return Err(error.clone());
                }
                if completion.finalized {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
    fn entered(&self, callback: &PluginCallbackIdentity) {
        *self
            .0
            .entries
            .lock_recover()
            .callbacks
            .entry(callback.clone())
            .or_default() += 1;
    }
    pub(crate) fn wrap(&self, contributions: &mut PluginContributions) {
        macro_rules! hooks {
            ($($field:ident),+ $(,)?) => {$(
                for registered in &mut contributions.$field {
                    let callback = registered.identity.clone();
                    let inner = Arc::clone(&registered.hook);
                    let counts = self.clone();
                    registered.hook = Arc::new(move |input| {
                        counts.entered(&callback);
                        inner(input)
                    });
                }
            )+};
        }
        hooks!(
            before_turn_hooks,
            after_turn_hooks,
            checkpoint_hooks,
            tool_args_transforms,
            tool_args_checks,
            tool_result_transforms,
            tool_result_checks,
            tool_catalog_contributors,
            assistant_stream_hooks,
            assistant_response_hooks,
            assistant_stream_finished_hooks,
            presentation_steps,
            cell_failure_feedback
        );
        for registered in &mut contributions.runtime_event_hooks {
            let callback = registered.identity.clone();
            let inner = Arc::clone(&registered.hook);
            let counts = self.clone();
            registered.hook = Arc::new(move |event| {
                counts.entered(&callback);
                let finalized = matches!(event, PluginLifecycleEvent::TurnFinalized(_));
                let future = inner(event);
                let callback = callback.clone();
                let counts = counts.clone();
                Box::pin(async move {
                    let result = future.await;
                    {
                        let mut entries = counts.0.entries.lock_recover();
                        let completion = entries.lifecycle.entry(callback).or_default();
                        completion.finalized |= finalized;
                        if let Err(error) = &result {
                            completion.error.get_or_insert_with(|| error.to_string());
                        }
                    }
                    counts.0.lifecycle_completed.notify_waiters();
                    result
                })
            });
        }
        if let Some(registered) = &mut contributions.presentation_presenter {
            let inner = Arc::clone(&registered.hook);
            let counts = self.clone();
            let callback = registered.identity.clone();
            registered.hook = Arc::new(move |input| {
                counts.entered(&callback);
                inner(input)
            });
        }
        for registered in &mut contributions.tool_providers {
            registered.hook = Arc::new(CountedProvider {
                inner: Arc::clone(&registered.hook),
                callback: registered.identity.callback_identity(),
                counts: self.clone(),
            });
        }
        for registered in &mut contributions.deferred_package_providers {
            let callback = PluginCallbackIdentity {
                owner: registered.identity.owner().to_owned(),
                key: format!(
                    "{}:{}",
                    CallbackSlot::DeferredPackageProvider.key_prefix(),
                    registered.identity.key()
                ),
            };
            self.0
                .entries
                .lock_recover()
                .resolutions
                .entry(callback.clone())
                .or_default();
            registered.hook = Arc::new(CountedPackageResolver {
                inner: Arc::clone(&registered.hook),
                callback,
                counts: self.clone(),
            });
        }
        if let Some(registered) = &mut contributions.deferred_provider {
            let callback = registered.identity.callback_identity();
            self.0
                .entries
                .lock_recover()
                .resolutions
                .entry(callback.clone())
                .or_default();
            registered.hook = Arc::new(CountedProvider {
                inner: Arc::clone(&registered.hook),
                callback,
                counts: self.clone(),
            });
        }
        for (_, registered) in &mut contributions.context_pressure_hooks {
            registered.hook = Arc::new(CountedPressure {
                inner: Arc::clone(&registered.hook),
                callback: registered.identity.clone(),
                counts: self.clone(),
            });
        }
        for (_, registered) in &mut contributions.context_compactors {
            registered.hook = Arc::new(CountedCompactor {
                inner: Arc::clone(&registered.hook),
                callback: registered.identity.clone(),
                counts: self.clone(),
            });
        }
        for (_, registered) in &mut contributions.attachment_omission_policies {
            registered.hook = Arc::new(CountedOmissions {
                inner: Arc::clone(&registered.hook),
                callback: registered.identity.clone(),
                counts: self.clone(),
            });
        }
    }
}
struct CountedProvider<T: ToolProvider + ?Sized> {
    inner: Arc<T>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
#[async_trait::async_trait]
impl<T: ToolProvider + ?Sized> ToolProvider for CountedProvider<T> {
    fn refresh_environment(&self) {
        self.counts
            .provider_entered(&self.callback, "refresh_environment");
        self.inner.refresh_environment();
    }
    fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        self.counts
            .provider_entered(&self.callback, "environment_mode");
        self.inner.environment_mode()
    }
    fn environment_config_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.counts
            .provider_entered(&self.callback, "environment_config_dependencies");
        self.inner.environment_config_dependencies()
    }
    fn environment_namespace_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.counts
            .provider_entered(&self.callback, "environment_namespace_dependencies");
        self.inner.environment_namespace_dependencies()
    }
    fn subscribe_environment(
        &self,
        source: &lash_core_store::environment::SourceId,
        sink: &crate::plugin::environment_sources::EnvironmentChangeSink,
    ) {
        self.counts
            .provider_entered(&self.callback, "subscribe_environment");
        self.inner.subscribe_environment(source, sink);
    }
    fn environment_snapshot(
        &self,
        key: &crate::ProviderKey,
        source: &lash_core_store::environment::SourceId,
        revision: lash_core_store::environment::SourceRevision,
        config: &OwnerConfigView,
        namespace: Option<(
            &lash_kernel_value::package::PublicationDomain,
            &lash_kernel_value::package::PublicationScope,
        )>,
    ) -> Result<lash_core_store::environment::SourceSnapshot, PluginError> {
        self.counts
            .provider_entered(&self.callback, "environment_snapshot");
        self.inner
            .environment_snapshot(key, source, revision, config, namespace)
    }
    fn tool_manifests(&self, config: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        self.counts
            .provider_entered(&self.callback, "tool_manifests");
        self.inner.tool_manifests(config)
    }
    fn resolve_manifest(&self, name: &str, config: &OwnerConfigView) -> Option<ToolManifest> {
        self.counts
            .provider_entered(&self.callback, "resolve_manifest");
        self.inner.resolve_manifest(name, config)
    }
    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.counts
            .provider_entered(&self.callback, "resolve_manifest_by_id");
        self.inner.resolve_manifest_by_id(id, config)
    }
    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.counts
            .provider_entered(&self.callback, "resolve_contract");
        self.inner.resolve_contract(name, config)
    }
    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.counts
            .provider_entered(&self.callback, "resolve_contract_by_id");
        self.inner.resolve_contract_by_id(id, config)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, ToolOutcome> {
        self.counts
            .provider_entered(&self.callback, "prepare_tool_call");
        self.counts.entered(&self.callback);
        self.inner.prepare_tool_call(call).await
    }
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.counts.provider_entered(&self.callback, "execute");
        self.counts.entered(&self.callback);
        self.inner.execute(call).await
    }
    fn readable_binding_formats(&self) -> &'static [lash_kernel_value::providers::FormatVersion] {
        self.counts
            .provider_entered(&self.callback, "readable_binding_formats");
        self.inner.readable_binding_formats()
    }
    fn link_payload(
        &self,
        manifest: &ToolManifest,
        config: &OwnerConfigView,
    ) -> Option<(
        lash_kernel_value::providers::FormatVersion,
        serde_json::Value,
    )> {
        self.counts.provider_entered(&self.callback, "link_payload");
        self.inner.link_payload(manifest, config)
    }
    fn isolated_start(
        &self,
        call: crate::IsolatedProcessRequest<'_>,
    ) -> Result<crate::IsolatedStart, crate::ProcessEnvRefusal> {
        self.counts
            .provider_entered(&self.callback, "isolated_start");
        self.counts.entered(&self.callback);
        self.inner.isolated_start(call)
    }
}

#[async_trait::async_trait]
impl crate::DeferredToolProvider for CountedProvider<dyn crate::DeferredToolProvider> {
    fn searchable_names(&self, cx: &crate::DeferredResolveContext<'_>) -> BTreeMap<String, String> {
        self.inner.searchable_names(cx)
    }
    async fn resolve(
        &self,
        cx: &crate::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, crate::DeferredOutcome> {
        *self
            .counts
            .0
            .entries
            .lock_recover()
            .resolutions
            .entry(self.callback.clone())
            .or_default() += 1;
        self.inner.resolve(cx, paths).await
    }
}
struct CountedPackageResolver {
    inner: Arc<dyn crate::plugin::packages::DeferredPackageProvider>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
#[async_trait::async_trait]
impl crate::plugin::packages::DeferredPackageProvider for CountedPackageResolver {
    fn validate_grant(
        &self,
        grant: &lash_kernel_value::package::DeferredGrant,
        registry: RegistryKind,
    ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
        self.inner.validate_grant(grant, registry)
    }
    async fn resolve(
        &self,
        cx: &crate::DeferredResolveContext<'_>,
        grant: &lash_kernel_value::package::DeferredGrant,
        requested: &lash_kernel_value::names::QualifiedName,
    ) -> Result<crate::plugin::packages::DeferredAnswer, crate::plugin::packages::ResolverFault>
    {
        *self
            .counts
            .0
            .entries
            .lock_recover()
            .resolutions
            .entry(self.callback.clone())
            .or_default() += 1;
        self.inner.resolve(cx, grant, requested).await
    }
}
struct CountedPackageProvider {
    inner: Arc<dyn packages::PackageProvider>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
#[async_trait::async_trait]
impl packages::PackageProvider for CountedPackageProvider {
    fn environment_tool_sources(
        &self,
        publisher: &ToolProviderHandle,
    ) -> std::collections::BTreeSet<ToolProviderHandle> {
        self.counts
            .provider_entered(&self.callback, "environment_tool_sources");
        self.inner.environment_tool_sources(publisher)
    }
    fn shares_tool_source(&self) -> bool {
        self.counts
            .provider_entered(&self.callback, "shares_tool_source");
        self.inner.shares_tool_source()
    }
    fn refresh_environment(&self) {
        self.counts
            .provider_entered(&self.callback, "refresh_environment");
        self.inner.refresh_environment()
    }
    fn environment_snapshot(
        &self,
        config: &OwnerConfigView,
        domain: &lash_kernel_value::package::PublicationDomain,
        scope: &lash_kernel_value::package::PublicationScope,
        key: &ToolProviderHandle,
        observation: (
            lash_core_store::environment::SourceId,
            lash_core_store::environment::SourceRevision,
        ),
    ) -> Result<lash_core_store::environment::SourceSnapshot, PluginError> {
        self.counts
            .provider_entered(&self.callback, "environment_snapshot");
        self.inner
            .environment_snapshot(config, domain, scope, key, observation)
    }
    fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        self.counts
            .provider_entered(&self.callback, "environment_mode");
        self.inner.environment_mode()
    }
    fn environment_config_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.counts
            .provider_entered(&self.callback, "environment_config_dependencies");
        self.inner.environment_config_dependencies()
    }
    fn environment_namespace_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.counts
            .provider_entered(&self.callback, "environment_namespace_dependencies");
        self.inner.environment_namespace_dependencies()
    }
    fn subscribe_environment(
        &self,
        source: &lash_core_store::environment::SourceId,
        sink: &crate::plugin::environment_sources::EnvironmentChangeSink,
    ) {
        self.counts
            .provider_entered(&self.callback, "subscribe_environment");
        self.inner.subscribe_environment(source, sink)
    }
    fn environment_fragment(
        &self,
        config: &OwnerConfigView,
        domain: &lash_kernel_value::package::PublicationDomain,
        scope: &lash_kernel_value::package::PublicationScope,
        key: &ToolProviderHandle,
        tools: Option<&[crate::SurfaceEntry]>,
    ) -> Result<lash_core_store::environment::EnvironmentFragment, PluginError> {
        self.counts
            .provider_entered(&self.callback, "environment_fragment");
        self.inner
            .environment_fragment(config, domain, scope, key, tools)
    }
    fn roots(
        &self,
        config: &OwnerConfigView,
    ) -> std::collections::BTreeSet<lash_kernel_value::names::Segment> {
        self.counts.provider_entered(&self.callback, "roots");
        self.inner.roots(config)
    }
    fn advertise(
        &self,
        config: &OwnerConfigView,
        context: &lash_kernel_value::package::PublicationContext,
    ) -> Result<Vec<packages::PackageContribution>, PluginError> {
        self.counts.provider_entered(&self.callback, "advertise");
        self.inner.advertise(config, context)
    }
    async fn hold(
        &self,
        claim: &lash_core_store::artifact_referrer::ReferrerClaim,
        advertisements: &[&lash_kernel_value::package::PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        self.counts.provider_entered(&self.callback, "hold");
        self.inner.hold(claim, advertisements).await
    }
}

struct CountedPressure {
    inner: Arc<dyn ContextPressureHook>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
#[async_trait::async_trait]
impl ContextPressureHook for CountedPressure {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    async fn decide(
        &self,
        ctx: &ContextPressureContext<'_>,
    ) -> Result<ContextPressureDecision, ContextError> {
        self.counts.entered(&self.callback);
        self.inner.decide(ctx).await
    }
}
struct CountedCompactor {
    inner: Arc<dyn ContextCompactor>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
#[async_trait::async_trait]
impl ContextCompactor for CountedCompactor {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    async fn compact(
        &self,
        ctx: &CompactionContext<'_>,
    ) -> Result<Option<ContextCompaction>, ContextError> {
        self.counts.entered(&self.callback);
        self.inner.compact(ctx).await
    }
}
struct CountedOmissions {
    inner: Arc<dyn AttachmentOmissionPolicy>,
    callback: PluginCallbackIdentity,
    counts: CallbackCounts,
}
impl AttachmentOmissionPolicy for CountedOmissions {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn omissions(
        &self,
        ctx: &AttachmentOmissionContext,
        history: &[crate::Message],
    ) -> Result<crate::plugin::AttachmentOmissions, PluginError> {
        self.counts.entered(&self.callback);
        self.inner.omissions(ctx, history)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    /// COUNTING-PRESERVES-PROVIDER-BEHAVIOR: a default added to the trait
    /// must not silently replace the wrapped provider's implementation.
    #[test]
    fn counted_provider_overrides_every_tool_provider_method() {
        for (source, trait_name) in [
            (include_str!("../tool_provider.rs"), "ToolProvider"),
            (include_str!("../plugin/packages.rs"), "PackageProvider"),
        ] {
            let provider = syn::parse_file(source).unwrap();
            let methods: BTreeSet<_> = provider
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Trait(item) if item.ident == trait_name => Some(&item.items),
                    _ => None,
                })
                .unwrap()
                .iter()
                .filter_map(|item| match item {
                    syn::TraitItem::Fn(method) => Some(method.sig.ident.to_string()),
                    _ => None,
                })
                .collect();
            let wrappers = syn::parse_file(include_str!("callback_counts.rs")).unwrap();
            let forwarded: BTreeSet<_> = wrappers
                .items
                .iter()
                .find_map(|item| match item {
                    syn::Item::Impl(item)
                        if item.trait_.as_ref().is_some_and(|(_, path, _)| {
                            path.segments
                                .last()
                                .is_some_and(|segment| segment.ident == trait_name)
                        }) =>
                    {
                        Some(&item.items)
                    }
                    _ => None,
                })
                .unwrap()
                .iter()
                .filter_map(|item| match item {
                    syn::ImplItem::Fn(method) => Some(method.sig.ident.to_string()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                forwarded, methods,
                "forward every method, including defaults"
            );
        }
    }
}
