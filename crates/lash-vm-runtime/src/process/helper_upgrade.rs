//! Explicit helper revisions retain the original Link DAG and operation fills.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::{HelperUpgrade, HelperUpgradeRefusal, IdentifiedDocument};
use lash_kernel_package::LinkedContent;
use lash_kernel_value::{Fill, LinkId, LinkedStart};

pub use lash_kernel_doc::{
    HelperPublicationUpgrade, HelperPublicationUpgradeRequest, HelperRunUpgradeRequest,
};

pub(super) struct PlannedHelperUpgrade {
    pub content: LinkedContent,
    pub links: BTreeMap<LinkId, LinkId>,
    pub rewritten: BTreeMap<LinkId, lash_kernel_migrate::Rewritten>,
    pub upgrade: HelperUpgrade,
}

fn invalid(message: impl std::fmt::Display) -> HelperUpgradeRefusal {
    HelperUpgradeRefusal::Document {
        message: message.to_string(),
    }
}

pub(super) fn plan(
    content: &LinkedContent,
    root: LinkId,
    archive: &lash_kernel_releases::archive::Archive,
) -> Result<PlannedHelperUpgrade, HelperUpgradeRefusal> {
    let release = archive
        .releases
        .last()
        .ok_or_else(|| invalid("empty helper ledger"))?
        .library
        .identity;
    plan_at(content, root, archive, release)
}

pub(super) fn plan_at(
    content: &LinkedContent,
    root: LinkId,
    archive: &lash_kernel_releases::archive::Archive,
    release: lash_kernel_value::HelperLibraryId,
) -> Result<PlannedHelperUpgrade, HelperUpgradeRefusal> {
    let reached = content
        .documents
        .values()
        .flat_map(|doc| doc.library.iter().copied())
        .collect();
    let upgrade = archive.upgrade_to(&reached, release)?;
    let mut planned = PlannedHelperUpgrade {
        content: LinkedContent::default(),
        links: BTreeMap::new(),
        rewritten: BTreeMap::new(),
        upgrade,
    };
    fn one(
        source: &LinkedContent,
        id: LinkId,
        visiting: &mut BTreeSet<LinkId>,
        out: &mut PlannedHelperUpgrade,
    ) -> Result<LinkId, HelperUpgradeRefusal> {
        if let Some(next) = out.links.get(&id) {
            return Ok(*next);
        }
        if !visiting.insert(id) {
            return Err(invalid("cyclic Link imports"));
        }
        let mut link = source.link(id).map_err(invalid)?.clone();
        for fill in link.fills.values_mut() {
            if let Fill::Export { link, .. } = fill {
                *link = one(source, *link, visiting, out)?;
            }
        }
        let before = source.document(link.document).map_err(invalid)?;
        let rewritten = lash_kernel_migrate::upgrade_helpers(before, &out.upgrade)?;
        let document = IdentifiedDocument::new(rewritten.document.clone());
        link.document = document.id();
        let next = link.identity();
        out.content
            .documents
            .insert(document.id(), Arc::clone(document.shared()));
        out.content.links.insert(next, link);
        out.links.insert(id, next);
        out.rewritten.insert(id, rewritten);
        visiting.remove(&id);
        Ok(next)
    }
    one(content, root, &mut BTreeSet::new(), &mut planned)?;
    Ok(planned)
}

/// The input itself is retained code for a run; a publication creates a new selected callable.
pub(super) fn check_frozen(
    value: &lash_kernel_doc::FunctionValue,
    content: &LinkedContent,
    helpers: &lash_kernel_doc::BuildHelpers,
    upgrade: &HelperUpgrade,
    retain_root: bool,
) -> Result<(), HelperUpgradeRefusal> {
    lash_kernel_migrate::check_upgrade_captures(value, upgrade)?;
    let mut live = lash_kernel_state::paged::RetainedCode::default();
    for node in value.nodes().into_iter().skip(usize::from(!retain_root)) {
        live.callables.insert(node.callable);
        for target in node.convention.targets() {
            match target {
                lash_kernel_doc::FunctionTarget::Local(function) => {
                    live.callables.insert(lash_kernel_value::CallableId {
                        link: node.callable.link,
                        function: *function,
                    });
                }
                lash_kernel_doc::FunctionTarget::Import(slot) => {
                    live.imports.insert((node.callable.link, *slot));
                }
            }
        }
    }
    let (documents, imports) = super::pages::helper_code(content).map_err(invalid)?;
    let retained = lash_kernel_state::paged::restore_selection(live, &documents, &imports, helpers)
        .map_err(invalid)?;
    if let Some(function) = retained.pins.intersection(&upgrade.changed).next() {
        return Err(HelperUpgradeRefusal::RetainedCallable {
            function: *function,
        });
    }
    Ok(())
}

pub(super) fn rewritten_code(
    code: &super::InlineRequest,
    source: &LinkedContent,
    planned: &PlannedHelperUpgrade,
) -> Result<super::InlineRequest, HelperUpgradeRefusal> {
    Ok(match code {
        super::InlineRequest::Linked { start } => super::InlineRequest::Linked {
            start: LinkedStart {
                link: planned.links[&start.link],
                selector: start.selector.clone(),
            },
        },
        super::InlineRequest::Callable {
            link,
            function,
            origin,
        } => {
            let next = planned.links[link];
            let before = source
                .document(source.link(*link).map_err(invalid)?.document)
                .map_err(invalid)?;
            let function = lash_kernel_migrate::upgrade_callable(
                function,
                before,
                &planned.rewritten[link],
                next,
                &planned.upgrade,
            )?;
            let mut origin = origin.clone();
            origin.authored.insert(next);
            super::InlineRequest::Callable {
                link: next,
                function: Box::new(function),
                origin,
            }
        }
        super::InlineRequest::Unlinked { .. } => return Err(invalid("publication is not linked")),
    })
}

