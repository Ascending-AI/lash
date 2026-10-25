//! Document-local import allocation and authored callable storage. Chosen
//! contracts and defining Links are supplied by fulfilment/execution, never
//! invented by a source front end.
use lash_kernel_doc::{Block, FunctionBody, FunctionDefinition, Implementation, Node};

use lash_vocabulary::{
    names::*,
    types::{Param, Signature, Type},
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Imports(
    std::cell::RefCell<BTreeMap<ImportSlot, Import>>,
    std::collections::BTreeSet<ImportSlot>,
);
impl Imports {
    /// Reserve the helper owner's slots before allocating authored imports.
    pub fn reserving(mut self, slots: impl IntoIterator<Item = ImportSlot>) -> Self {
        self.1.extend(slots);
        self
    }
    pub fn insert(&self, import: Import) -> ImportSlot {
        let mut entries = self.0.borrow_mut();
        if let Some((slot, _)) = entries.iter().find(|(_, found)| **found == import) {
            return *slot;
        }
        let slot = ImportSlot(
            u32::try_from(entries.len())
                .unwrap_or_else(|error| panic!("source admission bounds import count: {error}")),
        );
        let mut slot = slot;
        while entries.contains_key(&slot) || self.1.contains(&slot) {
            slot.0 += 1;
        }
        entries.insert(slot, import);
        slot
    }
    pub fn code(
        &self,
        name: QualifiedName,
        signature: Signature,
        convention: Convention,
    ) -> ImportSlot {
        self.insert(Import {
            name,
            kind: ImportKind::Code {
                signature,
                convention,
            },
        })
    }
    pub fn operation(&self, name: QualifiedName, signature: Signature) -> ImportSlot {
        self.insert(Import {
            name,
            kind: ImportKind::Operation { signature },
        })
    }
    pub fn into_map(self) -> BTreeMap<ImportSlot, Import> {
        self.0.into_inner()
    }
    pub fn as_map(&self) -> std::cell::Ref<'_, BTreeMap<ImportSlot, Import>> {
        self.0.borrow()
    }
}

/// Defines one authored body by content identity. Captures remain free
/// variables in its code; closure construction captures them under its Link.
pub fn local_function(
    params: Vec<Name>,
    block: Block,
    imports: &BTreeMap<ImportSlot, Import>,
    library: &dyn crate::Library,
    locals: &BTreeMap<FunctionId, FunctionDefinition>,
) -> FunctionDefinition {
    let ids = Node::Block(&block).functions_within();
    let functions = ids
        .into_iter()
        .filter_map(|id| {
            locals
                .get(&id)
                .map(|definition| definition.name.clone())
                .or_else(|| library.header(&id).map(|header| header.name.clone()))
                .map(|name| (id, name))
        })
        .collect();
    let used = used_imports(&block);
    FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: FunctionName::new("local.function")
            .unwrap_or_else(|error| panic!("qualified: {error}")),
        signature: Signature {
            params: params
                .into_iter()
                .map(|name| Param {
                    name,
                    ty: Type::Any,
                    optional: false,
                })
                .collect(),
            result: Type::Any,
        },
        errors: Default::default(),
        guard: None,
        implementation: Implementation::Guest {
            metering: lash_kernel_doc::Metering::Statements,
            body: FunctionBody {
                functions,
                imports: imports
                    .iter()
                    .filter(|(slot, _)| used.contains(slot))
                    .map(|(slot, import)| (*slot, import.clone()))
                    .collect(),
                block,
            },
        },
        native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
    }
}
pub fn used_imports(block: &Block) -> std::collections::BTreeSet<ImportSlot> {
    let mut used = std::collections::BTreeSet::new();
    let mut pending = vec![Node::Block(block)];
    while let Some(node) = pending.pop() {
        used.extend(node.imports());
        pending.extend(node.children());
    }
    used
}

/// One operation contract declared by the helper owner. Its slot belongs to
/// the document assembly that composes these helpers with authored code.
#[derive(Clone, Debug)]
pub struct HelperOperation {
    pub slot: ImportSlot,
    pub name: QualifiedName,
    pub requirement: lash_vocabulary::OperationRequirement,
}

/// Project the owner's declarations once, refusing duplicate slots or names.
pub fn operation_imports(
    operations: &[HelperOperation],
) -> Result<BTreeMap<ImportSlot, Import>, String> {
    let mut imports = BTreeMap::new();
    let mut names = std::collections::BTreeSet::new();
    for operation in operations {
        if !names.insert(operation.name.clone()) {
            return Err(format!("duplicate helper operation {}", operation.name));
        }
        let import = Import {
            name: operation.name.clone(),
            kind: ImportKind::Operation {
                signature: operation.requirement.signature.clone(),
            },
        };
        if imports.insert(operation.slot, import).is_some() {
            return Err(format!(
                "duplicate helper operation slot {:?}",
                operation.slot
            ));
        }
    }
    Ok(imports)
}

/// Project direct build-library references and lift their transitive operation
/// imports into the owner's document-local slots. Hashed bodies stay in the build.
pub fn include_library_dependencies(
    document: &mut lash_kernel_doc::Document,
    used: impl IntoIterator<Item = FunctionId>,
    library: &dyn crate::Library,
) -> Result<(), String> {
    let used: Vec<_> = used
        .into_iter()
        .filter(|id| !document.functions.contains_key(id))
        .collect();
    document.library.0.extend(used);
    lash_kernel_doc::library_requirements(document, library).map_err(|refusal| refusal.to_string())
}
