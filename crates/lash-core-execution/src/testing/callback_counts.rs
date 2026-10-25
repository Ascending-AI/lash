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
    lifecycle: BTreeMap<PluginCallbackIdentity, LifecycleCompletion>,
}
impl CallbackCounts {
    pub fn snapshot(&self) -> BTreeMap<PluginCallbackIdentity, u64> {
        self.0.entries.lock_recover().callbacks.clone()
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
        self.inner.refresh_environment();
    }
    fn environment_mode(&self) -> lash_core_store::environment::SourceMode {
        self.inner.environment_mode()
    }
    fn environment_config_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.inner.environment_config_dependencies()
    }
    fn environment_namespace_dependencies(&self) -> std::collections::BTreeSet<String> {
        self.inner.environment_namespace_dependencies()
    }
    fn subscribe_environment(
        &self,
        source: &lash_core_store::environment::SourceId,
        sink: &crate::plugin::environment_sources::EnvironmentChangeSink,
    ) {
        self.inner.subscribe_environment(source, sink);
    }
    fn environment_snapshot(
        &self,
        key: &crate::ProviderKey,
        source: &lash_core_store::environment::SourceId,
        revision: lash_core_store::environment::SourceRevision,
        config: &OwnerConfigView,
        namespace: Option<(
            &lash_vocabulary::package::PublicationDomain,
            &lash_vocabulary::package::PublicationScope,
        )>,
    ) -> Result<lash_core_store::environment::SourceSnapshot, PluginError> {
        self.inner
            .environment_snapshot(key, source, revision, config, namespace)
    }
    fn tool_manifests(&self, config: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        self.inner.tool_manifests(config)
    }
    fn resolve_manifest(&self, name: &str, config: &OwnerConfigView) -> Option<ToolManifest> {
        self.inner.resolve_manifest(name, config)
    }
    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.inner.resolve_manifest_by_id(id, config)
    }
    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.inner.resolve_contract(name, config)
    }
    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        config: &OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.inner.resolve_contract_by_id(id, config)
    }
    async fn prepare_tool_call(
        &self,
        call: ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, ToolOutcome> {
        self.counts.entered(&self.callback);
        self.inner.prepare_tool_call(call).await
    }
    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.counts.entered(&self.callback);
        self.inner.execute(call).await
    }
    fn readable_binding_formats(&self) -> &'static [lash_vocabulary::providers::FormatVersion] {
        self.inner.readable_binding_formats()
    }
    fn link_payload(
        &self,
        manifest: &ToolManifest,
        config: &OwnerConfigView,
    ) -> Option<(lash_vocabulary::providers::FormatVersion, serde_json::Value)> {
        self.inner.link_payload(manifest, config)
    }
    fn isolated_start(
        &self,
        call: crate::IsolatedProcessRequest<'_>,
    ) -> Result<crate::IsolatedStart, crate::ProcessEnvRefusal> {
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
        grant: &lash_vocabulary::package::DeferredGrant,
        registry: RegistryKind,
    ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
        self.inner.validate_grant(grant, registry)
    }
    async fn resolve(
        &self,
        cx: &crate::DeferredResolveContext<'_>,
        grant: &lash_vocabulary::package::DeferredGrant,
        requested: &lash_vocabulary::names::QualifiedName,
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
        let provider = syn::parse_file(include_str!("../tool_provider.rs")).unwrap();
        let methods: BTreeSet<_> = provider
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Trait(item) if item.ident == "ToolProvider" => Some(&item.items),
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
                    if item
                        .trait_
                        .as_ref()
                        .is_some_and(|(_, path, _)| path.is_ident("ToolProvider")) =>
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
