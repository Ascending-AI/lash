//! RLM-only deferred trigger-definition resolution.
//!
//! Trigger definition discovery is intentionally separate from deferred tool
//! discovery: it has its own provider, registry, resolver, grant and durable
//! record. Resolution only contributes a constructor and its event schema to a
//! link-scoped catalog. It never installs a route, activates a provider,
//! creates a subscription, or changes Tool Catalog membership.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::LashlangSurface;

/// Link-scoped authority to use one trigger-source constructor.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TriggerGrant {
    /// Stable provider identity captured with the grant for later route use.
    pub provider_id: String,
    /// Fully-qualified constructor path, e.g. `calendar.Changed`.
    pub constructor_path: Vec<String>,
    /// Constructor input contract.
    pub input_type: lashlang::TypeExpr,
    /// Named event type and its complete schema dependency.
    pub event_type: lashlang::NamedDataType,
    /// Provider-owned route data. The linker records but never executes it.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub route: serde_json::Value,
}

impl TriggerGrant {
    pub fn new(
        constructor_path: impl IntoIterator<Item = impl Into<String>>,
        input_type: lashlang::TypeExpr,
        event_type: lashlang::NamedDataType,
    ) -> Self {
        Self {
            provider_id: String::new(),
            constructor_path: constructor_path.into_iter().map(Into::into).collect(),
            input_type,
            event_type,
            route: serde_json::Value::Null,
        }
    }

    pub fn with_route(mut self, route: serde_json::Value) -> Self {
        self.route = route;
        self
    }

    /// Set the stable provider identity when a host implements the batch
    /// resolver directly. [`DeferredTriggerProviderRegistry`] sets this from
    /// [`DeferredTriggerProvider::id`] automatically.
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = provider_id.into();
        self
    }

    pub fn call_path(&self) -> String {
        self.constructor_path.join(".")
    }
}

/// Outcome for one trigger-constructor reference.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerResolution {
    Resolved(Box<TriggerGrant>),
    NotAvailable,
    Ambiguous { provider_ids: Vec<String> },
}

/// One source of trigger definitions. Providers perform read-only discovery;
/// provider activation belongs to registration/execution, not linking.
#[async_trait]
pub trait DeferredTriggerProvider: Send + Sync {
    fn id(&self) -> &str;

    /// Return a definition only when this provider owns `path`.
    async fn resolve(&self, path: &str) -> Option<TriggerGrant>;
}

/// Deterministic registry that applies the unannotated-reference rule: zero
/// definitions is unavailable, one resolves, and multiple are ambiguous.
#[derive(Default)]
pub struct DeferredTriggerProviderRegistry {
    providers: Vec<Arc<dyn DeferredTriggerProvider>>,
}

impl DeferredTriggerProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn DeferredTriggerProvider>) {
        self.providers.push(provider);
        self.providers
            .sort_by(|left, right| left.id().cmp(right.id()));
    }
}

/// Host-provided resolver for trigger definitions absent from the link surface.
#[async_trait]
pub trait DeferredTriggerResolver: Send + Sync {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, TriggerResolution>;
}

#[async_trait]
impl DeferredTriggerResolver for DeferredTriggerProviderRegistry {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, TriggerResolution> {
        let mut outcomes = BTreeMap::new();
        for path in paths {
            let mut matches = Vec::new();
            for provider in &self.providers {
                if let Some(mut grant) = provider.resolve(path).await {
                    grant.provider_id = provider.id().to_string();
                    matches.push(grant);
                }
            }
            let outcome = match matches.as_slice() {
                [] => TriggerResolution::NotAvailable,
                [grant] => TriggerResolution::Resolved(Box::new(grant.clone())),
                grants => TriggerResolution::Ambiguous {
                    provider_ids: grants
                        .iter()
                        .map(|grant| grant.provider_id.clone())
                        .collect(),
                },
            };
            outcomes.insert((*path).to_string(), outcome);
        }
        outcomes
    }
}

