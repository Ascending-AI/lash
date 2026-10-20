//! Deferred matches: tools the run's deferred provider matches a cell on
//! demand (S6).
//!
//! The surface a run records names its deferred provider
//! ([`lash_core::DeferredToolProvider`]), which resolves the call paths a
//! cell writes that the surface does not offer into
//! [`lash_core::DeferredMatch`]es: a resolved tool with the provider that
//! executes it and its [`lash_core::ExecutionBinding`], a refusal, or an
//! unavailable path. The provider resolves on demand only: it does not
//! enumerate, advertise or rank tools.
//!
//! A cell is lowered against the effects its host offers
//! ([`lash_vm_runtime::HostBoundary`]), so resolution runs before lowering:
//! `gather → decide → offer`. The call paths the cell's source writes that
//! no surface tool answers are decided before lowering, in one batch whose
//! matches commit before any call they admit. A redrive reuses the committed
//! matches and never resolves again. The surface is never mutated: a match
//! is the cell's.

/// version_surface = "coexist"
/// version_guard(items(DEFERRED_MATCH_PREFIX_VERSION, journal_deferred_matches))
const DEFERRED_MATCH_PREFIX_VERSION: &str = "deferred_match:v1:";

/// The contract an unavailable call path is offered under: no runner serves
/// it, so no live contract is ever compared with it.
const UNAVAILABLE_CONTRACT: &str = "lash.unavailable";

use std::collections::{BTreeMap, BTreeSet};

use lash_core::{DeferredMatch, DeferredOutcome, DeferredResolveContext};
use lash_vm_runtime::{BoundaryError, HostBoundary};

/// Every refused or unavailable path with a note as an operation of
/// `boundary` whose call raises the note, so a cell reaches the host's
/// explanation at the call, in either dialect. They grant no tool
/// authority. A path that is no canonical operation, or that an offered
/// operation answers, takes no note.
pub(crate) fn offer_unavailable_notes(
    matches: &BTreeMap<String, DeferredMatch>,
    boundary: &mut HostBoundary,
) -> BTreeMap<String, String> {
    let mut notes = BTreeMap::new();
    for (path, matched) in matches {
        let note = match &matched.outcome {
            DeferredOutcome::Unavailable { note: Some(note) } => note,
            DeferredOutcome::Refused { reason } => reason,
            DeferredOutcome::Unavailable { note: None } | DeferredOutcome::Resolved { .. } => {
                continue;
            }
        };
        let Ok(operation) = lash_kernel_dialect::operation_text::parse(path) else {
            continue;
        };
        let signature = lash_kernel_doc::Signature {
            params: vec![lash_kernel_doc::Param {
                name: lash_kernel_doc::Name::new("input"),
                ty: lash_kernel_doc::Type::Any,
                optional: true,
            }],
            result: lash_kernel_doc::Type::Any,
        };
        // A note has no runner: its offer states the fixed contract of an
        // operation no runner serves, and a perform of it raises the note.
        let offered = lash_kernel_dialect::Offered {
            visibility: lash_kernel_dialect::Visibility::Public,
            requirement: lash_kernel_doc::OperationRequirement {
                signature,
                contract: lash_kernel_doc::ContractId::new(UNAVAILABLE_CONTRACT),
            },
            controls: BTreeSet::new(),
            read: false,
            source: lash_kernel_dialect::OfferedSource::Unavailable { note: note.clone() },
        };
        if boundary.offer_operation(operation, offered).is_ok() {
            notes.insert(path.clone(), note.clone());
        }
    }
    notes
}