pub(super) fn check_arguments(
    input: &super::KernelProcessInput,
    content: &LinkedContent,
    helpers: &lash_kernel_doc::BuildHelpers,
    upgrade: &HelperUpgrade,
) -> Result<(), HelperUpgradeRefusal> {
    let (link, _) = input
        .code
        .linked()
        .ok_or_else(|| invalid("run is not linked"))?;
    let document = content
        .document(content.link(link).map_err(invalid)?.document)
        .map_err(invalid)?;
    let args = super::run::entry_args(document, input, helpers, &lash_kernel_doc::Selection::Fresh)
        .map_err(invalid)?;
    for value in &args {
        lash_kernel_migrate::check_upgrade_data(value, upgrade)?;
    }
    Ok(())
}

fn reference(
    code: &super::InlineRequest,
) -> Result<lash_kernel_value::WorkflowReference, HelperUpgradeRefusal> {
    match code {
        super::InlineRequest::Linked { start } => Ok(start.clone().into()),
        super::InlineRequest::Callable { function, .. } => Ok(
            lash_kernel_value::WorkflowReference::Callable(function.callable),
        ),
        super::InlineRequest::Unlinked { .. } => Err(invalid("publication is not linked")),
    }
}

impl super::KernelDocuments {
    pub async fn upgrade_publication(
        &self,
        claim: &lash_core::ReferrerClaim,
        request: &HelperPublicationUpgradeRequest,
    ) -> Result<
        Result<HelperPublicationUpgrade, HelperUpgradeRefusal>,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        Ok(self
            .upgrade_code(
                claim,
                &super::InlineRequest::Linked {
                    start: request.publication.clone(),
                },
            )
            .await?
            .map(|(_, publication)| publication))
    }

    pub async fn upgrade_code(
        &self,
        claim: &lash_core::ReferrerClaim,
        code: &super::InlineRequest,
    ) -> Result<
        Result<(super::InlineRequest, HelperPublicationUpgrade), HelperUpgradeRefusal>,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.upgrade_code_with(claim, code, None).await
    }

    pub async fn upgrade_run_code(
        &self,
        claim: &lash_core::ReferrerClaim,
        input: &super::KernelProcessInput,
    ) -> Result<
        Result<(super::InlineRequest, HelperPublicationUpgrade), HelperUpgradeRefusal>,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        self.upgrade_code_with(claim, &input.code, Some(input))
            .await
    }

    async fn upgrade_code_with(
        &self,
        claim: &lash_core::ReferrerClaim,
        code: &super::InlineRequest,
        input: Option<&super::KernelProcessInput>,
    ) -> Result<
        Result<(super::InlineRequest, HelperPublicationUpgrade), HelperUpgradeRefusal>,
        lash_execution_contracts::process::linking::EnginePortError,
    > {
        use lash_execution_contracts::process::linking::{ContentRef, EnginePortError};
        let from = match reference(code) {
            Ok(from) => from,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let source = self.authoring_content(code).await?;
        let archive = lash_kernel_releases::archive_candidate().map_err(|error| {
            EnginePortError::Unavailable {
                message: error.to_string(),
            }
        })?;
        let prepared = (|| {
            let planned = plan(&source, from.link(), &archive)?;
            if let Some(input) = input {
                check_arguments(input, &source, self.helpers(), &planned.upgrade)?;
            }
            if let super::InlineRequest::Callable { function, .. } = code {
                check_frozen(
                    function,
                    &source,
                    self.helpers(),
                    &planned.upgrade,
                    input.is_some(),
                )?;
            }
            let next = rewritten_code(code, &source, &planned)?;
            Ok::<_, HelperUpgradeRefusal>((planned, next))
        })();
        let (planned, next) = match prepared {
            Ok(prepared) => prepared,
            Err(refusal) => {
                tracing::info!(publication = ?from, ?refusal, "helper publication upgrade refused");
                return Ok(Err(refusal));
            }
        };
        let to = reference(&next).map_err(|error| EnginePortError::Unavailable {
            message: error.to_string(),
        })?;
        let (_, selected) = next.linked().ok_or_else(|| EnginePortError::Unavailable {
            message: "upgrade lost its Link".into(),
        })?;
        self.inspect(
            &planned.content,
            to.link(),
            &super::documents::selection(&selected),
        )
        .map_err(super::map_link_refusal)?;
        self.hold(claim, &ContentRef::Link(from.link())).await?;
        self.hold_helper_upgrade(claim, &planned.content).await?;
        if let super::InlineRequest::Callable { origin, .. } = &next {
            self.hold_callable_origin(claim, origin).await?;
        }
        self.load_execution(&next, &Default::default()).await?;
        Ok(Ok((
            next,
            HelperPublicationUpgrade {
                from,
                document: planned.content.links[&to.link()].document,
                to,
                links: planned.links,
                upgrade: planned.upgrade,
            },
        )))
    }
}
