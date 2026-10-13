//! P1: the link check at the start hold point. A process is the pair of a
//! template its engine runs and the env it starts under; the pair links only
//! when the env maps exactly the effects the template performs, each to a
//! tool that fits the effect's kernel signature and that an env component
//! serves with the surface the link retained.

use std::collections::BTreeMap;

use lash_kernel_doc::EffectName;
use serde::{Deserialize, Serialize};

use crate::{LinkKind, PerformedEffects, ProcessEnv, ProcessEnvRefusal};

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
/// tool that fits it under its policy in `fits` (`Compatible` when absent).
/// The `call_dynamic` endpoint is linked only at its own path, as a
/// `Dynamic` link retaining its own surface, and fits like any tool: the
/// document is checked against its known signature, and its target is
/// matched per call.
///
/// # Errors
///
/// [`ProcessEnvRefusal::EffectNotLinked`],
/// [`ProcessEnvRefusal::EffectNotPerformed`] or
/// [`ProcessEnvRefusal::EffectUnfit`], naming the effect.
pub fn check_links(
    env: &ProcessEnv,
    performed: &PerformedEffects,
    fits: &BTreeMap<EffectName, FitPolicy>,
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
        let at_endpoint = effect.as_str() == crate::CALL_DYNAMIC_EFFECT;
        let endpoint = link.kind == LinkKind::Dynamic
            && link.surface == crate::ProcessSurface::of(&crate::call_dynamic_endpoint());
        if (link.kind == LinkKind::Dynamic) != at_endpoint || (at_endpoint && !endpoint) {
            return Err(ProcessEnvRefusal::EffectUnfit {
                effect: effect.to_string(),
                tool: link.tool().to_string(),
                reason: format!(
                    "the call_dynamic endpoint is linked only at `{}`, as a dynamic link of \
                     its own surface",
                    crate::CALL_DYNAMIC_EFFECT
                ),
            });
        }
        let provided = lash_kernel_check::tool_signature(
            link.surface.input_schema.canonical(),
            link.surface.output_schema.canonical(),
        );
        let fit = fits.get(effect).copied().unwrap_or_default();
        let fits = match fit {
            FitPolicy::Compatible => lash_kernel_check::signature_serves(&provided, expected),
            FitPolicy::Exact => provided == *expected,
        };
        if !fits {
            return Err(ProcessEnvRefusal::EffectUnfit {
                effect: effect.to_string(),
                tool: link.tool().to_string(),
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
