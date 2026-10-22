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

/// Canonical package name of a helper. All helpers under one source root
/// share one package, so a helper and helpers beneath its name cannot make
/// a member/namespace collision. Length-delimited canonical words preserve
/// the original segment boundaries; canonical-name collisions are refused
/// by the library at definition time.
pub fn helper_name(name: &FunctionName) -> QualifiedName {
    let mut parts = name
        .as_str()
        .split('.')
        .map(canonical_segment)
        .collect::<Vec<_>>();
    let root = if parts.len() > 1 {
        parts.remove(0)
    } else {
        Segment::new("kernel").unwrap_or_else(|error| panic!("canonical: {error}"))
    };
    let mut member = String::from("h");
    for part in parts {
        member.push_str(&format!("_n{}_{}", part.as_str().len(), part.as_str()));
    }
    QualifiedName {
        package: PackagePath::new(root),
        member: Segment::new(member)
            .unwrap_or_else(|error| panic!("length-delimited canonical helper words: {error}")),
        type_member: None,
    }
}

fn canonical_segment(text: &str) -> Segment {
    let chars = text.chars().collect::<Vec<_>>();
    let mut result = String::new();
    for (at, ch) in chars.iter().copied().enumerate() {
        if ch == '_' {
            if !result.is_empty() && !result.ends_with('_') {
                result.push('_');
            }
            continue;
        }
        if ch.is_ascii_uppercase()
            && !result.is_empty()
            && !result.ends_with('_')
            && (at > 0 && chars[at - 1].is_ascii_lowercase()
                || chars.get(at + 1).is_some_and(char::is_ascii_lowercase)
                    && at > 0
                    && chars[at - 1].is_ascii_uppercase())
        {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    let result = result
        .trim_matches('_')
        .split('_')
        .map(|word| {
            if word.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                format!("n{word}")
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("_");
    Segment::new(&result)
        .unwrap_or_else(|error| panic!("helper names have canonical word segments: {error}"))
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

/// Retain the definitions behind native calls, including their reference bodies
/// and identity-addressed local callees. Imported guest helpers retain their
/// own defining document instead.
pub fn include_native_definitions(
    document: &mut lash_kernel_doc::Document,
    used: impl IntoIterator<Item = FunctionId>,
    library: &dyn crate::Library,
) {
    let mut pending = used.into_iter().collect::<Vec<_>>();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(definition) = document
            .functions
            .get(&id)
            .or_else(|| library.definition(&id))
            .cloned()
        else {
            continue;
        };
        if definition.has_native() {
            document.requires.natives.insert(id);
        }
        if let Some(body) = definition.body() {
            pending.extend(body.functions.keys().copied());
            for (slot, import) in &body.imports {
                if let Some(previous) = document.imports.insert(*slot, import.clone()) {
                    assert_eq!(
                        previous, *import,
                        "native body import slots retain their owner"
                    );
                }
            }
        }
        document.functions.insert(id, definition);
    }
}
