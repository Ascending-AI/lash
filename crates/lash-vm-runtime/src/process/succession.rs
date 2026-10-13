//! A kernel process carried onto a successor pair its host names
//! (FIG-6121, D2).
//!
//! The successor's payload names the same entry and arguments over another
//! document. Each library function the process's document lists and the
//! successor's does not is replaced by the successor's function of the same
//! name; [`plan_carry`] rewrites the document over those replacements and
//! carries the parked run onto it, and the rewrite must be the successor's
//! document, or no correspondence carries the run. The waits the run holds
//! move to the identities the rewrite gives their effects. Their admissions
//! keep the requests they recorded, so a step already admitted keeps its
//! call and the match its admission recorded, and nothing is re-matched. A
//! successor of the same document carries the state unchanged: only the env
//! moves.

use std::collections::BTreeMap;

use lash_core::{EngineState, ProcessId, SuccessionError, SuccessorCarry};
use lash_kernel_doc::{FunctionId, FunctionName};
use lash_kernel_state::AdmittedWait;

use super::helpers::{HelperAdoptionRefusal, plan_carry};
use super::state::{KernelProcessInput, Phase, SealedRun, Wait};
use super::{KernelProcessEngine, advance};

/// How the kernel engine carries a process onto a successor pair.
pub(crate) struct KernelSuccession {
    pub(crate) engine: std::sync::Arc<KernelProcessEngine>,
}

fn refused(refusal: &HelperAdoptionRefusal) -> SuccessionError {
    SuccessionError::Refused {
        refusal: serde_json::to_value(refusal).unwrap_or(serde_json::Value::Null),
        message: refusal.to_string(),
    }
}

fn undecodable(message: impl std::fmt::Display) -> SuccessionError {
    refused(&HelperAdoptionRefusal::State {
        message: message.to_string(),
    })
}

#[async_trait::async_trait]
impl lash_core::EngineSuccession for KernelSuccession {
    async fn carry(
        &self,
        _process: &ProcessId,
        from: &serde_json::Value,
        to: &serde_json::Value,
        state: Option<&EngineState>,
    ) -> Result<SuccessorCarry, SuccessionError> {
        let engine = &self.engine;
        let decoded = state
            .map(|state| advance::decode(engine.writes, state))
            .transpose()
            .map_err(undecodable)?;
        if let Some((state, written)) = &decoded {
            // A state of an earlier kernel version is carried to this one
            // first; a run in flight is carried at its next park.
            if *written != engine.writes
                || matches!(state.phase, Phase::Running { .. } | Phase::Ended)
            {
                return Ok(SuccessorCarry::InFlight);
            }
        }
        let payload = decoded.as_ref().map_or(from, |(state, _)| &state.payload);
        let input = KernelProcessInput::from_payload(payload).map_err(undecodable)?;
        let successor = KernelProcessInput::from_payload(to).map_err(undecodable)?;
        if successor.entry != input.entry || successor.args != input.args {
            return Err(refused(&HelperAdoptionRefusal::OtherEntry {
                entry: successor.entry,
            }));
        }
        let Some((mut state, _)) = decoded else {
            // Nothing ran yet: the successor's document is admitted where it
            // was published, and the start reads it.
            return Ok(SuccessorCarry::Carried {
                state: None,
                own_formats: false,
            });
        };
        state.payload = to.clone();
        if successor.document == input.document {
            return advance::encode(&state, engine.writes)
                .map(|state| SuccessorCarry::Carried {
                    state: Some(state),
                    own_formats: false,
                })
                .map_err(undecodable);
        }
        let documents = &engine.documents;
        let read = |document| async move {
            documents
                .get(&document)
                .await
                .map_err(|error| SuccessionError::Unavailable(error.to_string()))?
                .ok_or_else(|| refused(&HelperAdoptionRefusal::DocumentMissing { document }))
        };
        let base = read(input.document).await?;
        let next = read(successor.document).await?;
        let replacements =
            replacements(&base, &next, &engine.functions).map_err(|refusal| refused(&refusal))?;
        let whole = state
            .parked
            .as_ref()
            .map(super::migrate::unsealed)
            .transpose()
            .map_err(undecodable)?;
        let admitted: Vec<AdmittedWait> = state
            .admissions
            .values()
            .map(|admission| admission.0.clone())
            .collect();
        let carried = plan_carry(
            &base,
            whole.as_ref().map(|whole| (whole, admitted.as_slice())),
            &engine.functions,
            &replacements,
            engine.workers.config().graph_reads.execution(),
        )
        .map_err(|refusal| refused(&refusal))?;
        if carried.rewritten.identity() != successor.document {
            return Err(refused(&HelperAdoptionRefusal::NoCorrespondence {
                document: input.document,
                successor: successor.document,
            }));
        }
        let identify = |identity: &mut lash_kernel_doc::EffectIdentity| {
            carried
                .rewritten
                .effect_identity(&base, identity)
                .map(|moved| *identity = moved)
                .map_err(|refusal| {
                    refused(&HelperAdoptionRefusal::Parked {
                        document: carried.from,
                        refusal,
                    })
                })
        };
        for wait in state.waits.values_mut() {
            let (Wait::Step { identity, .. } | Wait::Timer { identity, .. }) = wait;
            identify(identity)?;
        }
        for delivery in &mut state.settled {
            identify(&mut delivery.identity)?;
        }
        for admission in state.admissions.values_mut() {
            identify(&mut admission.0.identity)?;
        }
        let owner = state.parked.as_ref().map(|parked| parked.0.owner().clone());
        state.parked = match (carried.parked, owner) {
            (Some(run), Some(owner)) => Some(SealedRun(
                run.sealed(owner, &carried.rewritten.identity().to_string()),
            )),
            _ => None,
        };
        // The run now pins the build's own helper release.
        advance::encode(&state, engine.writes)
            .map(|state| SuccessorCarry::Carried {
                state: Some(state),
                own_formats: true,
            })
            .map_err(undecodable)
    }
}

/// Each library function `base` lists and `next` does not, with the
/// function `next` lists under the same name.
fn replacements(
    base: &lash_kernel_doc::Document,
    next: &lash_kernel_doc::Document,
    functions: &lash_kernel_doc::FunctionRegistry,
) -> Result<BTreeMap<FunctionId, FunctionId>, HelperAdoptionRefusal> {
    let by_name: BTreeMap<&FunctionName, FunctionId> = next
        .manifest
        .functions
        .iter()
        .map(|(function, name)| (name, *function))
        .collect();
    let mut replacements = BTreeMap::new();
    for (function, name) in &base.manifest.functions {
        if next.manifest.functions.contains_key(function) {
            continue;
        }
        let Some(counterpart) = by_name.get(name).copied() else {
            return Err(HelperAdoptionRefusal::NoCounterpart {
                function: *function,
                name: name.clone(),
            });
        };
        if functions.get(&counterpart).is_none() {
            return Err(HelperAdoptionRefusal::State {
                message: format!("helper {counterpart} is not held"),
            });
        }
        replacements.insert(*function, counterpart);
    }
    Ok(replacements)
}
