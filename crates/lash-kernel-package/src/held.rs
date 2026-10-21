//! Runtime lookup keeps bytes separate from linked instance identity.
use crate::{LinkRefusal, ReachSelection};
use lash_kernel_doc::{Document, Export, FunctionExport};

use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::{
    claims::NodeRequirements, names::*, operations::OperationRequirement, package::*,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct LinkedContent {
    pub documents: BTreeMap<DocumentId, Document>,
    pub links: BTreeMap<LinkId, Link>,
    pub packages: BTreeMap<PackageId, Package>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetainedContent {
    Document(DocumentId),
    Link(LinkId),
    Package(PackageId),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationNeed {
    Fixed {
        name: QualifiedName,
        requirement: OperationRequirement,
    },
    Deferred {
        name: QualifiedName,
        grant: Box<DeferredGrant>,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkedReach {
    /// Selected local function identities, qualified by their defining Link.
    pub functions: BTreeMap<LinkId, BTreeSet<FunctionId>>,
    pub operations: BTreeMap<OperationUse, OperationNeed>,
    pub machine: NodeRequirements,
    pub artifacts: BTreeSet<RetainedContent>,
    pub signature: Option<lash_vocabulary::types::Signature>,
}
impl LinkedContent {
    pub fn insert_document(&mut self, document: Document) -> Result<DocumentId, LinkRefusal> {
        lash_kernel_doc::validate_document(&document, &document.functions).map_err(|e| {
            LinkRefusal::InvalidDocument {
                message: e.to_string(),
            }
        })?;
        let id = document.identity();
        self.documents.insert(id, document);
        Ok(id)
    }
    pub fn insert_link(&mut self, link: Link) -> Result<LinkId, LinkRefusal> {
        self.check_link(&link)?;
        let id = link.identity();
        self.links.insert(id, link);
        Ok(id)
    }
    pub fn insert_package(&mut self, package: Package) -> PackageId {
        let id = package.identity();
        self.packages.insert(id, package);
        id
    }
    pub fn document(&self, id: DocumentId) -> Result<&Document, LinkRefusal> {
        self.documents
            .get(&id)
            .ok_or(LinkRefusal::MissingDocument { document: id })
    }
    pub fn link(&self, id: LinkId) -> Result<&Link, LinkRefusal> {
        let link = self
            .links
            .get(&id)
            .ok_or(LinkRefusal::MissingLink { link: id })?;
        if link.identity() != id {
            return Err(LinkRefusal::InvalidDocument {
                message: format!("Link {id} has another identity"),
            });
        }
        Ok(link)
    }
    pub fn export(&self, link: LinkId, name: &ExportName) -> Result<Export, LinkRefusal> {
        let document = self.document(self.link(link)?.document)?;
        let export = document
            .exports
            .get(&name.name)
            .ok_or_else(|| LinkRefusal::NotFunction {
                export: name.clone(),
            })?;
        match (&name.member, export) {
            (None, export) => Ok(export.clone()),
            (Some(member), Export::Type(ty)) => ty
                .members
                .get(member)
                .map(|m| Export::Function(m.function().clone()))
                .ok_or_else(|| LinkRefusal::NotFunction {
                    export: name.clone(),
                }),
            _ => Err(LinkRefusal::NotFunction {
                export: name.clone(),
            }),
        }
    }
    pub fn function_export(
        &self,
        link: LinkId,
        name: &ExportName,
    ) -> Result<FunctionExport, LinkRefusal> {
        match self.export(link, name)? {
            Export::Function(f) => Ok(f),
            _ => Err(LinkRefusal::NotFunction {
                export: name.clone(),
            }),
        }
    }
    /// Local validation requires every slot, a serving kind and no undeclared fill.
    pub fn check_link(&self, link: &Link) -> Result<(), LinkRefusal> {
        let document = self.document(link.document)?;
        if link.fills.len() != document.imports.len() {
            return Err(LinkRefusal::InvalidDocument {
                message: "Link fills must equal the Document's Import slots".into(),
            });
        }
        for (slot, import) in &document.imports {
            let fill = link
                .fills
                .get(slot)
                .ok_or_else(|| LinkRefusal::MissingImport {
                    slot: *slot,
                    name: import.name.clone(),
                })?;
            crate::build::compatible(self, *slot, import, fill)?;
        }
        Ok(())
    }
    /// The full retention DAG is deliberately broader than a selected executable reach.
    pub fn retain_reach(&self, root: LinkId) -> Result<BTreeSet<RetainedContent>, LinkRefusal> {
        let mut found = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !found.insert(RetainedContent::Link(id)) {
                continue;
            }
            let link = self.link(id)?;
            self.check_link(link)?;
            self.document(link.document)?;
            found.insert(RetainedContent::Document(link.document));
            pending.extend(link.fills.values().filter_map(|fill| match fill {
                Fill::Export { link, .. } => Some(*link),
                _ => None,
            }));
        }
        Ok(found)
    }
    /// Inspect selected code without re-fulfilling old Links against advertisements.
    pub fn inspect(
        &self,
        root: LinkId,
        selector: &ReachSelection,
    ) -> Result<LinkedReach, LinkRefusal> {
        let mut reach = LinkedReach {
            artifacts: self.retain_reach(root)?,
            ..Default::default()
        };
        let root_document = self.document(self.link(root)?.document)?;
        if let ReachSelection::Start(StartSelector::Export(name)) = selector {
            reach.signature = Some(
                lash_kernel_check::startable_export(root_document, name)?
                    .signature
                    .clone(),
            );
        }
        let mut pending = vec![(root, selector.clone())];
        let mut visited = BTreeSet::new();
        while let Some((id, selector)) = pending.pop() {
            if !visited.insert((id, selector.clone())) {
                continue;
            }
            let link = self.link(id)?;
            let document = self.document(link.document)?;
            let required = match &selector {
                ReachSelection::Start(selector) => {
                    lash_kernel_check::selected_reach(document, selector)?
                }
                ReachSelection::Callable(function) => {
                    if !document.functions.contains_key(function) {
                        return Err(LinkRefusal::InvalidDocument {
                            message: format!("missing callable {function}"),
                        });
                    }
                    let mut required = lash_kernel_check::Requirements::default();
                    crate::build::follow_function(document, *function, &mut required);
                    required
                }
                ReachSelection::AllExports => {
                    lash_kernel_check::requirements(document, &document.functions)
                }
                ReachSelection::Function(export) => {
                    // Imported functions may accept executable values; the host-only predicate does not apply.
                    let function = self.function_export(id, export)?;
                    crate::build::function_reach(document, &function)
                }
                ReachSelection::Type(export) => {
                    let Export::Type(ty) = self.export(id, export)? else {
                        return Err(LinkRefusal::NotFunction {
                            export: export.clone(),
                        });
                    };
                    let mut required = lash_kernel_check::Requirements::default();
                    crate::build::follow_function(document, ty.checker, &mut required);
                    for member in ty.members.values() {
                        let r = crate::build::function_reach(document, member.function());
                        required.imports.extend(r.imports);
                        required.functions.extend(r.functions);
                    }
                    required
                }
            };
            reach
                .functions
                .entry(id)
                .or_default()
                .extend(required.functions.keys().copied());
            reach.machine.kernels.extend(&document.requires.kernels);
            reach.machine.natives.extend(
                required
                    .functions
                    .keys()
                    .filter(|id| document.requires.natives.contains(id))
                    .copied(),
            );
            for slot in required.imports {
                let import =
                    document
                        .imports
                        .get(&slot)
                        .ok_or_else(|| LinkRefusal::InvalidDocument {
                            message: format!("undeclared reached slot {slot:?}"),
                        })?;
                let fill = link
                    .fills
                    .get(&slot)
                    .ok_or_else(|| LinkRefusal::MissingImport {
                        slot,
                        name: import.name.clone(),
                    })?;
                let use_ = OperationUse { link: id, slot };
                match fill {
                    Fill::Operation(requirement) => {
                        reach.operations.insert(
                            use_,
                            OperationNeed::Fixed {
                                name: import.name.clone(),
                                requirement: requirement.clone(),
                            },
                        );
                    }
                    Fill::Deferred { provider } => {
                        reach.operations.insert(
                            use_,
                            OperationNeed::Deferred {
                                name: import.name.clone(),
                                grant: provider.clone(),
                            },
                        );
                    }
                    Fill::Export { link, export } => match &import.kind {
                        ImportKind::Code { .. } => {
                            pending.push((*link, ReachSelection::Function(export.clone())))
                        }
                        ImportKind::Type { .. } => {
                            pending.push((*link, ReachSelection::Type(export.clone())))
                        }
                        ImportKind::Error { .. } => {}
                        ImportKind::Operation { .. } => {
                            return Err(LinkRefusal::KindMismatch {
                                slot,
                                name: import.name.clone(),
                            });
                        }
                    },
                }
            }
        }
        if reach.machine.kernels.len() != 1 {
            return Err(LinkRefusal::InvalidDocument {
                message: "linked executable reach must use one kernel version".into(),
            });
        }
        Ok(reach)
    }
}
