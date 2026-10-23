//! Sidecar facts for one retained linked document. Nothing here enters identity.
use lash_helper_labels::MemberDisplay;
use lash_kernel_doc::{Document, FunctionDefinition};
use lash_kernel_package::{LinkRefusal, LinkedContent};
use lash_vocabulary::{CallableId, Fill, ImportSlot, LinkId};
use std::collections::BTreeMap;

/// The immutable Link graph and its separately retained display facts.
/// The caller projects labels only from the publications retained by this execution.
#[derive(Clone, Debug)]
pub struct ViewContext {
    pub(crate) reference: lash_kernel_doc::WorkflowReference,
    helpers: std::sync::Arc<lash_kernel_doc::BuildHelpers>,
    pub(crate) content: LinkedContent,
    timers: std::collections::BTreeSet<(lash_vocabulary::PackageId, lash_vocabulary::Segment)>,
    pub(crate) labels: BTreeMap<CallableId, MemberDisplay>,
}
impl ViewContext {
    pub fn new(
        reference: lash_kernel_doc::WorkflowReference,
        content: LinkedContent,
        labels: BTreeMap<CallableId, MemberDisplay>,
        helpers: std::sync::Arc<lash_kernel_doc::BuildHelpers>,
    ) -> Result<Self, LinkRefusal> {
        content.link(reference.link())?;
        for link in content.links.values() {
            content.check_link(link)?;
        }
        Ok(Self {
            reference,
            helpers,
            content,
            labels,
            timers: Default::default(),
        })
    }
    /// Install build-owned display facts for exact helpers in each Link's
    /// admitted library reach. Labels confer no code or operation authority.
    pub fn add_helper_labels(
        &mut self,
        labels: &BTreeMap<lash_vocabulary::FunctionId, String>,
    ) -> Result<(), lash_helper_labels::LabelError> {
        for (function, json) in labels {
            if !self.helpers.contains(function) {
                return Err(lash_helper_labels::LabelError::at(
                    "/definitions",
                    &function.to_string(),
                    "helper is not installed",
                ));
            }
            let label = lash_helper_labels::read_json(&format!(
                "{{\"definitions\":{{\"helper\":{json}}}}}"
            ))?
            .definitions
            .remove("helper")
            .ok_or_else(|| {
                lash_helper_labels::LabelError::at(
                    "/definitions",
                    "helper",
                    "missing helper display",
                )
            })?;
            for (link, instance) in &self.content.links {
                let document = self.document(*link).ok_or_else(|| {
                    lash_helper_labels::LabelError::at("", "link", "missing document")
                })?;
                if self.helpers.admits(&document.library, function) {
                    self.labels.insert(
                        CallableId {
                            link: *link,
                            function: *function,
                        },
                        label.clone(),
                    );
                }
                let _ = instance;
            }
        }
        Ok(())
    }

    pub fn reference(&self) -> &lash_kernel_doc::WorkflowReference {
        &self.reference
    }
    pub(crate) fn document(&self, link: LinkId) -> Option<&Document> {
        self.content
            .links
            .get(&link)
            .and_then(|link| self.content.documents.get(&link.document))
    }
    pub(crate) fn resolve(&self, link: LinkId, slot: ImportSlot) -> Option<CallableId> {
        let Fill::Export { link, export } = self.content.links.get(&link)?.fills.get(&slot)? else {
            return None;
        };
        Some(CallableId {
            link: *link,
            function: self.content.function_export(*link, export).ok()?.function,
        })
    }
    pub(crate) fn display(&self, callable: &CallableId) -> Option<&MemberDisplay> {
        if let Some(display) = self.labels.get(callable) {
            return Some(display);
        }
        let definition = self.function(callable)?;
        self.labels.iter().find_map(|(other, display)| {
            if other.function != callable.function {
                return None;
            }
            let same_bindings = definition.body().is_none_or(|body| {
                body.imports.keys().all(|slot| {
                    self.content
                        .links
                        .get(&callable.link)
                        .and_then(|link| link.fills.get(slot))
                        == self
                            .content
                            .links
                            .get(&other.link)
                            .and_then(|link| link.fills.get(slot))
                })
            });
            same_bindings.then_some(display)
        })
    }
    pub(crate) fn header(
        &self,
        callable: &CallableId,
    ) -> Option<std::sync::Arc<lash_kernel_doc::FunctionHeader>> {
        let document = self.document(callable.link)?;
        document
            .functions
            .get(&callable.function)
            .map(|definition| std::sync::Arc::new(lash_kernel_doc::FunctionHeader::of(definition)))
            .or_else(|| {
                self.helpers
                    .admits(&document.library, &callable.function)
                    .then(|| {
                        self.helpers
                            .registered(&callable.function)
                            .map(|registered| std::sync::Arc::clone(registered.header()))
                    })
                    .flatten()
            })
    }
    pub(crate) fn function(&self, callable: &CallableId) -> Option<&FunctionDefinition> {
        let document = self.document(callable.link)?;
        document.functions.get(&callable.function).or_else(|| {
            self.helpers
                .admits(&document.library, &callable.function)
                .then(|| {
                    self.helpers
                        .registered(&callable.function)
                        .map(|registered| &**registered.definition())
                })
                .flatten()
        })
    }

