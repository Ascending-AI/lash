//! Copy immutable per-use outcomes under an explicit successor correspondence; never resolve during adoption.
use super::state::Phase;
use super::{KernelProcessEngine, KernelProcessInput, advance};
use lash_core_store::resolutions::ResolutionRecord;
use lash_execution_contracts::process::{
    EngineState, EngineSuccession, SuccessionContext, SuccessionError, SuccessorCarry,
    UseCorrespondence,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
pub(crate) struct KernelSuccession {
    pub(crate) engine: Arc<KernelProcessEngine>,
}
fn refuse(message: impl Into<String>) -> SuccessionError {
    let message = message.into();
    SuccessionError::Refused {
        refusal: serde_json::json!({"kind":"incompatible_succession","message":message}),
        message,
    }
}
fn typed_refusal(cause: impl serde::Serialize + std::fmt::Display) -> SuccessionError {
    let message = cause.to_string();
    match serde_json::to_value(cause) {
        Ok(refusal) => SuccessionError::Refused { refusal, message },
        Err(error) => SuccessionError::Unavailable(error.to_string()),
    }
}

#[async_trait::async_trait]
impl EngineSuccession for KernelSuccession {
    async fn carry(
        &self,
        process: &lash_core::ProcessId,
        context: &SuccessionContext,
        state: Option<&EngineState>,
    ) -> Result<SuccessorCarry, SuccessionError> {
        let mut content = self
            .engine
            .documents
            .load_linked(context.from.link)
            .await
            .map_err(|e| SuccessionError::Unavailable(e.to_string()))?;
        let next = self
            .engine
            .documents
            .load_linked(context.to.link)
            .await
            .map_err(|e| SuccessionError::Unavailable(e.to_string()))?;
        content.documents.extend(next.documents);
        content.links.extend(next.links);
        self.engine
            .documents
            .inspect(
                &content,
                context.to.link,
                &super::documents::selection(&context.to.selector),
            )
            .map_err(|e| refuse(e.to_string()))?;
        for callable in &context.predecessor.retained_code {
            if !content.links.contains_key(&callable.link) {
                let held = self
                    .engine
                    .documents
                    .load_linked(callable.link)
                    .await
                    .map_err(|error| SuccessionError::Unavailable(error.to_string()))?;
                content.documents.extend(held.documents);
                content.links.extend(held.links);
                content.packages.extend(held.packages);
            }
        }
        let live = context
            .predecessor
            .uses
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let adopted = super::helpers::plan_adoption(
            &content,
            context.from.link,
            context.to.link,
            &live,
            &context.resolutions,
            &BTreeMap::new(),
        )
        .map_err(|e| refuse(e.to_string()))?;
        let mut requirements = context.environment.requirements.clone();
        for outcome in adopted.resolutions.values() {
            if let lash_core_store::resolutions::ResolutionEntry::Resolved {
                requirements: target,
                ..
            } = outcome
            {
                requirements.extend(target);
            }
        }
        let mut predecessor_code = BTreeSet::new();
        let mut encoded = None;
        let mut own_formats = false;
        if let Some(state) = state {
            let (mut state, _written) =
                advance::decode(self.engine.writes, state).map_err(|e| refuse(e.to_string()))?;
            if matches!(state.phase, Phase::Running { .. } | Phase::Ended) {
                return Ok(SuccessorCarry::InFlight);
            }
            let input = KernelProcessInput::from_payload(&state.payload)
                .map_err(|e| refuse(e.to_string()))?;
            let from = KernelProcessInput::from_payload(&context.from_payload)
                .map_err(|e| refuse(e.to_string()))?;
            let to = KernelProcessInput::from_payload(&context.to_payload)
                .map_err(|e| refuse(e.to_string()))?;
            if input != from {
                return Err(refuse("state is not the predecessor payload"));
            }
            match (&from.code, &to.code) {
                (
                    super::InlineRequest::Callable {
                        function: before,
                        origin: prior,
                        ..
                    },
                    super::InlineRequest::Callable {
                        function: after,
                        origin: next,
                        ..
                    },
                ) if before == after && prior == next => {}
                (super::InlineRequest::Linked { .. }, super::InlineRequest::Linked { .. }) => {}
                _ => {
                    return Err(refuse(
                        "succession must preserve a callable's exact defining Link and frozen captures",
                    ));
                }
            }
            let before = content
                .document(
                    content
                        .link(context.from.link)
                        .map_err(|e| refuse(e.to_string()))?
                        .document,
                )
                .map_err(|e| refuse(e.to_string()))?;
            let after = content
                .document(
                    content
                        .link(context.to.link)
                        .map_err(|e| refuse(e.to_string()))?
                        .document,
                )
                .map_err(|e| refuse(e.to_string()))?;
            let rewritten = if before.kernel() == after.kernel() {
                lash_kernel_migrate::Rewritten {
                    document: after.clone(),
                    correspondence: lash_kernel_migrate::unchanged(before, after)
                        .map_err(typed_refusal)?,
                    functions: before
                        .functions
                        .keys()
                        .filter(|id| after.functions.contains_key(id))
                        .map(|id| (*id, *id))
                        .collect(),
                }
            } else {
                let plan = super::plan_migration(before, &self.engine.functions)
                    .map_err(typed_refusal)?
                    .ok_or_else(|| refuse("no format correspondence"))?;
                if plan.rewritten.document != *after {
                    return Err(refuse(
                        "successor is not the proved format rewrite of the pinned document",
                    ));
                }
                plan.rewritten
            };
            let (staged, debt) = self
                .engine
                .carry_pages(
                    process,
                    &mut state,
                    &rewritten,
                    &to.code,
                    &context.predecessor.retained_code,
                    &UseCorrespondence {
                        uses: adopted.uses.clone(),
                    },
                )
                .await
                .map_err(|error| match error {
                    lash_kernel_migrate::CarryFault::Refused(refusal) => typed_refusal(refusal),
                    lash_kernel_migrate::CarryFault::Heap(error) => {
                        SuccessionError::Unavailable(error.to_string())
                    }
                })?;
            predecessor_code = debt;
            requirements.machine.extend(&state.requirements.machine);
            state.payload = context.to_payload.clone();
            let mut carried =
                advance::encode(&state, self.engine.writes).map_err(|e| refuse(e.to_string()))?;
            carried.staged = staged;
            encoded = Some(Box::new(carried));
            own_formats = after.kernel() == self.engine.writes.number();
        }
        let inspection = &context.to;
        let resolutions = adopted
            .resolutions
            .into_iter()
            .map(|(use_, entry)| {
                let requested = content
                    .links
                    .get(&use_.link)
                    .and_then(|link| content.documents.get(&link.document))
                    .and_then(|document| document.imports.get(&use_.slot))
                    .map(|import| import.name.clone())
                    .ok_or_else(|| refuse("adopted use has no Import"))?;
                Ok(ResolutionRecord {
                    key: lash_core_store::resolutions::ResolutionKey {
                        execution: context.execution.clone(),
                        use_,
                    },
                    requested,
                    entry,
                })
            })
            .collect::<Result<Vec<_>, SuccessionError>>()?;
        Ok(SuccessorCarry::Carried {
            state: encoded,
            own_formats,
            correspondence: UseCorrespondence { uses: adopted.uses },
            predecessor_code,
            resolutions,
            requirements: Box::new(requirements),
            artifacts: inspection.artifacts.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// FIG-6447: the successor boundary retains owned typed causes, replacing
    /// the retired automatic-migration park wrapper's round-trip laws.
    #[test]
    fn a_successor_refusal_keeps_its_typed_origin() {
        let cause = lash_kernel_migrate::KernelMigrationRefusal::Parked {
            document: lash_kernel_value::DocumentId::from_bytes([1; 32]),
            refusal: lash_kernel_migrate::ParkedRefusal::SiteNotCarried {
                site: lash_kernel_value::sites::Site::new(
                    lash_kernel_value::sites::Unit::Main,
                    vec![3],
                ),
                held: "spawn origin".into(),
            },
        };
        let SuccessionError::Refused { refusal, .. } = typed_refusal(cause.clone()) else {
            panic!("a semantic refusal")
        };
        assert_eq!(
            serde_json::from_value::<lash_kernel_migrate::KernelMigrationRefusal>(refusal).unwrap(),
            cause
        );
    }
}
