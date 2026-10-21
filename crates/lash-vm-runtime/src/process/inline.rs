//! A function a start carries inline (FIG-6382): the kernel engine's
//! [`lash_execution_contracts::process::InlineStarts`].
//!
//! The payload is a function value as a tool receives it,
//! `{"$lash_function": <FunctionValue>}`. The tag carries a value, not
//! authority: it is decoded under the standard bounds, its document checked
//! against the library of the package set its starter linked it under
//! (FIG-6993) and its entry admitted before anything is stored. Realization then publishes the document and the declarations the
//! store lacks through the one document writer, under the start's claim; the
//! caller publishes the descriptor beside them.

use std::sync::Arc;

use lash_kernel_check::{Environment, WorkflowDocument};
use lash_kernel_doc::FunctionValue;
use lash_kernel_doc::FunctionValueLimits;
use lash_kernel_doc::IdentifiedDocument;
use lash_kernel_doc::WorkflowDocumentEntry;
use lash_vocabulary::data_limits::KernelDataLimits;

use super::{KernelProcessDefinition, KernelProcessEngine, definition_draft, entry_signature};

pub(crate) struct KernelInlineStarts {
    pub(crate) engine: Arc<KernelProcessEngine>,
}

fn refused(message: impl std::fmt::Display) -> lash_execution_contracts::plugin_error::PluginError {
    lash_execution_contracts::plugin_error::PluginError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::DefinitionRefused,
        format!("the function carried inline is refused: {message}"),
    ))
}

impl KernelInlineStarts {
    /// The checked function value and its admitted document, with what a
    /// start of it, linked under `packages`, runs, and the registry it is
    /// checked against: that set's, read from the store where this node
    /// does not hold it, so every node checks it alike (FIG-6993).
    async fn check(
        &self,
        payload: &serde_json::Value,
        packages: lash_kernel_doc::PackageSetDigest,
    ) -> Result<
        (
            IdentifiedDocument,
            lash_execution_contracts::process::InlineDefinition,
            Arc<lash_kernel_doc::FunctionRegistry>,
        ),
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let engine = &self.engine;
        let (surface, functions) = engine.linked_packages(packages).await.map_err(|message| {
            lash_execution_contracts::plugin_error::PluginError::Session(format!(
                "the package surface a start is linked under is not read: {message}"
            ))
        })?;
        let value = FunctionValue::from_transport_json(
            payload,
            functions.as_ref(),
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
            .effects(identity, identified.document(), &functions, &value.entry)
            .map_err(refused)?;
        // The entry is admitted against exactly the effects it performs:
        // the launch it is started under links them, which realization
        // checks against the held environment.
        let mut environment = Environment::new(functions.as_ref());
        environment.effects.clone_from(&performed);
        let document = WorkflowDocument::admit(
            identified.document().clone(),
            WorkflowDocumentEntry::Entry {
                function: value.entry.clone(),
            },
            &environment,
        )
        .map_err(refused)?;
        drop(environment);
        let Some(signature) = document.entry_signature().map(entry_signature) else {
            return Err(refused(format!("it has no entry `{}`", value.entry)));
        };
        let draft = definition_draft(&KernelProcessDefinition::of(
            identified.document(),
            identity,
            value.entry,
            packages,
            surface.selection(),
        ))
        .map_err(refused)?;
        Ok((
            identified,
            lash_execution_contracts::process::InlineDefinition {
                draft,
                signature,
                performed,
            },
            functions,
        ))
    }
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::InlineStarts for KernelInlineStarts {
    async fn inspect(
        &self,
        payload: &serde_json::Value,
        packages: lash_kernel_doc::PackageSetDigest,
    ) -> Result<
        lash_execution_contracts::process::InlineDefinition,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.check(payload, packages)
            .await
            .map(|(_, inline, _)| inline)
    }

    async fn realize(
        &self,
        claim: &lash_core::ReferrerClaim,
        payload: &serde_json::Value,
        packages: lash_kernel_doc::PackageSetDigest,
    ) -> Result<
        lash_execution_contracts::process::InlineDefinition,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let (document, inline, functions) = self.check(payload, packages).await?;
        let declarations =
            lash_kernel_doc::CheckedDeclarations::required(document.document(), functions.as_ref())
                .map_err(refused)?;
        self.engine
            .documents
            .publish(claim, &document, &declarations)
            .await
            .map_err(super::workflow::store_fault)?;
        Ok(inline)
    }
}
