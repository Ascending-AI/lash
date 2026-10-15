//! Counting adapters on the typed recipe's executable binders.
use crate::plugin::PluginContributions;
use crate::plugin::*;
use crate::{
    ToolCall, ToolContract, ToolId, ToolManifest, ToolOutcome, ToolPrepareCall, ToolProvider,
};
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Counts callback entries, not factory construction or metadata inspection.
#[derive(Clone, Default)]
pub struct CallbackCounts(Arc<Mutex<Entries>>);
#[derive(Default)]
struct Entries {
    callbacks: BTreeMap<PluginCallbackIdentity, u64>,
    resolutions: BTreeMap<PluginCallbackIdentity, u64>,
}
impl CallbackCounts {
    pub fn snapshot(&self) -> BTreeMap<PluginCallbackIdentity, u64> {
        self.0.lock_recover().callbacks.clone()
    }
    /// Resolution entries, separate from prepare/execute on the selected provider.
    /// A registered resolver has a zero entry even before it is exercised.
    pub fn resolution_snapshot(&self) -> BTreeMap<PluginCallbackIdentity, u64> {
        self.0.lock_recover().resolutions.clone()
    }
    fn entered(&self, callback: &PluginCallbackIdentity) {
        *self
            .0
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
            runtime_event_hooks,
            presentation_steps,
            cell_failure_feedback
        );
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
        if let Some(registered) = &mut contributions.deferred_provider {
            let callback = registered.identity.callback_identity();
            self.0
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
    fn readable_binding_formats(&self) -> &'static [crate::FormatVersion] {
        self.inner.readable_binding_formats()
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
            .lock_recover()
            .resolutions
            .entry(self.callback.clone())
            .or_default() += 1;
        self.inner.resolve(cx, paths).await
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
    ) -> Result<std::collections::BTreeSet<HistoryPartId>, PluginError> {
        self.counts.entered(&self.callback);
        self.inner.omissions(ctx, history)
    }
}
