//! A kernel process carried onto a successor pair its host names
//! (FIG-6121, D2).
//!
//! The successor's payload names the same entry and arguments over another
//! document. Each library function the process's document lists and the
//! successor's does not is relinked to the successor's function of the same
//! name; [`plan_carry`] rewrites the document over that relink, and the
//! rewrite must be the successor's document, or no correspondence carries
//! the process. A parked run is refused, typed: no migrator carries its
//! pages (FIG-6233). The waits the state holds move to the identities
//! the rewrite gives their effects. Their admissions
//! keep the requests they recorded, so a step already admitted keeps its
//! call and the match its admission recorded, and nothing is re-matched. A
//! successor of the same document carries the state unchanged: only the env
//! moves.

use std::collections::BTreeMap;

use lash_core::ProcessId;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::SuccessionError;
use lash_execution_contracts::process::SuccessorCarry;
use lash_kernel_doc::{FunctionId, FunctionName};
use lash_kernel_migrate::Relink;

use super::helpers::{HelperAdoptionRefusal, plan_carry};
use super::state::{KernelProcessInput, Phase};
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
impl lash_execution_contracts::process::EngineSuccession for KernelSuccession {
    async fn carry(
        &self,
        process: &ProcessId,
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
        // Both documents' code, as the store keeps what this node does not
        // hold, declared through the loader (FIG-6985): adoption reads it
        // as rebasing does, under its own policy.
        let unread = |message: String| refused(&HelperAdoptionRefusal::Unread { message });
        let mut stored = BTreeMap::new();
        for document in [&base, &next] {
            match documents
                .stored_definitions(document, &engine.functions)
                .await
                .map_err(|error| SuccessionError::Unavailable(error.to_string()))?
            {
                Ok(found) => stored.extend(found),
                Err(unretained) => return Err(unread(format!("{unretained:?}"))),
            }
        }
        let functions = lash_vm_broker::kernel::loader::declare(
            base.manifest
                .functions
                .keys()
                .chain(next.manifest.functions.keys())
                .copied(),
            base.manifest.kernel,
            &engine.functions,
            &stored,
        )
        .map_err(|refusal| unread(refusal.to_string()))?;
        let relink = relink(&base, &next, &functions).map_err(|refusal| refused(&refusal))?;
        let carried = plan_carry(&base, state.parked.is_some(), &functions, &relink)
            .map_err(|refusal| refused(&refusal))?;
        if carried.rewritten.identity() != successor.document {
            return Err(refused(&HelperAdoptionRefusal::NoCorrespondence {
                document: input.document,
                successor: successor.document,
            }));
        }
        // The process holds the record of the rewrite beside the successor's
        // document, so a host rekeys a map it holds of the earlier one.
        let claim = lash_core::ReferrerClaim::unguarded(
            lash_core::ArtifactReferrer::ProcessRecord(process.clone()),
        )
        .map_err(|error| SuccessionError::Unavailable(error.to_string()))?;
        documents
            .record_rewrite(&claim, &carried.rewritten.correspondence)
            .await
            .map_err(|error| SuccessionError::Unavailable(error.to_string()))?;
        // A state that stands on no run holds no wait: every wait is a
        // park's, and a parked run was refused above.
        advance::encode(&state, engine.writes)
            .map(|state| SuccessorCarry::Carried {
                state: Some(state),
                own_formats: true,
            })
            .map_err(undecodable)
    }
}

/// Each library function `base` lists and `next` does not, with the
/// function `next` lists under the same name, if it lists one.
fn relink(
    base: &lash_kernel_doc::Document,
    next: &lash_kernel_doc::Document,
    functions: &lash_kernel_doc::FunctionRegistry,
) -> Result<Relink, HelperAdoptionRefusal> {
    let by_name: BTreeMap<&FunctionName, FunctionId> = next
        .manifest
        .functions
        .iter()
        .map(|(function, name)| (name, *function))
        .collect();
    let mut relink = Relink::new();
    for (function, name) in &base.manifest.functions {
        if next.manifest.functions.contains_key(function) {
            continue;
        }
        let counterpart = by_name.get(name).copied();
        if let Some(counterpart) = counterpart
            && functions.get(&counterpart).is_none()
        {
            return Err(HelperAdoptionRefusal::State {
                message: format!("library function {counterpart} is not held"),
            });
        }
        relink.insert(*function, counterpart);
    }
    Ok(relink)
}
