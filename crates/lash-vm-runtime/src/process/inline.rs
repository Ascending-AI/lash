//! A function a start carries inline (FIG-6382): the kernel engine's
//! [`lash_core::InlineStarts`].
//!
//! The payload is a function value as a tool receives it,
//! `{"$lash_function": <FunctionValue>}`. The tag carries a value, not
//! authority: it is decoded under the standard bounds, its document checked
//! against this engine's library and its entry admitted before anything is
//! stored. Realization then publishes the document and the declarations the
//! store lacks through the one document writer, under the start's claim; the
//! caller publishes the descriptor beside them.

use std::sync::Arc;

use lash_kernel_check::{Environment, WorkflowDocument};
use lash_kernel_doc::{
    FunctionValue, FunctionValueLimits, IdentifiedDocument, KernelDataLimits, WorkflowDocumentEntry,
};

use super::{KernelProcessDefinition, KernelProcessEngine, definition_draft, entry_signature};

pub(crate) struct KernelInlineStarts {
    pub(crate) engine: Arc<KernelProcessEngine>,
}

fn refused(message: impl std::fmt::Display) -> lash_core::PluginError {
    lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::DefinitionRefused,
        format!("the function carried inline is refused: {message}"),
    ))
}

impl KernelInlineStarts {
    /// The checked function value and its admitted document, with what a
    /// start of it runs.
    fn check(
        &self,
        payload: &serde_json::Value,
    ) -> Result<(IdentifiedDocument, lash_core::InlineDefinition), lash_core::PluginError> {
        let engine = &self.engine;
        let value = FunctionValue::from_transport_json(
            payload,
            engine.functions.as_ref(),
            FunctionValueLimits::standard(),
            KernelDataLimits::standard(),
        )
        .map_err(refused)?;
        let identified = IdentifiedDocument::new(value.document);
        let identity = identified.id();
        let performed = engine
            .effect_preparations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effects(
                identity,
                identified.document(),
                &engine.functions,
                &value.entry,
            )
            .map_err(refused)?;
        // The entry is admitted against exactly the effects it performs:
        // the launch it is started under links them, which realization
        // checks against the held environment.
        let mut environment = Environment::new(engine.functions.as_ref());
        environment.effects.clone_from(&performed);
        let document = WorkflowDocument::admit(
            identified.document().clone(),
            WorkflowDocumentEntry::Entry {
                function: value.entry.clone(),
            },
            &environment,
        )
        .map_err(refused)?;
        let Some(signature) = document.entry_signature().map(entry_signature) else {
            return Err(refused(format!("it has no entry `{}`", value.entry)));
        };
        let draft = definition_draft(&KernelProcessDefinition::of(
            identified.document(),
            identity,
            value.entry,
        ))
        .map_err(refused)?;
        Ok((
            identified,
            lash_core::InlineDefinition {
                draft,
                signature,
                performed,
            },
        ))
    }
}

#[async_trait::async_trait]
impl lash_core::InlineStarts for KernelInlineStarts {
    async fn inspect(
        &self,
        payload: &serde_json::Value,
    ) -> Result<lash_core::InlineDefinition, lash_core::PluginError> {
        self.check(payload).map(|(_, inline)| inline)
    }

    async fn realize(
        &self,
        claim: &lash_core::ReferrerClaim,
        payload: &serde_json::Value,
    ) -> Result<lash_core::InlineDefinition, lash_core::PluginError> {
        let (document, inline) = self.check(payload)?;
        let declarations = lash_kernel_doc::CheckedDeclarations::required(
            document.document(),
            self.engine.functions.as_ref(),
        )
        .map_err(refused)?;
        self.engine
            .documents
            .publish(claim, &document, &declarations)
            .await
            .map_err(super::workflow::store_fault)?;
        Ok(inline)
    }
}