/// Refuse before a resolved grant can be committed or offered.
fn refuse_internal(
    outcome: DeferredOutcome,
    view: &dyn lash_kernel_dialect::ExportView,
) -> Result<DeferredOutcome, lash_execution_contracts::plugin_error::PluginError> {
    if let DeferredOutcome::Resolved { link } = &outcome
        && let Some(definition) = link.resolution()
        && lash_kernel_dialect::operation_visibility(view, &definition.manifest.operation)
            == lash_kernel_dialect::Visibility::Internal
    {
        return Err(
            lash_execution_contracts::plugin_error::PluginError::InternalOperation {
                operation: definition.manifest.operation.effect_name().to_string(),
            },
        );
    }
    if let DeferredOutcome::Resolved { link } = &outcome
        && let Some(definition) = link.resolution()
        && view.owns_namespace(definition.manifest.operation.effect_name().as_str())
    {
        return Err(lash_execution_contracts::PluginError::PackageNamespace {
            path: definition.manifest.operation.effect_name().to_string(),
        });
    }
    Ok(outcome)
}

/// A cell could not decide or offer its deferred matches.
#[derive(Debug, thiserror::Error)]
pub enum DeferredResolutionError {
    #[error("deferred resolution requires an admitted ExecCode effect address")]
    MissingLinkIdentity,
    #[error("deferred provider address refused: {0}")]
    ProviderRegistry(#[source] lash_execution_contracts::plugin_error::PluginError),
    #[error("failed to commit deferred matches: {0}")]
    Journal(#[source] lash_core::RuntimeEffectControllerError),
    #[error("journaled deferred matches are invalid: {0}")]
    InvalidJournaledOutcome(#[source] serde_json::Error),
    /// The run's surface recorded a deferred provider this node does not
    /// serve: the cell parks until a node that serves it claims the run.
    #[error("the run's deferred tool provider `{provider}` is not served on this node")]
    ProviderUnserved { provider: lash_core::ProviderKey },
    /// A match's binding is in a format its codec does not read on this
    /// node (S8): the cell parks before the match is offered.
    #[error("the matched tool for `{path}` cannot run here: {source}")]
    BindingUnreadable {
        path: String,
        #[source]
        source: lash_core::ExecutionBindingUnreadable,
    },
    #[error("failed to offer the matched tool for `{path}`: {source}")]
    Offer {
        path: String,
        #[source]
        source: Box<BoundaryError>,
    },
}

impl DeferredResolutionError {
    /// Maps a failure onto the runtime's bounded retry, park and terminal
    /// vocabulary without discarding its typed source.
    pub fn runtime_effect_error(&self) -> lash_core::RuntimeEffectControllerError {
        let code = match self {
            Self::Journal(error) => return error.clone(),
            Self::ProviderRegistry(error) => return error.clone().into(),
            Self::ProviderUnserved { .. } => lash_core::RuntimeErrorCode::LashVmCellBindingDrift,
            Self::BindingUnreadable { .. } => {
                lash_core::RuntimeErrorCode::ExecutionBindingUnreadable
            }
            Self::MissingLinkIdentity | Self::InvalidJournaledOutcome(_) | Self::Offer { .. } => {
                lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed
            }
        };
        lash_core::RuntimeEffectControllerError::new(code, self.to_string())
    }
}

/// Decides each candidate under the cell's effect: the run's deferred
/// provider resolves the batch once, and the matches commit before the cell
/// is lowered. A redrive reuses the committed matches; a precommit redrive
/// may resolve again. `None` when the run's surface records no deferred
/// provider.
pub(crate) async fn journal_deferred_matches(
    candidates: BTreeSet<String>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    view: &(dyn lash_kernel_dialect::ExportView + Sync),
) -> Result<Option<BTreeMap<String, DeferredMatch>>, DeferredResolutionError> {
    let mut local = BTreeMap::new();
    let mut external = BTreeSet::new();
    for path in candidates {
        let segments = path
            .split('.')
            .map(lash_kernel_dialect::Segment::new)
            .collect::<Result<Vec<_>, _>>();
        let classified = segments.as_ref().map_or_else(
            |_| {
                if view.owns_namespace(&path) {
                    lash_kernel_dialect::PackagePathClass::Unknown
                } else {
                    lash_kernel_dialect::PackagePathClass::Outside
                }
            },
            |segments| lash_kernel_dialect::classify_package_path(view, segments),
        );
        match classified {
            lash_kernel_dialect::PackagePathClass::Outside => {
                external.insert(path);
                continue;
            }
            lash_kernel_dialect::PackagePathClass::Internal => {
                return Err(DeferredResolutionError::ProviderRegistry(
                    lash_execution_contracts::PluginError::InternalOperation { operation: path },
                ));
            }
            lash_kernel_dialect::PackagePathClass::Public
            | lash_kernel_dialect::PackagePathClass::Unknown => {}
        }
        local.insert(
            path.clone(),
            DeferredMatch {
                requested: path.clone(),
                outcome: DeferredOutcome::Refused {
                    reason: format!(
                        "no such member `{path}` is offered by the installed package surface"
                    ),
                },
            },
        );
    }
    if external.is_empty() {
        return Ok(Some(local));
    }
    let candidates = external;
    let (provider_handle, provider) = match ctx.deferred_provider() {
        Ok(Some((handle, provider))) => {
            ctx.owner_config()
                .validate_provider(handle)
                .map_err(DeferredResolutionError::ProviderRegistry)?;
            (handle.clone(), std::sync::Arc::clone(provider))
        }
        Ok(None) => return Ok((!local.is_empty()).then_some(local)),
        Err(provider) => {
            ctx.owner_config()
                .validate_provider(&provider)
                .map_err(DeferredResolutionError::ProviderRegistry)?;
            return Err(DeferredResolutionError::ProviderUnserved { provider });
        }
    };
    let address = ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    let effect_id = format!("{}:deferred-tool-resolution", address.replay_key);
    let operation = format!(
        "{DEFERRED_MATCH_PREFIX_VERSION}{}",
        serde_json::to_string(&candidates)
            .map_err(DeferredResolutionError::InvalidJournaledOutcome)?
    );
    let phase_context = ctx.clone();
    let owner = ctx.owner().clone();
    let run = ctx.logical_run().cloned();
    let capabilities = ctx.run_capabilities().clone();
    let config = ctx.owner_config().clone();
    let journaled = ctx
        .journaled_deferred_resolution_with(effect_id, operation, move || async move {
            let paths = candidates.iter().map(String::as_str).collect::<Vec<_>>();
            let cx = DeferredResolveContext::new(
                &provider_handle,
                &owner,
                run.as_ref(),
                &capabilities,
                &config,
            );
            let mut resolved = provider.resolve(&cx, &paths).await;
            let matches = paths
                .into_iter()
                .map(|path| {
                    Ok(DeferredMatch {
                        requested: path.to_owned(),
                        outcome: refuse_internal(
                            resolved
                                .remove(path)
                                .unwrap_or(DeferredOutcome::Unavailable { note: None }),
                            view,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, lash_execution_contracts::plugin_error::PluginError>>()?;
            for matched in &matches {
                if let Some(link) = matched.resolved() {
                    config
                        .validate_provider(&link.provider)
                        .map_err(lash_core::RuntimeEffectControllerError::from)?;
                    if let Some(binding) = &link.payload {
                        config
                            .validate_provider(&binding.codec)
                            .map_err(lash_core::RuntimeEffectControllerError::from)?;
                    }
                }
            }
            let _phase = phase_context
                .named_phase("codemode_lash_vm.deferred_resolve.after_resolver_return");
            serde_json::to_value(matches).map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to encode deferred matches: {error}"),
                )
            })
        })
        .await
        .map_err(DeferredResolutionError::Journal)?;
    {
        let _phase = ctx.named_phase("codemode_lash_vm.deferred_resolve.after_durable_record");
    }
    let matches: Vec<DeferredMatch> = serde_json::from_value(journaled)
        .map_err(DeferredResolutionError::InvalidJournaledOutcome)?;
    Ok(Some(
        matches
            .into_iter()
            .map(|matched| (matched.requested.clone(), matched))
            .chain(local)
            .collect(),
    ))
}

/// `offer`: every resolved match of `matches` as an operation of
/// `boundary`, the matched tool's own, and the grant its calls execute
/// under: the matched tool, on the provider the match names, with its
/// binding.
pub(crate) fn offer_deferred_matches(
    boundary: &mut HostBoundary,
    matches: &BTreeMap<String, DeferredMatch>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>, DeferredResolutionError> {
    let mut grants = BTreeMap::new();
    for (path, matched) in matches {
        let Some(link) = matched.resolved() else {
            continue;
        };
        let (provider, binding) = (&link.provider, link.payload.as_ref());
        let Some(definition) = link.resolution() else {
            continue;
        };
        ctx.owner_config()
            .validate_provider(provider)
            .map_err(DeferredResolutionError::ProviderRegistry)?;
        if let Some(binding) = binding {
            ctx.owner_config()
                .validate_provider(&binding.codec)
                .map_err(DeferredResolutionError::ProviderRegistry)?;
        }
        // A binding is read by its codec only in a format the codec
        // declares; one it cannot read parks the cell before anything is
        // offered, and an unserved codec is an unserved provider.
        if let Some(binding) = binding {
            let readable = ctx.binding_formats(&binding.codec).ok_or_else(|| {
                DeferredResolutionError::ProviderUnserved {
                    provider: binding.codec.clone(),
                }
            })?;
            binding.check_readable(readable).map_err(|source| {
                DeferredResolutionError::BindingUnreadable {
                    path: path.clone(),
                    source,
                }
            })?;
        }
        boundary
            .offer_operation(
                definition.manifest.operation.clone(),
                lash_vm_runtime::offered_tool(
                    &definition.manifest,
                    &definition.contract,
                    lash_kernel_dialect::OfferedSource::Deferred {
                        tool: definition.manifest.id.to_string(),
                    },
                ),
            )
            .map_err(|source| DeferredResolutionError::Offer {
                path: path.clone(),
                source: Box::new(source),
            })?;
        // A match naming a provider this node does not register parks the
        // cell for a node that does, as an unserved codec does.
        let provider = ctx
            .tool_provider(&definition.manifest.id, Some(provider))
            .map_err(|_| DeferredResolutionError::ProviderUnserved {
                provider: provider.clone(),
            })?;
        let Some(execution_grant) =
            lash_core::ToolExecutionGrant::of_link(&lash_core::OperationLink {
                provider,
                ..link.clone()
            })
        else {
            continue;
        };
        grants.insert(execution_grant.manifest().id.clone(), execution_grant);
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process address returned for a session refuses before the unserved-provider park.
    #[tokio::test]
    async fn a_process_provider_handle_is_refused_before_session_provider_lookup() {
        let host = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let handle = lash_core::plugin::ToolProviderHandle::new(
            lash_core::RegistryKind::Process,
            "process-tools",
            lash_core::tool_provider_key!("deferred"),
        );
        let surface = lash_core::RecordedSurface::new(Vec::new(), Some(handle.clone()))
            .expect("a recorded surface");
        let routes = lash_core::SurfaceTools::of_surface(
            &surface,
            std::sync::Arc::new(lash_core::ToolRegistry::default()),
        );
        let context = lash_core::testing::TestExecutionContextBuilder::new(host.ports())
            .routes(routes)
            .build()
            .into_runtime();
        let error = journal_deferred_matches(
            BTreeSet::from(["outside.lookup".into()]),
            &context,
            &lash_kernel_dialect::NoPackages,
        )
        .await
        .expect_err("the process address cannot resolve a session");
        assert!(
            matches!(&error, DeferredResolutionError::ProviderRegistry(lash_execution_contracts::plugin_error::PluginError::ProviderRegistryMismatch { expected: lash_core::RegistryKind::Session, provider }) if provider == &handle)
        );
        let controller = error.runtime_effect_error();
        assert!(
            matches!(
                controller.cause,
                Some(lash_core::RuntimeErrorCause::PluginOperation { .. })
            ),
            "the host receives a typed cause"
        );
    }
}