    pub(crate) fn function_graph(&self, callable: &CallableId) -> Option<lash_kernel_check::Graph> {
        let document = self.document(callable.link)?;
        let definition = self.function(callable)?;
        let local = lash_kernel_doc::DocumentCatalog::new(document, self.helpers.as_ref());
        let catalog: &dyn lash_kernel_doc::FunctionCatalog =
            if document.functions.contains_key(&callable.function) {
                &local
            } else {
                self.helpers.as_ref()
            };
        lash_kernel_check::derive_definition(definition, catalog).ok()
    }
}
impl ViewContext {
    /// Join one independently retained sidecar by its immutable Package id.
    pub fn add_labels(
        &mut self,
        id: lash_vocabulary::PackageId,
        json: &str,
    ) -> Result<(), lash_helper_labels::LabelError> {
        use lash_kernel_package::MemberKind;
        use lash_vocabulary::{ExportName, Segment};
        let missing =
            || lash_helper_labels::LabelError::at("", "package", "sidecar Package is not retained");
        let package = self.content.packages.get(&id).ok_or_else(missing)?;
        let labels = lash_kernel_package::read_labels(&self.content, package, json)?;
        for (name, display) in labels.members {
            let mut parts = name.split('.');
            let segment = Segment::new(parts.next().unwrap_or_default()).map_err(|_| missing())?;
            let Some(lash_kernel_package::Member {
                kind: MemberKind::Export { link, export },
                ..
            }) = package.members.get(&segment)
            else {
                continue;
            };
            let export = ExportName {
                name: export.name.clone(),
                member: parts
                    .next()
                    .map(Segment::new)
                    .transpose()
                    .map_err(|_| missing())?
                    .or_else(|| export.member.clone()),
            };
            if let Ok(function) = self.content.function_export(*link, &export) {
                self.labels.insert(
                    CallableId {
                        link: *link,
                        function: function.function,
                    },
                    display,
                );
            }
        }
        for (name, display) in labels.definitions {
            for member in package.members.values() {
                if let MemberKind::Export { link, .. } = &member.kind
                    && let Some(document) = self.document(*link)
                {
                    let ids = document
                        .functions
                        .iter()
                        .filter(|(_, definition)| definition.name.as_str() == name)
                        .map(|(id, _)| *id)
                        .collect::<Vec<_>>();
                    for function in ids {
                        self.labels.insert(
                            CallableId {
                                link: *link,
                                function,
                            },
                            display.clone(),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

impl ViewContext {
    /// The runtime supplies its well-known timer symbols as neutral immutable identities.
    pub fn with_timer_operations(
        mut self,
        timers: impl IntoIterator<Item = (lash_vocabulary::PackageId, lash_vocabulary::Segment)>,
    ) -> Self {
        self.timers.extend(timers);
        self
    }
    pub(crate) fn is_timer(&self, use_: lash_vocabulary::OperationUse) -> bool {
        let Some(link) = self.content.links.get(&use_.link) else {
            return false;
        };
        let Some(document) = self.document(use_.link) else {
            return false;
        };
        let Some(import) = document.imports.get(&use_.slot) else {
            return false;
        };
        let Some(lash_vocabulary::Fill::Operation(required)) = link.fills.get(&use_.slot) else {
            return false;
        };
        self.content.packages.iter().any(|(id, package)| {
            self.timers.contains(&(*id, import.name.member.clone())) && package.path == import.name.package
                && package.members.get(&import.name.member).is_some_and(|member| matches!(&member.kind, lash_vocabulary::MemberKind::Operation(offer) if offer.requirement == *required))
        })
    }
    pub(crate) fn only_timer(&self, callable: CallableId) -> Option<lash_vocabulary::OperationUse> {
        let reach = self
            .content
            .inspect(
                callable.link,
                &lash_kernel_package::ReachSelection::Callable(callable.function),
                self.helpers.as_ref(),
            )
            .ok()?;
        let mut uses = reach.operations.keys();
        let use_ = *uses.next()?;
        (uses.next().is_none() && self.is_timer(use_)).then_some(use_)
    }
}

impl ViewContext {
    pub(crate) fn reached(
        &self,
        callable: CallableId,
    ) -> std::collections::BTreeSet<lash_vocabulary::OperationUse> {
        self.content
            .inspect(
                callable.link,
                &lash_kernel_package::ReachSelection::Callable(callable.function),
                self.helpers.as_ref(),
            )
            .map(|reach| reach.operations.into_keys().collect())
            .unwrap_or_default()
    }
}