pub type SharedDeferredTriggerResolver = Arc<dyn DeferredTriggerResolver>;

/// Trigger outcomes for the active admitted `ExecCode` link. This remains a
/// distinct durable record from tool outcomes, even when both use the same
/// effect address.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeferredTriggerResolutionRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_key: Option<crate::DeferredResolutionLinkKey>,
    pub resolutions: BTreeMap<String, TriggerResolution>,
}

impl DeferredTriggerResolutionRecord {
    pub fn select_link(&mut self, link_key: crate::DeferredResolutionLinkKey) {
        if self.link_key.as_ref() != Some(&link_key) {
            self.link_key = Some(link_key);
            self.resolutions.clear();
        }
    }

    pub fn clear_link(&mut self) {
        self.link_key = None;
        self.resolutions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.resolutions.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DeferredTriggerResolutionError {
    #[error("deferred trigger resolution requires an admitted ExecCode effect address")]
    MissingLinkIdentity,
    #[error(
        "deferred trigger resolution record does not match the admitted ExecCode effect address"
    )]
    LinkIdentityMismatch,
    #[error("failed to commit deferred trigger resolution outcome: {0}")]
    Journal(#[source] lash_core::RuntimeEffectControllerError),
    #[error("journaled deferred trigger resolution outcome is invalid: {0}")]
    InvalidJournaledOutcome(#[source] serde_json::Error),
    #[error("invalid Lashlang host trigger surface: {0}")]
    Ambient(#[source] crate::ToolBindingError),
    #[error("trigger constructor `{path}` is ambiguous across providers {provider_ids:?}")]
    Ambiguous {
        path: String,
        provider_ids: Vec<String>,
    },
    #[error("deferred trigger grant for `{path}` names constructor `{grant_path}`")]
    GrantPathMismatch { path: String, grant_path: String },
    #[error("deferred trigger grant for `{path}` has no provider identity")]
    MissingProviderIdentity { path: String },
    #[error("failed to fold deferred trigger grant for `{path}`: {source}")]
    Fold {
        path: String,
        #[source]
        source: lashlang::LashlangHostCatalogError,
    },
}

impl DeferredTriggerResolutionError {
    pub fn runtime_effect_error(&self) -> lash_core::RuntimeEffectControllerError {
        match self {
            Self::Journal(error) => error.clone(),
            _ => lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed,
                self.to_string(),
            ),
        }
    }
}

/// Resolve and atomically fold missing trigger constructors and their event
/// schema dependencies into a link-scoped surface.
pub async fn resolve_and_fold_deferred_triggers(
    referenced: &BTreeSet<String>,
    mut surface: LashlangSurface,
    resolver: Option<&SharedDeferredTriggerResolver>,
    record: &DeferredTriggerResolutionRecord,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<(LashlangSurface, DeferredTriggerResolutionRecord), DeferredTriggerResolutionError> {
    if referenced.is_empty() || (resolver.is_none() && record.resolutions.is_empty()) {
        return Ok((surface, record.clone()));
    }
    let link_key = record
        .link_key
        .as_ref()
        .ok_or(DeferredTriggerResolutionError::MissingLinkIdentity)?;
    let admitted_address = ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
        .ok_or(DeferredTriggerResolutionError::MissingLinkIdentity)?;
    if admitted_address != &link_key.address {
        return Err(DeferredTriggerResolutionError::LinkIdentityMismatch);
    }

    let ambient_paths = referenced
        .iter()
        .filter(|path| surface.resources.provides_value_constructor(path))
        .cloned()
        .collect::<BTreeSet<_>>();
    let base_environment = surface
        .host_environment(&lash_core::ToolCatalog::default())
        .map_err(DeferredTriggerResolutionError::Ambient)?;
    let ambient_paths = referenced
        .iter()
        .filter(|path| {
            if ambient_paths.contains(*path) {
                return true;
            }
            let Some((module_path, operation)) = path.rsplit_once('.') else {
                return false;
            };
            base_environment
                .resources
                .provides_module_operation(module_path, operation)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let recorded = record.resolutions.clone();
    let referenced_for_resolution = referenced.clone();
    let resolver_for_resolution = resolver.cloned();
    let effect_id = format!(
        "{}:deferred-trigger-resolution",
        link_key.address.replay_key
    );
    let operation = format!(
        "deferred_trigger_resolution:v1:{}",
        serde_json::to_string(referenced)
            .expect("deferred trigger call-paths encode as canonical JSON")
    );
    let journaled = ctx
        .journaled_deferred_resolution_with(effect_id, operation, move || async move {
            let mut outcomes = BTreeMap::new();
            let mut unknown = Vec::new();
            for path in &referenced_for_resolution {
                if let Some(outcome) = recorded.get(path) {
                    outcomes.insert(path.clone(), outcome.clone());
                } else if !ambient_paths.contains(path) {
                    unknown.push(path.as_str());
                }
            }
            if let Some(resolver) = resolver_for_resolution.as_ref()
                && !unknown.is_empty()
            {
                let mut resolved = resolver.resolve(&unknown).await;
                for path in unknown {
                    outcomes.insert(
                        path.to_string(),
                        resolved
                            .remove(path)
                            .unwrap_or(TriggerResolution::NotAvailable),
                    );
                }
            }
            serde_json::to_value(outcomes).map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to encode deferred trigger resolution outcome: {error}"),
                )
            })
        })
        .await
        .map_err(DeferredTriggerResolutionError::Journal)?;
    let outcomes: BTreeMap<String, TriggerResolution> = serde_json::from_value(journaled)
        .map_err(DeferredTriggerResolutionError::InvalidJournaledOutcome)?;

    for path in outcomes.keys() {
        surface.resources.mask_trigger_source_constructor(path);
    }
    for (path, outcome) in &outcomes {
        match outcome {
            TriggerResolution::NotAvailable => {}
            TriggerResolution::Ambiguous { provider_ids } => {
                return Err(DeferredTriggerResolutionError::Ambiguous {
                    path: path.clone(),
                    provider_ids: provider_ids.clone(),
                });
            }
            TriggerResolution::Resolved(grant) => {
                if grant.provider_id.trim().is_empty() {
                    return Err(DeferredTriggerResolutionError::MissingProviderIdentity {
                        path: path.clone(),
                    });
                }
                let grant_path = grant.call_path();
                if grant.constructor_path.is_empty() || grant_path != *path {
                    return Err(DeferredTriggerResolutionError::GrantPathMismatch {
                        path: path.clone(),
                        grant_path,
                    });
                }
                surface
                    .resources
                    .add_trigger_source_constructor(
                        grant.constructor_path.iter().map(String::as_str),
                        grant.input_type.clone(),
                        grant.event_type.clone(),
                    )
                    .map_err(|source| DeferredTriggerResolutionError::Fold {
                        path: path.clone(),
                        source,
                    })?;
            }
        }
    }

    Ok((
        surface,
        DeferredTriggerResolutionRecord {
            link_key: record.link_key.clone(),
            resolutions: outcomes,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn event_type(name: &str, field: &str) -> lashlang::NamedDataType {
        lashlang::NamedDataType::object(
            name,
            vec![lashlang::TypeField {
                name: field.into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }],
        )
        .expect("valid event type")
    }

    fn grant(path: &str, provider_route: &str) -> TriggerGrant {
        TriggerGrant::new(
            path.split('.'),
            lashlang::TypeExpr::Object(vec![]),
            event_type("calendar.Change", "id"),
        )
        .with_route(serde_json::json!({"route": provider_route}))
    }

    struct Provider {
        id: &'static str,
        path: &'static str,
        calls: Arc<AtomicUsize>,
        observed: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DeferredTriggerProvider for Provider {
        fn id(&self) -> &str {
            self.id
        }

        async fn resolve(&self, path: &str) -> Option<TriggerGrant> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.observed.lock_recover().push(path.to_string());
            (path == self.path).then(|| grant(path, self.id))
        }
    }

    fn provider(id: &'static str, path: &'static str) -> (Arc<Provider>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Provider {
                id,
                path,
                calls: Arc::clone(&calls),
                observed: Arc::new(Mutex::new(Vec::new())),
            }),
            calls,
        )
    }

    fn context(
        record: &mut DeferredTriggerResolutionRecord,
        replay_key: &str,
    ) -> lash_core::RuntimeExecutionContext<'static> {
        let invocation = lash_core::testing::exec_code_invocation(
            "session",
            "turn",
            0,
            0,
            "exec-code",
            replay_key,
        );
        record.select_link(
            crate::DeferredResolutionLinkKey::from_exec_code_invocation(&invocation)
                .expect("effect invocation has identity"),
        );
        lash_core::testing::code_execution_context_with_invocation(invocation)
    }

    #[tokio::test]
    async fn registry_applies_zero_one_two_provider_rule() {
        let (first, _) = provider("first", "calendar.Changed");
        let (second, _) = provider("second", "calendar.Changed");
        let mut one = DeferredTriggerProviderRegistry::new();
        one.register(first.clone());
        assert!(matches!(
            one.resolve(&["missing.Source"]).await["missing.Source"],
            TriggerResolution::NotAvailable
        ));
        assert!(matches!(
            one.resolve(&["calendar.Changed"]).await["calendar.Changed"],
            TriggerResolution::Resolved(ref grant) if grant.provider_id == "first"
        ));

        one.register(second);
        assert_eq!(
            one.resolve(&["calendar.Changed"]).await["calendar.Changed"],
            TriggerResolution::Ambiguous {
                provider_ids: vec!["first".to_string(), "second".to_string()]
            }
        );
    }

    #[tokio::test]
    async fn deferred_and_resident_definitions_build_equivalent_link_surfaces() {
        let trigger_grant = grant("calendar.Changed", "calendar-primary");
        let mut resident = LashlangSurface::default();
        resident
            .resources
            .add_trigger_source_constructor(
                trigger_grant.constructor_path.iter().map(String::as_str),
                trigger_grant.input_type.clone(),
                trigger_grant.event_type.clone(),
            )
            .expect("resident definition is valid");

        let (provider, _) = provider("calendar-provider", "calendar.Changed");
        let mut registry = DeferredTriggerProviderRegistry::new();
        registry.register(provider);
        let resolver: SharedDeferredTriggerResolver = Arc::new(registry);
        let referenced = BTreeSet::from(["calendar.Changed".to_string()]);
        let mut record = DeferredTriggerResolutionRecord::default();
        let ctx = context(&mut record, "exec-code:resident-equivalence");
        let (deferred, _) = resolve_and_fold_deferred_triggers(
            &referenced,
            LashlangSurface::default(),
            Some(&resolver),
            &record,
            &ctx,
        )
        .await
        .expect("deferred definition resolves");

        assert_eq!(deferred.resources, resident.resources);
    }

    #[tokio::test]
    async fn recorded_grant_masks_changed_ambient_and_preserves_route_without_activation() {
        let (provider, calls) = provider("calendar-provider", "calendar.Changed");
        let mut registry = DeferredTriggerProviderRegistry::new();
        registry.register(provider);
        let resolver: SharedDeferredTriggerResolver = Arc::new(registry);
        let referenced = BTreeSet::from(["calendar.Changed".to_string()]);
        let mut record = DeferredTriggerResolutionRecord::default();
        let ctx = context(&mut record, "exec-code:trigger-replay");
        let (resolved, captured) = resolve_and_fold_deferred_triggers(
            &referenced,
            LashlangSurface::default(),
            Some(&resolver),
            &record,
            &ctx,
        )
        .await
        .expect("definition resolves");
        assert!(
            resolved
                .resources
                .provides_value_constructor("calendar.Changed")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let mut changed = LashlangSurface::default();
        changed
            .resources
            .add_trigger_source_constructor(
                ["calendar", "Changed"],
                lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                    name: "different".into(),
                    ty: lashlang::TypeExpr::Int,
                    optional: false,
                }]),
                event_type("calendar.Change", "different"),
            )
            .expect("changed ambient definition");
        let replay_ctx = context(&mut record, "exec-code:trigger-replay");
        let (replayed, replayed_record) =
            resolve_and_fold_deferred_triggers(&referenced, changed, None, &captured, &replay_ctx)
                .await
                .expect("captured definition replays");
        let constructor = replayed
            .resources
            .resolve_value_constructor(&["calendar", "Changed"])
            .expect("captured constructor restored");
        assert_eq!(constructor.input_ty, lashlang::TypeExpr::Object(vec![]));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            replayed_record.resolutions["calendar.Changed"],
            TriggerResolution::Resolved(ref grant)
                if grant.route == serde_json::json!({"route": "calendar-provider"})
        ));
    }

    #[tokio::test]
    async fn recorded_unavailable_masks_later_ambient_trigger_definition() {
        let resolver: SharedDeferredTriggerResolver =
            Arc::new(DeferredTriggerProviderRegistry::new());
        let referenced = BTreeSet::from(["calendar.Changed".to_string()]);
        let mut record = DeferredTriggerResolutionRecord::default();
        let ctx = context(&mut record, "exec-code:negative-trigger-replay");
        let (_, captured) = resolve_and_fold_deferred_triggers(
            &referenced,
            LashlangSurface::default(),
            Some(&resolver),
            &record,
            &ctx,
        )
        .await
        .expect("unavailable outcome records");
        assert!(matches!(
            captured.resolutions["calendar.Changed"],
            TriggerResolution::NotAvailable
        ));

        let mut changed = LashlangSurface::default();
        changed
            .resources
            .add_trigger_source_constructor(
                ["calendar", "Changed"],
                lashlang::TypeExpr::Object(vec![]),
                event_type("calendar.Change", "id"),
            )
            .expect("later ambient definition");
        let replay_ctx = context(&mut record, "exec-code:negative-trigger-replay");
        let (replayed, replayed_record) =
            resolve_and_fold_deferred_triggers(&referenced, changed, None, &captured, &replay_ctx)
                .await
                .expect("negative outcome replays");

        assert!(
            !replayed
                .resources
                .provides_value_constructor("calendar.Changed")
        );
        assert!(matches!(
            replayed_record.resolutions["calendar.Changed"],
            TriggerResolution::NotAvailable
        ));
    }

    #[tokio::test]
    async fn ambiguous_definition_fails_before_linking() {
        let (first, _) = provider("a", "calendar.Changed");
        let (second, _) = provider("b", "calendar.Changed");
        let mut registry = DeferredTriggerProviderRegistry::new();
        registry.register(second);
        registry.register(first);
        let resolver: SharedDeferredTriggerResolver = Arc::new(registry);
        let referenced = BTreeSet::from(["calendar.Changed".to_string()]);
        let mut record = DeferredTriggerResolutionRecord::default();
        let ctx = context(&mut record, "exec-code:ambiguous-trigger");

        assert!(matches!(
            resolve_and_fold_deferred_triggers(
                &referenced,
                LashlangSurface::default(),
                Some(&resolver),
                &record,
                &ctx,
            )
            .await,
            Err(DeferredTriggerResolutionError::Ambiguous { path, provider_ids })
                if path == "calendar.Changed"
                    && provider_ids == ["a".to_string(), "b".to_string()]
        ));
    }
}
