//! Copy immutable per-use outcomes under an explicit successor correspondence; never resolve during adoption.
use super::state::Phase;
use super::{KernelProcessEngine, KernelProcessInput, advance};
use lash_core_store::resolutions::ResolutionRecord;
use lash_execution_contracts::process::{
    EngineState, EngineSuccession, SuccessionContext, SuccessionError, SuccessorCarry,
    UseCorrespondence,
};
use lash_vocabulary::names::*;
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
        content
            .inspect(
                context.to.link,
                &lash_kernel_package::ReachSelection::Start(context.to.selector.clone()),
            )
            .map_err(|e| refuse(e.to_string()))?;
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
        let mut encoded = None;
        let mut own_formats = false;
        if let Some(state) = state {
            let (mut state, written) =
                advance::decode(self.engine.writes, state).map_err(|e| refuse(e.to_string()))?;
            if written != self.engine.writes
                || matches!(state.phase, Phase::Running { .. } | Phase::Ended)
            {
                return Ok(SuccessorCarry::InFlight);
            }
            let input = KernelProcessInput::from_payload(&state.payload)
                .map_err(|e| refuse(e.to_string()))?;
            if input.code
                != (super::InlineRequest::Linked {
                    start: context.from.clone(),
                })
            {
                return Err(refuse("state is not the predecessor start"));
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
            let correspondence =
                lash_kernel_migrate::unchanged(before, after).map_err(|e| refuse(e.to_string()))?;
            let rewritten = lash_kernel_migrate::Rewritten {
                document: after.clone(),
                correspondence,
                functions: before
                    .functions
                    .keys()
                    .filter(|id| after.functions.contains_key(id))
                    .map(|id| (*id, *id))
                    .collect(),
            };
            let staged = self
                .engine
                .carry_pages(process, &mut state, &rewritten, Some(&context.to), None)
                .await
                .map_err(|e| refuse(e.to_string()))?;
            state.payload = serde_json::to_value(KernelProcessInput {
                code: super::InlineRequest::Linked {
                    start: context.to.clone(),
                },
                args: input.args,
            })
            .map_err(|e| refuse(e.to_string()))?;
            let mut carried =
                advance::encode(&state, self.engine.writes).map_err(|e| refuse(e.to_string()))?;
            carried.staged = staged;
            encoded = Some(Box::new(carried));
            own_formats = true;
        }
        let inspection = self
            .engine
            .documents
            .inspect_link(
                context.to.link,
                &lash_execution_contracts::process::linking::ReachSelector::Start(
                    context.to.selector.clone(),
                ),
            )
            .await
            .map_err(|e| SuccessionError::Unavailable(e.to_string()))?;
        let resolutions = adopted
            .resolutions
            .into_iter()
            .map(|(use_, entry)| {
                let requested = context
                    .environment
                    .uses
                    .keys()
                    .find(|old| adopted.uses.get(old) == Some(&use_))
                    .and_then(|old| {
                        content
                            .links
                            .get(&old.link)
                            .and_then(|link| content.documents.get(&link.document))
                            .and_then(|document| document.imports.get(&old.slot))
                    })
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
            resolutions,
            requirements: Box::new(requirements),
            artifacts: inspection.artifacts,
        })
    }
}
