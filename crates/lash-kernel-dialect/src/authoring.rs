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
pub struct Imports(std::cell::RefCell<BTreeMap<ImportSlot, Import>>);
impl Imports {
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
        while entries.contains_key(&slot) {
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

/// Stable owner-assigned slots in shipped helper definitions. Release assembly
/// unions these declarations without rewriting already identified functions.
pub fn service_slot(name: &QualifiedName) -> Option<ImportSlot> {
    let base = match name.package.to_string().as_str() {
        "typescript" => 0xffff0000,
        "python" => 0xffff0010,
        _ => return None,
    };
    let offset = match name.member.as_str() {
        "clock" => 0,
        "random" => 1,
        "sleep" => 2,
        _ => return None,
    };
    Some(ImportSlot(base + offset))
}

/// The semantic service contract, shared by release fulfilment and runners.
/// Runtime bounds and injected time/random sources are hosting policy.
pub fn service_requirement(name: &QualifiedName) -> Option<lash_vocabulary::OperationRequirement> {
    service_slot(name)?;
    let signature = match name.member.as_str() {
        "clock" => Signature {
            params: Vec::new(),
            result: Type::Timestamp,
        },
        "random" => Signature {
            params: Vec::new(),
            result: Type::Bytes,
        },
        "sleep" => crate::sleep_signature(),
        _ => return None,
    };
    Some(lash_vocabulary::OperationRequirement::new(
        signature,
        lash_vocabulary::ContractId::new(format!("lash.dialect.{}.{}", name.package, name.member)),
    ))
}

/// Project direct build-library references and lift their transitive operation
/// contracts into the caller's stable service slots. Hashed bodies stay in the build.
pub fn include_library_dependencies(
    document: &mut lash_kernel_doc::Document,
    used: impl IntoIterator<Item = FunctionId>,
    library: &dyn crate::Library,
) -> Result<(), String> {
    let mut pending: Vec<_> = used
        .into_iter()
        .filter(|id| !document.functions.contains_key(id))
        .collect();
    document.library.0.extend(pending.iter().copied());
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let header = library
            .header(&id)
            .ok_or_else(|| format!("missing build helper {id}"))?;
        if header.has_native() {
            document.requires.natives.insert(id);
        }
        document.requires.kernels.insert(header.kernel);
        let (callees, imports) = if let Some(registered) = library.registered(&id) {
            (registered.callees().clone(), registered.imports().clone())
        } else {
            let definition = library
                .definition(&id)
                .ok_or_else(|| format!("missing build helper {id}"))?;
            definition
                .body()
                .map(|body| {
                    (
                        body.functions.keys().copied().collect(),
                        body.imports.clone(),
                    )
                })
                .unwrap_or_default()
        };
        pending.extend(callees);
        for (slot, import) in imports {
            if service_slot(&import.name) != Some(slot) {
                return Err(format!(
                    "build helper operation {} has no stable service slot",
                    import.name
                ));
            }
            if let Some(previous) = document.imports.get(&slot) {
                if previous != &import {
                    return Err(format!("incompatible operation bindings at {slot:?}"));
                }
            } else {
                document.imports.insert(slot, import);
            }
        }
    }
    Ok(())
}
