//! Inline source fulfils once; callable starts retain their defining code and captures.
use super::{KernelProcessDefinition, KernelProcessEngine, definition_draft, entry_signature};
use lash_execution_contracts::process::linking::*;
use lash_execution_contracts::process::{InlineCode, InlineDefinition, InlineStarts};
use lash_kernel_doc::FunctionTarget;

use lash_kernel_package::{LinkedContent, LinkedReach, ReachSelection, RetainedContent};
use lash_vocabulary::{names::*, package::*};
use std::{collections::BTreeSet, sync::Arc};

impl super::InlineRequest {
    pub(super) fn target(&self) -> Result<lash_kernel_vm::Target, String> {
        Ok(match self {
            Self::Linked { start } => match &start.selector {
                StartSelector::Main => lash_kernel_vm::Target::Main,
                StartSelector::Export(name) => lash_kernel_vm::Target::Entry(name.clone()),
            },
            Self::Callable { function, .. } => lash_kernel_vm::Target::Callable(function.clone()),
            Self::Unlinked { .. } => return Err("unrealized process input".into()),
        })
    }

    /// Encode kernel input behind the neutral engine boundary without losing captures.
    pub fn into_request(
        self,
        snapshot: NamespaceSnapshot,
        context: PublicationContext,
        attachment: Attachment,
    ) -> Result<lash_execution_contracts::process::InlineRequest, EnginePortError> {
        let payload = serde_json::to_value(self).map_err(|error| invalid(error.to_string()))?;
        Ok(lash_execution_contracts::process::InlineRequest {
            code: InlineCode::Engine { payload },
            snapshot,
            context,
            attachment,
        })
    }
}
fn invalid(message: impl Into<String>) -> EnginePortError {
    EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
        message: message.into(),
    })
}
fn decoded(
    request: &lash_execution_contracts::process::InlineRequest,
) -> Result<super::InlineRequest, EnginePortError> {
    Ok(match &request.code {
        InlineCode::Engine { payload } => {
            serde_json::from_value(payload.clone()).map_err(|error| invalid(error.to_string()))?
        }
        InlineCode::Linked { start } => super::InlineRequest::Linked {
            start: start.clone(),
        },
        InlineCode::Unlinked { document, selector } => super::InlineRequest::Unlinked {
            document: *document,
            selector: selector.clone(),
        },
    })
}
pub(super) struct LoadedExecution {
    pub content: LinkedContent,
    pub reach: LinkedReach,
    pub inspection: LinkInspection,
}
impl KernelProcessEngine {
    /// Load every captured defining instance, then derive the exact code and helper reach.
    pub(super) async fn load_execution(
        &self,
        code: &super::InlineRequest,
        retained: &BTreeSet<CallableId>,
    ) -> Result<LoadedExecution, EnginePortError> {
        let (root, selector) = code
            .linked()
            .ok_or_else(|| invalid("unlinked code was not realized"))?;
        let mut content = self.documents.load_linked(root).await?;
        let mut callables = Vec::new();

        if let super::InlineRequest::Callable { link, function, .. } = code {
            if *link != function.callable.link {
                return Err(invalid("callable differs from its defining Link"));
            }
            function
                .check(
                    &lash_kernel_doc::FunctionRegistry::default(),
                    lash_kernel_doc::FunctionValueLimits::standard(),
                    lash_vocabulary::data_limits::KernelDataLimits::standard(),
                )
                .map_err(|refusal| match refusal {
                    lash_kernel_doc::FunctionValueRefusal::TooLarge { bytes, limit } => {
                        EnginePortError::Refused(AdmissionRefusal::CallableTooLarge {
                            bytes,
                            limit,
                        })
                    }
                    refusal => invalid(refusal.to_string()),
                })?;
            callables.extend(function.nodes());
        }
        let mut roots = vec![(root, super::documents::selection(&selector))];
        for callable in &callables {
            roots.push((
                callable.callable.link,
                ReachSelection::Callable(callable.callable.function),
            ));
        }
        roots.extend(
            retained
                .iter()
                .map(|callable| (callable.link, ReachSelection::Callable(callable.function))),
        );
        let links: BTreeSet<_> = roots.iter().map(|(link, _)| *link).collect();
        for link in links {
            if let super::InlineRequest::Callable { origin, .. } = code {
                self.documents
                    .validate_published_origin(link, origin)
                    .await?;
            }
            if !content.links.contains_key(&link) {
                let loaded = self.documents.load_linked(link).await?;
                content.documents.extend(loaded.documents);
                content.links.extend(loaded.links);
                content.packages.extend(loaded.packages);
            }
        }
        for callable in &callables {
            let document = content
                .document(
                    content
                        .link(callable.callable.link)
                        .map_err(super::map_link_refusal)?
                        .document,
                )
                .map_err(super::map_link_refusal)?;
            callable.check_definition(document).map_err(|_| {
                EnginePortError::Refused(AdmissionRefusal::CallableIdentity {
                    callable: callable.callable,
                })
            })?;
            let definition = document
                .functions
                .get(&callable.callable.function)
                .ok_or_else(|| invalid("captured callable definition is absent"))?;
            lash_kernel_doc::validate_callable_signature(
                &callable.signature,
                &callable.convention,
                &definition.signature,
            )
            .map_err(|error| invalid(error.to_string()))?;
            for target in callable.convention.targets() {
                match target {
                    FunctionTarget::Local(function) => {
                        roots.push((callable.callable.link, ReachSelection::Callable(*function)))
                    }
                    FunctionTarget::Import(slot) => match content
                        .link(callable.callable.link)
                        .map_err(super::map_link_refusal)?
                        .fills
                        .get(slot)
                    {
                        Some(Fill::Export { link, export }) => {
                            roots.push((*link, ReachSelection::Function(export.clone())))
                        }
                        _ => {
                            return Err(invalid("callable convention helper is not a code Import"));
                        }
                    },
                }
            }
        }
        let mut reach = LinkedReach::default();
        for (link, selection) in roots {
            let selected = content
                .inspect(link, &selection)
                .map_err(super::map_link_refusal)?;
            reach.operations.extend(selected.operations);
            reach.machine.extend(&selected.machine);
            reach.artifacts.extend(selected.artifacts);
            for (link, functions) in selected.functions {
                reach.functions.entry(link).or_default().extend(functions);
            }
        }
        let mut inspection = super::documents::inspection(&content, root, &selector)?;
        if let super::InlineRequest::Callable { function, .. } = code {
            let document = content
                .document(
                    content
                        .link(function.callable.link)
                        .map_err(super::map_link_refusal)?
                        .document,
                )
                .map_err(super::map_link_refusal)?;
            function
                .entry_export(document)
                .map_err(|error| match error {
                    lash_kernel_doc::FunctionValueRefusal::EntryAnnotation { source } => {
                        EnginePortError::Refused(AdmissionRefusal::EntryAnnotation { source })
                    }
                    lash_kernel_doc::FunctionValueRefusal::SignatureRequired { source } => {
                        EnginePortError::Refused(AdmissionRefusal::SignatureRequired { source })
                    }
                    _ => EnginePortError::Refused(AdmissionRefusal::CallableIdentity {
                        callable: function.callable,
                    }),
                })?;
            if function
                .signature
                .params
                .iter()
                .any(|param| !lash_vocabulary::types::data_only(&param.ty))
                || !lash_vocabulary::types::data_only(&function.signature.result)
            {
                return Err(invalid("host callable signature must contain only data"));
            }
            inspection.signature = Some(function.signature.clone());
        }
        inspection.operations = reach
            .operations
            .iter()
            .map(|(use_, need)| {
                (
                    *use_,
                    match need {
                        lash_kernel_package::OperationNeed::Fixed { name, requirement } => {
                            OperationNeed::Fixed {
                                name: name.clone(),
                                requirement: requirement.clone(),
                            }
                        }
                        lash_kernel_package::OperationNeed::Deferred { name, grant } => {
                            OperationNeed::Deferred {
                                name: name.clone(),
                                grant: grant.clone(),
                            }
                        }
                    },
                )
            })
            .collect();
        inspection.machine = reach.machine.clone();
        inspection.artifacts = reach
            .artifacts
            .iter()
            .map(|artifact| match artifact {
                RetainedContent::Document(id) => super::documents::document_artifact(id),
                RetainedContent::Link(id) => super::documents::link_artifact(*id),
                RetainedContent::Package(id) => super::documents::package_artifact(*id),
            })
            .collect();
        Ok(LoadedExecution {
            content,
            reach,
            inspection,
        })
    }
}
pub(crate) struct KernelInlineStarts {
    pub(crate) engine: Arc<KernelProcessEngine>,
}
#[async_trait::async_trait]
impl InlineStarts for KernelInlineStarts {
    async fn inspect(
        &self,
        request: &lash_execution_contracts::process::InlineRequest,
    ) -> Result<InlineDefinition, EnginePortError> {
        let code = match decoded(request)? {
            super::InlineRequest::Unlinked { document, selector } => {
                let fulfilled = self
                    .engine
                    .documents
                    .fulfil(&FulfilRequest {
                        document,
                        selector: ReachSelector::Start(selector.clone()),
                        snapshot: request.snapshot.clone(),
                        context: request.context.clone(),
                        purpose: LinkPurpose::Workflow {
                            attachment: request.attachment.clone(),
                        },
                    })
                    .await?;
                super::InlineRequest::Linked {
                    start: LinkedStart {
                        link: fulfilled.content.identity(),
                        selector,
                    },
                }
            }
            code => code,
        };
        let loaded = self
            .engine
            .load_execution(&code, &Default::default())
            .await?;
        let draft = definition_draft(&KernelProcessDefinition { code })
            .map_err(|error| invalid(error.to_string()))?;
        let signature = loaded.inspection.signature.as_ref().map_or(
            lash_execution_contracts::process::ProcessSignature::Unknown,
            entry_signature,
        );
        Ok(InlineDefinition {
            draft,
            signature,
            linked: loaded.inspection,
        })
    }
    async fn realize(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: &lash_execution_contracts::process::InlineRequest,
    ) -> Result<InlineDefinition, EnginePortError> {
        let definition = self.inspect(request).await?;
        self.engine
            .documents
            .hold_publications(claim, &request.snapshot)
            .await?;
        let payload = definition.draft.value().as_json();
        let code = KernelProcessDefinition::from_payload(payload)
            .map_err(|error| invalid(error.to_string()))?
            .code;
        if let super::InlineRequest::Callable { origin, .. } = &code {
            self.engine
                .documents
                .hold_callable_origin(claim, origin)
                .await?;
        }
        for link in code.links() {
            self.engine
                .documents
                .hold(claim, &ContentRef::Link(link))
                .await?;
        }
        // A fresh load validates the retained origin after every acquisition.
        self.engine
            .load_execution(&code, &Default::default())
            .await?;
        Ok(definition)
    }
}
