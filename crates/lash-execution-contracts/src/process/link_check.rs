//! P1: the link check at the start hold point. A process is the pair of a
//! template its engine runs and the env it starts under; the pair links only
//! when the env maps exactly the effects the template performs, each to a
//! tool that fits the effect's kernel signature and that an env component
//! serves under the contract the process pins.

use std::collections::BTreeMap;

use lash_kernel_doc::EffectName;
use serde::{Deserialize, Serialize};

use crate::OwnedOperations;
use crate::PerformedEffects;
use crate::ProcessEnv;
use crate::ProcessEnvRefusal;

/// How a linked tool must fit the kernel signature of the effect it
/// implements (R7): a parameter of one link check, never stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitPolicy {
    /// The tool's projected signature serves the effect's (K-ADM-002).
    #[default]
    Compatible,
    /// The tool's projected signature is the effect's.
    Exact,
}

/// Check that `env` links exactly the effects of `performed`, each to a
/// tool that fits it under its policy in `fits` (`Compatible` when absent)
/// and is served, as `served` holds it, under the contract the process's
/// code requires (OP-CONTRACT-PIN). An effect of `owned`, on a path a
/// package owns, is served only by a tool that declares the package's
/// contract id and intents and serves its signature (PKG-RUNNER). `served`
/// is each link's tool as an env component serves it on this node,
/// admitted under the link ([`crate::OperationRoute::admitted`]); a link it
/// lacks is not served. A dispatch grant fits like any tool: the document
/// is checked against the granted tool's signature, and its target is
/// matched per call.
///
/// # Errors
///
/// [`ProcessEnvRefusal::EffectNotLinked`],
/// [`ProcessEnvRefusal::EffectNotPerformed`],
/// [`ProcessEnvRefusal::ToolNotServed`],
/// [`ProcessEnvRefusal::RunnerNotPackaged`],
/// [`ProcessEnvRefusal::ContractChanged`] or
/// [`ProcessEnvRefusal::EffectUnfit`], naming the effect.
pub fn check_links(
    env: &ProcessEnv,
    performed: &PerformedEffects,
    owned: &OwnedOperations,
    fits: &BTreeMap<EffectName, FitPolicy>,
    served: &BTreeMap<EffectName, lash_protocol_types::ToolDefinition>,
) -> Result<(), ProcessEnvRefusal> {
    if let Some(effect) = performed
        .keys()
        .find(|effect| !env.effects().contains_key(*effect))
    {
        return Err(ProcessEnvRefusal::EffectNotLinked {
            effect: effect.to_string(),
        });
    }
    if let Some(effect) = env
        .effects()
        .keys()
        .find(|effect| !performed.contains_key(*effect))
    {
        return Err(ProcessEnvRefusal::EffectNotPerformed {
            effect: effect.to_string(),
        });
    }
    for (effect, expected) in performed {
        let link = &env.effects()[effect];
        let Some(definition) = served.get(effect) else {
            return Err(ProcessEnvRefusal::ToolNotServed {
                effect: effect.to_string(),
                tool: link.tool.to_string(),
                provider: format!("{}/{}", link.provider.owner(), link.provider.key()),
            });
        };
        if let Some(owner) = owned.get(effect) {
            check_runner(effect, owner, &definition.manifest, &definition.contract)?;
        }
        let live = definition.contract_id();
        if live != expected.contract {
            return Err(ProcessEnvRefusal::ContractChanged {
                effect: effect.to_string(),
                tool: link.tool.to_string(),
                pinned: expected.contract.clone(),
                live,
            });
        }
        let expected = &expected.signature;
        let provided = lash_protocol_types::tool_signature(
            &definition.contract.input_schema.canonical,
            &definition.contract.output_schema.canonical,
        );
        let fit = fits.get(effect).copied().unwrap_or_default();
        let fits = match fit {
            FitPolicy::Compatible => lash_kernel_check::signature_serves(&provided, expected),
            FitPolicy::Exact => provided == *expected,
        };
        if !fits {
            return Err(ProcessEnvRefusal::EffectUnfit {
                effect: effect.to_string(),
                tool: link.tool.to_string(),
                reason: match fit {
                    FitPolicy::Compatible => format!(
                        "its signature {provided:?} does not serve the effect's {expected:?}"
                    ),
                    FitPolicy::Exact => format!(
                        "its signature {provided:?} is not exactly the effect's {expected:?}"
                    ),
                },
            });
        }
    }
    Ok(())
}

/// PKG-RUNNER: on a path a package owns, the tool `manifest` describes,
/// under its schema `contract`, must carry
/// [`lash_protocol_types::ContractSource::Declared`] with the package's
/// contract id, declare exactly the package's intents and serve its kernel
/// signature.
///
/// # Errors
///
/// [`ProcessEnvRefusal::RunnerNotPackaged`], naming the tool and the
/// package.
pub fn check_runner(
    effect: &EffectName,
    owner: &crate::OwnedOperation,
    manifest: &lash_protocol_types::ToolManifest,
    contract: &lash_protocol_types::ToolContract,
) -> Result<(), ProcessEnvRefusal> {
    let refuse = |reason: String| ProcessEnvRefusal::RunnerNotPackaged {
        effect: effect.to_string(),
        tool: manifest.id.to_string(),
        package: owner.package.clone(),
        reason,
    };
    match &manifest.contract {
        lash_protocol_types::ContractSource::Declared { id }
            if *id == owner.requirement.contract => {}
        lash_protocol_types::ContractSource::Declared { id } => {
            return Err(refuse(format!(
                "it declares contract `{id}`, and the package declares `{}`",
                owner.requirement.contract
            )));
        }
        lash_protocol_types::ContractSource::Derived { .. } => {
            return Err(refuse(format!(
                "it derives its contract id, and the package declares `{}`",
                owner.requirement.contract
            )));
        }
    }
    let intents = manifest
        .declaration()
        .intents
        .iter()
        .map(|intent| intent.as_str().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    if intents != owner.intents {
        return Err(refuse(format!(
            "it declares the intents {intents:?}, and the package declares {:?}",
            owner.intents
        )));
    }
    let provided = lash_protocol_types::tool_signature(
        &contract.input_schema.canonical,
        &contract.output_schema.canonical,
    );
    if !lash_kernel_check::signature_serves(&provided, &owner.requirement.signature) {
        return Err(refuse(format!(
            "its signature {provided:?} does not serve the package's {:?}",
            owner.requirement.signature
        )));
    }
    Ok(())
}
