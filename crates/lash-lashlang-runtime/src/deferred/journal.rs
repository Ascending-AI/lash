//! Durable journaling and application of deferred resolution outcomes.
//!
//! Moved verbatim out of `deferred.rs` to keep the parent module under the
//! production line budget; the journal identity and the recorded-then-fold
//! application order live together here.

use std::collections::{BTreeMap, BTreeSet};

use crate::LashlangHostEnvironment;
use crate::deferred::{
    DeferredResolutionError, DeferredResolutionRecord, Resolution, SharedDeferredToolResolver,
    ToolBindingError, fold_grant,
};

#[expect(
    clippy::expect_used,
    reason = "the journaled deferred-tool identity is a call path of validated strings, so it encodes as canonical JSON, as the site's own message states"
)]
pub(super) async fn journal_deferred_outcomes<F>(
    referenced: BTreeSet<String>,
    ambient_paths: F,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &DeferredResolutionRecord,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<BTreeMap<String, Resolution>, DeferredResolutionError>
where
    F: FnOnce() -> Result<BTreeSet<String>, ToolBindingError> + Send,
{
    let link_key = record
        .link_key
        .as_ref()
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    let admitted_address = ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    if admitted_address != &link_key.address {
        return Err(DeferredResolutionError::LinkIdentityMismatch);
    }
    let effect_id = format!("{}:deferred-tool-resolution", link_key.address.replay_key);
    let operation = format!(
        "deferred_tool_resolution:v1:{}",
        serde_json::to_string(&referenced)
            .expect("deferred call-path strings encode as canonical JSON")
    );
    let recorded = record.resolutions.clone();
    let phase_context = ctx.clone();
    let resolver_for_resolution = resolver.cloned();
    let referenced_for_resolution = referenced.clone();
    let journaled = ctx
        .journaled_deferred_resolution_with(effect_id, operation, move || async move {
            let ambient_paths = match ambient_paths() {
                Ok(paths) => paths,
                Err(error) => {
                    let message = error.to_string();
                    return Err(lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed,
                        message,
                    ));
                }
            };
            let mut outcomes = BTreeMap::new();
            let mut unknown = Vec::new();
            for path in &referenced_for_resolution {
                if let Some(resolution) = recorded.get(path) {
                    outcomes.insert(path.clone(), resolution.clone());
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
                        resolved.remove(path).unwrap_or(Resolution::NotAvailable),
                    );
                }
            }
            let _phase =
                phase_context.named_phase("rlm_lashlang.deferred_resolve.after_resolver_return");
            serde_json::to_value(outcomes).map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to encode deferred resolution outcome: {error}"),
                )
            })
        })
        .await;
    let journaled = match journaled {
        Ok(journaled) => journaled,
        Err(error)
            if error.code == lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed
                && error.summary.is_none()
                && error.cause.is_none() =>
        {
            let message = error.message;
            return Err(DeferredResolutionError::Ambient(Box::new(
                ToolBindingError::JournaledAmbient { message },
            )));
        }
        Err(error) => return Err(DeferredResolutionError::Journal(error)),
    };
    {
        let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve.after_durable_record");
    }
    serde_json::from_value(journaled).map_err(DeferredResolutionError::InvalidJournaledOutcome)
}

pub(super) fn apply_deferred_outcomes(
    host_environment: &mut LashlangHostEnvironment,
    outcomes: &BTreeMap<String, Resolution>,
    resolver: Option<&SharedDeferredToolResolver>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<(), DeferredResolutionError> {
    // Recorded authority is applied before ambient availability is consulted:
    // exact paths are masked first, negative outcomes remain absent, and
    // positive outcomes fold their captured definitions through the catalog's
    // ordinary collision checks before any process-local route is installed.
    for path in outcomes.keys() {
        if let Some((module_path, operation)) = path.rsplit_once('.') {
            host_environment
                .resources
                .mask_module_operation(module_path, operation);
        }
    }
    for (path, resolution) in outcomes {
        let Resolution::Resolved(grant) = resolution else {
            continue;
        };
        fold_grant(host_environment, grant).map_err(|source| DeferredResolutionError::Fold {
            path: path.clone(),
            source: Box::new(source),
        })?;
    }
    for (path, resolution) in outcomes {
        let Resolution::Resolved(grant) = resolution else {
            continue;
        };
        let Some(resolver) = resolver else {
            continue;
        };
        {
            let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve.before_registration");
            resolver
                .install_recorded_grant(path, grant)
                .map_err(|source| DeferredResolutionError::Install {
                    path: path.clone(),
                    source,
                })?;
        }
    }
    Ok(())
}
