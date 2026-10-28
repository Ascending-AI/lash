//! Immutable document admission and preparation, scoped to one prepared
//! library and layout. Only live document allocations can reuse admission:
//! the weak reference prevents pointer reuse and copy-on-write mutation from
//! ever selecting code for a different document. Code holds neither its
//! document nor its library, so the cache has no ownership cycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lash_kernel_doc::{Document, Invalid, validate_document_with_limits};

use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::names::DocumentId;

use super::*;
use crate::{OpenError, StartError};

#[derive(Default)]
pub(super) struct Documents(
    BTreeMap<
        (
            usize,
            usize,
            lash_kernel_value::HelperLibraryId,
            doc::Selection,
        ),
        Arc<Admission>,
    >,
    /// The identities validated against this library, with the document
    /// depth they were validated under: a later admission of one skips
    /// validation (FIG-6931).
    BTreeSet<(DocumentId, usize, lash_kernel_value::HelperLibraryId)>,
    BTreeMap<usize, Arc<ImmutableIdentity>>,
);

/// Weak ownership reserves the allocation address without retaining source
/// bytes. Arc copy-on-write gives modified content a distinct allocation.
struct ImmutableIdentity {
    document: Weak<Document>,
    identity: OnceLock<DocumentId>,
}

/// The most validated identities a library remembers; past it, it starts
/// again.
const VALIDATED: usize = 4096;

type CodeCell = Arc<OnceLock<Result<PreparedCode, Missing>>>;

pub(crate) struct Admission {
    pub(super) selection: doc::Selection,
    document: Weak<Document>,
    identity: OnceLock<DocumentId>,
    code: Mutex<BTreeMap<CodeContent, CodeCell>>,
}

/// Operation fills stay per execution. Immutable compilation also keys the
/// code Imports it resolves, over retained content and admitted functions.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CodeContent {
    documents: Vec<DocumentId>,
    functions: Vec<FunctionId>,
    imports: BTreeSet<ImportResolution>,
}

/// Code identity depends on resolved targets and export descriptors, never on
/// execution Links or operation routes. Certification is derived only on a miss.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ImportResolution {
    source: DocumentId,
    slot: lash_kernel_value::ImportSlot,
    target: Option<(DocumentId, FunctionId, lash_kernel_value::ExportName)>,
}

/// Only a resolution shared by every loaded instance of a source unit is
/// immutable code content. Distinct fills of the same document stay on dispatch.
fn code_imports(
    root: DocumentId,
    documents: &BTreeMap<DocumentId, Arc<Document>>,
    functions: &BTreeSet<FunctionId>,
    instances: &BTreeMap<lash_kernel_value::LinkId, crate::interface::PreparedInstance>,
) -> BTreeMap<(Unit, lash_kernel_value::ImportSlot), (DocumentId, FunctionId)> {
    let mut units = BTreeMap::<_, Option<(DocumentId, FunctionId)>>::new();
    for instance in instances.values() {
        let Some(source) = documents.get(&instance.document) else {
            continue;
        };
        let owners = source
            .functions
            .iter()
            .filter_map(|(id, definition)| {
                #[cfg(test)]
                IMPORT_SOURCE_VISITS.with(|count| count.set(count.get() + 1));
                functions
                    .contains(id)
                    .then_some((Unit::Library(*id), &definition.body()?.imports))
            })
            .chain((instance.document == root).then_some((Unit::Main, &source.imports)));
        // Resolve a document's slots once, then project them to the units that
        // actually declare those Imports. Helpers without Imports add no work.
        let resolved: BTreeMap<_, _> = source
            .imports
            .keys()
            .map(|slot| {
                let target = (|| {
                    let crate::interface::PreparedSlot::Function { callable, export } =
                        instance.slots.get(slot)?
                    else {
                        return None;
                    };
                    if !functions.contains(&callable.function) {
                        return None;
                    }
                    let target = instances.get(&callable.link)?.document;
                    let document = documents.get(&target)?;
                    let descriptor = match (document.exports.get(&export.name)?, &export.member) {
                        (doc::Export::Function(function), None) => function,
                        (doc::Export::Type(ty), Some(member)) => match ty.members.get(member)? {
                            doc::TypeMember::Static(function)
                            | doc::TypeMember::Method(function)
                            | doc::TypeMember::Property(function) => function,
                        },
                        _ => return None,
                    };
                    let definition = document.functions.get(&callable.function)?;
                    // Other ABIs and nontrivial result fitting belong to the
                    // public call boundary, before any helper work. Lowering
                    // checks each site's argument kinds before using a hint.
                    if descriptor.function != callable.function
                        || descriptor.signature != definition.signature
                        || descriptor.convention != doc::StartConvention::Direct
                        || descriptor.asynchronous
                        || descriptor.rest
                        || super::import_result_check(&descriptor.signature.result).is_none()
                        || definition
                            .body()
                            .is_none_or(|body| !body.imports.is_empty())
                    {
                        return None;
                    }
                    Some((target, callable.function))
                })();
                (*slot, target)
            })
            .collect();
        for (unit, imports) in owners {
            for slot in imports.keys() {
                let target = resolved.get(slot).copied().flatten();
                units
                    .entry((unit.clone(), *slot))
                    .and_modify(|old| {
                        if *old != target {
                            *old = None;
                        }
                    })
                    .or_insert(target);
            }
        }
    }
    units
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?)))
        .collect()
}

struct PreparedCode {
    own: Arc<DocumentCode>,
    /// Fresh permission sets are immutable document content over this admission's
    /// exact helper image. Link membership is still checked by the executable.
    helper_reach: Box<[(DocumentId, Box<[FunctionId]>)]>,
    /// Present only if compilation extended the registry. Holding the original
    /// library here would cycle through its admission cache.
    library: Option<PreparedLibrary>,
}

pub(crate) enum Refused {
    Invalid(Invalid),
}

impl From<Refused> for StartError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

impl From<Refused> for OpenError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Invalid(invalid) => Self::NotAdmitted(invalid),
        }
    }
}

#[cfg(test)]
thread_local! {
    static IMPORT_SOURCE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DEFINITION_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DOCUMENT_ENCODINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn identify_document(document: &Document) -> DocumentId {
    #[cfg(test)]
    DOCUMENT_ENCODINGS.with(|count| count.set(count.get() + 1));
    document.identity()
}

impl PreparedLibrary {
    pub(crate) fn note_identified_document(&self, document: &doc::IdentifiedDocument) {
        let key = Arc::as_ptr(document.shared()) as usize;
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        documents
            .2
            .retain(|_, known| known.document.strong_count() != 0);
        documents.2.entry(key).or_insert_with(|| {
            Arc::new(ImmutableIdentity {
                document: Arc::downgrade(document.shared()),
                identity: OnceLock::from(document.id()),
            })
        });
    }

    pub(crate) fn document_identity(&self, document: &Arc<Document>) -> DocumentId {
        let key = Arc::as_ptr(document) as usize;
        let known = {
            let mut documents = self
                .0
                .documents
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(known) = documents.2.get(&key) {
                Arc::clone(known)
            } else {
                documents
                    .2
                    .retain(|_, known| known.document.strong_count() != 0);
                let known = Arc::new(ImmutableIdentity {
                    document: Arc::downgrade(document),
                    identity: OnceLock::new(),
                });
                documents.2.insert(key, Arc::clone(&known));
                known
            }
        };
        // Canonical encoding runs once for this live immutable allocation,
        // outside the library cache lock. Its result never crosses a mutation.
        *known.identity.get_or_init(|| identify_document(document))
    }

    /// Admission precedes compilation, preserving import's refusal order:
    /// document identity and pins are checked before missing body callees.
    pub(crate) fn admit(
        &self,
        document: &Arc<Document>,
        limits: KernelDataLimits,
        selection: &doc::Selection,
    ) -> Result<Arc<Admission>, Refused> {
        self.admit_as(document, None, limits, selection)
    }

    /// Records that the document of `identity` was validated against this
    /// library's functions under `limits`, so a later admission of it is not
    /// validated again (FIG-6931). Only a caller that validated that very
    /// document records it, as a dialect that validates while it lowers.
    pub fn note_validated(&self, identity: DocumentId, limits: KernelDataLimits) {
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if documents.1.len() >= VALIDATED {
            documents.1.clear();
        }
        documents
            .1
            .insert((identity, limits.document_depth(), self.helpers().identity()));
    }

    /// Admits `document`, whose identity is `identity`, ahead of its start
    /// under `limits`, and answers whether it is admitted: an identity
    /// validated against this library before is not validated again
    /// (FIG-6931). A refused document's start refuses it with its typed
    /// cause.
    pub fn admit_identified(
        &self,
        document: &Arc<Document>,
        identity: DocumentId,
        limits: KernelDataLimits,
        selection: &doc::Selection,
    ) -> bool {
        self.admit_as(document, Some(identity), limits, selection)
            .is_ok()
    }

    fn admit_as(
        &self,
        document: &Arc<Document>,
        identity: Option<DocumentId>,
        limits: KernelDataLimits,
        selection: &doc::Selection,
    ) -> Result<Arc<Admission>, Refused> {
        let mut documents = self
            .0
            .documents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Only document depth affects structural validation. Runtime value
        // and inline limits are applied by each run's heap, not admission.
        let key = (
            Arc::as_ptr(document) as usize,
            limits.document_depth(),
            self.helpers().identity(),
            selection.clone(),
        );
        if let Some(admitted) = documents.0.get(&key) {
            return Ok(Arc::clone(admitted));
        }
        // Reclaim preparation for documents no caller or machine retains.
        // Weak references keep their allocation addresses reserved until this
        // sweep, so even a dropped document can never produce a false hit.
        documents
            .0
            .retain(|_, admitted| admitted.document.strong_count() != 0);
        let validated = matches!(selection, doc::Selection::Fresh)
            && identity.is_some_and(|identity| {
                documents.1.contains(&(
                    identity,
                    limits.document_depth(),
                    self.helpers().identity(),
                ))
            });
        if !validated {
            match selection {
                doc::Selection::Fresh => {
                    for function in document.library.iter() {
                        if !self.helpers().contains(function) {
                            return Err(Refused::Invalid(lash_kernel_doc::Invalid {
                                site: None,
                                reason: Box::new(
                                    lash_kernel_doc::InvalidReason::FunctionNotResolved {
                                        function: *function,
                                    },
                                ),
                            }));
                        }
                    }
                    validate_document_with_limits(document, self.registry().as_ref(), limits)
                        .map_err(Refused::Invalid)?;
                    if let Some(identity) = identity {
                        if documents.1.len() >= VALIDATED {
                            documents.1.clear();
                        }
                        documents.1.insert((
                            identity,
                            limits.document_depth(),
                            self.helpers().identity(),
                        ));
                    }
                }
                doc::Selection::Restored(restored) => {
                    doc::validate_restored_document_with_limits(
                        document,
                        self.helpers(),
                        self.registry().as_ref(),
                        restored,
                        limits,
                    )
                    .map_err(Refused::Invalid)?;
                }
            }
        }
        let admitted = Arc::new(Admission {
            selection: selection.clone(),
            document: Arc::downgrade(document),
            identity: identity.map_or_else(OnceLock::new, OnceLock::from),
            code: Mutex::new(BTreeMap::new()),
        });
        documents.0.insert(key, Arc::clone(&admitted));
        Ok(admitted)
    }
}

impl Admission {
    pub(crate) fn identity(&self, document: &Document) -> DocumentId {
        *self.identity.get_or_init(|| document.identity())
    }

    pub(crate) fn executable(
        self: &Arc<Self>,
        document: &Arc<Document>,
        library: &PreparedLibrary,
        documents: &BTreeMap<DocumentId, Arc<Document>>,
        functions: &BTreeSet<lash_kernel_value::FunctionId>,
        instances: &BTreeMap<
            lash_kernel_value::content_ids::LinkId,
            crate::interface::PreparedInstance,
        >,
    ) -> Result<Arc<Executable>, Missing> {
        let content = CodeContent {
            documents: documents.keys().copied().collect(),
            functions: functions.iter().copied().collect(),
            imports: instances
                .values()
                .flat_map(|instance| {
                    instance.slots.iter().map(|(slot, prepared)| {
                        let target = match prepared {
                            crate::interface::PreparedSlot::Function { callable, export } => {
                                instances.get(&callable.link).map(|target| {
                                    (target.document, callable.function, export.clone())
                                })
                            }
                            _ => None,
                        };
                        ImportResolution {
                            source: instance.document,
                            slot: *slot,
                            target,
                        }
                    })
                })
                .collect(),
        };
        let code = {
            let mut compiled = self.code.lock().unwrap_or_else(|error| error.into_inner());
            Arc::clone(compiled.entry(content).or_default())
        };
        // One producer compiles this content; no admission map lock is held
        // while registry preparation or compilation runs.
        let code = code
            .get_or_init(|| {
                let imports =
                    code_imports(self.identity(document), documents, functions, instances);
                Self::prepare_code(
                    document,
                    library,
                    documents,
                    functions,
                    &imports,
                    &self.selection,
                )
            })
            .as_ref()
            .map_err(|missing| *missing)?;
        let library = code.library.as_ref().unwrap_or(library);
        // These facts belong to this exact immutable instance set, selection
        // and helper image. Never place them in DocumentCode, whose content can
        // be shared by different operation Links and restore selections.
        let mut helper_origins = BTreeMap::new();
        let mut library_reached = BTreeSet::new();
        for (link, instance) in instances {
            let Some(source) = documents.get(&instance.document) else {
                continue;
            };
            let mut reached = BTreeSet::new();
            match &self.selection {
                doc::Selection::Fresh => {
                    if let Ok(at) = code
                        .helper_reach
                        .binary_search_by_key(&instance.document, |(id, _)| *id)
                    {
                        reached.extend(code.helper_reach[at].1.iter().copied());
                    }
                }
                doc::Selection::Restored(selected) => {
                    reached.extend(selected.live.iter().filter_map(|callable| {
                        (callable.link == *link
                            && selected.pins.contains(&callable.function)
                            && library.helpers().contains(&callable.function))
                        .then_some(callable.function)
                    }));
                }
            }
            library_reached.extend(source.functions.keys().copied());
            library_reached.extend(reached.iter().copied());
            helper_origins.insert(*link, reached);
        }
        // This walk has a different duty: authored library frames can stand
        // in non-native callees even when those are not helper-image members.
        let mut pending: Vec<_> = document.functions.keys().copied().collect();
        let mut authored = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !authored.insert(id) {
                continue;
            }
            if let Some(registered) = library.registry().get(&id)
                && registered.native().is_none()
            {
                pending.extend(registered.callees().iter().copied());
            }
        }
        library_reached.extend(authored);
        Ok(Arc::new(Executable {
            origin_cache_bytes: (std::mem::size_of_val(code.helper_reach.as_ref()) as u64)
                .saturating_add(
                    code.helper_reach
                        .iter()
                        .map(|(_, ids)| std::mem::size_of_val(ids.as_ref()) as u64)
                        .sum::<u64>(),
                ),
            helper_origins: helper_origins
                .into_iter()
                .map(|(link, reached)| (link, reached.into_iter().collect()))
                .collect(),
            library_reached: library_reached.into_iter().collect(),
            code_base: library.0.tables.codes.len(),
            block_base: library.0.tables.blocks.len(),
            stmt_base: library.0.tables.stmts.len(),
            instances: instances
                .iter()
                .filter_map(|(link, instance)| {
                    documents
                        .get(&instance.document)
                        .map(|doc| (*link, Arc::clone(doc)))
                })
                .collect(),
            library: library.clone(),
            document: Arc::clone(document),
            own: Arc::clone(&code.own),
            admission: Arc::clone(self),
        }))
    }

    fn prepare_code(
        document: &Document,
        library: &PreparedLibrary,
        documents: &BTreeMap<DocumentId, Arc<Document>>,
        functions: &BTreeSet<FunctionId>,
        imports: &BTreeMap<(Unit, lash_kernel_value::ImportSlot), (DocumentId, FunctionId)>,
        selection: &doc::Selection,
    ) -> Result<PreparedCode, Missing> {
        // Link tables are instance-specific; compile the reached documents together,
        // deduplicating identical FunctionIds while keeping frame Links separate.
        let mut reached = BTreeMap::new();
        for source in std::iter::once(document).chain(documents.values().map(Arc::as_ref)) {
            for (id, definition) in &source.functions {
                if functions.contains(id) {
                    reached.insert(*id, definition);
                }
            }
        }
        // Authored frames follow executable guest roots. A native or formula
        // helper's retained fallback definitions remain owned by the library;
        // compiling them again would create dormant charged regions.
        let mut pending = document.main.as_ref().map_or_else(BTreeSet::new, |body| {
            doc::Node::Block(body).functions_within()
        });
        // Saved cell values enter through the heap, not the current main.
        // Their defining cells still own the authored function layouts.
        for main in documents.values().filter_map(|source| source.main.as_ref()) {
            pending.extend(doc::Node::Block(main).functions_within());
        }
        // Definitions first admitted by this execution include retained
        // callable environments, even when its main does not name them.
        pending.extend(
            reached
                .keys()
                .filter(|id| library.registry().get(id).is_none())
                .copied(),
        );
        // Exported lexical owners can be outside the executable selection when
        // only their frozen child is started. Walk their authored descendants;
        // compile_document still emits only definitions present in `reached`.
        for loaded in documents.values() {
            for export in loaded.exports.values() {
                match export {
                    doc::Export::Function(function) => {
                        pending.insert(function.function);
                    }
                    doc::Export::Type(ty) => {
                        pending.insert(ty.checker);
                        for member in ty.members.values() {
                            let (doc::TypeMember::Static(function)
                            | doc::TypeMember::Method(function)
                            | doc::TypeMember::Property(function)) = member;
                            pending.insert(function.function);
                        }
                    }
                    doc::Export::Error { .. } => {}
                }
            }
        }
        let mut authored = BTreeSet::new();
        while let Some(id) = pending.pop_first() {
            // Classification follows the retained lexical owner even when
            // that owner is outside selected execution reach. A frozen child
            // still needs its authored free-variable layout; compile_document
            // below only emits definitions in `reached`.
            let Some(definition) = std::iter::once(document)
                .chain(documents.values().map(Arc::as_ref))
                .find_map(|source| source.functions.get(&id))
            else {
                continue;
            };
            let doc::Implementation::Guest {
                body,
                metering: doc::Metering::Statements,
            } = &definition.implementation
            else {
                continue;
            };
            if authored.insert(id) {
                pending.extend(doc::Node::Block(&body.block).functions_within());
            }
        }
        let loaded = if reached
            .keys()
            .any(|id| library.registry().get(id).is_none())
        {
            let mut registry = library.registry().as_ref().clone();
            let mut pending: BTreeSet<_> = reached
                .keys()
                .filter(|id| registry.get(id).is_none())
                .copied()
                .collect();
            while let Some(blocked) = pending.first().copied() {
                let ready =
                    pending.iter().copied().find(|id| {
                        reached[id].body().is_none_or(|body| {
                            doc::Node::Block(&body.block).functions_within().iter().all(
                                |dependency| dependency == id || registry.get(dependency).is_some(),
                            )
                        })
                    });
                let id = ready.ok_or(Missing(blocked))?;
                #[cfg(test)]
                DEFINITION_COPIES.with(|count| count.set(count.get() + 1));
                registry
                    .register(reached[&id].clone(), None)
                    .map_err(|_| Missing(id))?;
                pending.remove(&id);
            }
            Some(
                PreparedLibrary::with_layout_and_charge(
                    Arc::new(registry),
                    library.0.layout,
                    library.0.charge,
                )
                .with_helper_handle(Arc::clone(library.helpers())),
            )
        } else {
            None
        };
        let library = loaded.as_ref().unwrap_or(library);
        let mut own = compile_document(document, &reached, library, &authored, imports)?;
        if matches!(selection, doc::Selection::Restored(_)) {
            // These are the original, already decoded ordinary definitions.
            // Census does not admit/register them or consult helper bodies.
            // Their historical rows remain valid after executable reach narrows.
            let mut seen = BTreeSet::new();
            let sites = std::iter::once(document)
                .chain(documents.values().map(Arc::as_ref))
                .flat_map(|source| {
                    source
                        .functions
                        .iter()
                        .filter_map(|(id, definition)| {
                            if !seen.insert(*id) {
                                return None;
                            }
                            lazy::body_census(definition, &source.functions)
                        })
                        .collect::<Vec<_>>()
                })
                .flat_map(|census| census.occurrences)
                .filter(|site| !library.0.tables.occurrence_at.contains_key(site));
            own.tables
                .retain_occurrences(sites, library.0.tables.occurrence_keys.len());
        }
        let own = Arc::new(own);
        let helper_reach: Box<[(DocumentId, Box<[FunctionId]>)]> =
            if matches!(selection, doc::Selection::Fresh) {
                documents
                    .iter()
                    .map(|(id, source)| {
                        (
                            *id,
                            library
                                .helpers()
                                .admitted(&source.library)
                                .into_iter()
                                .collect(),
                        )
                    })
                    .collect()
            } else {
                Box::new([])
            };
        Ok(PreparedCode {
            helper_reach,
            own,
            library: loaded,
        })
    }
}

impl Executable {
    pub(crate) fn identity(&self) -> DocumentId {
        self.admission.identity(&self.document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PreparedInstance, PreparedSlot};
    use {
        lash_kernel_value::ContractId, lash_kernel_value::Import, lash_kernel_value::ImportKind,
        lash_kernel_value::ImportSlot, lash_kernel_value::OperationRequirement,
        lash_kernel_value::OperationUse, lash_kernel_value::QualifiedName,
        lash_kernel_value::Signature, lash_kernel_value::Type, lash_kernel_value::package::Fill,
        lash_kernel_value::package::Link,
    };

    /// A closure creation before the saved continuation can refer to a helper
    /// omitted by the live proof. Compiling retained Main bytes must not panic.
    #[test]
    fn unselected_removed_closure_does_not_panic_during_preparation() {
        let missing = FunctionId::from_bytes([41; 32]);
        let mut document = Document::new(
            doc::NumberPolicy::BySpelling,
            vec![doc::Stmt::Print {
                value: doc::Expr::Closure(Box::new(doc::Closure {
                    function: missing,
                    presentation: None,
                })),
            }],
        );
        document.library.insert(missing);
        let library = PreparedLibrary::new(Arc::new(doc::FunctionRegistry::declarations()));
        let code = compile_document(
            &document,
            &BTreeMap::new(),
            &library,
            &BTreeSet::new(),
            &BTreeMap::new(),
        );
        assert!(
            code.is_ok(),
            "dead closure code is outside the restored executable selection"
        );
    }

    /// FIG-7135: starts share immutable import-source facts, and selecting one
    /// function never copies the unrelated retained library definitions.
    #[test]
    fn linked_preparation_work_is_bounded_by_selected_content() {
        let mut root = Document::new(doc::NumberPolicy::BySpelling, vec![]);
        for index in 0..64 {
            let definition = doc::parse_definition(&format!(
                "function retained_{index}() -> Null\nkernel 1\ncharge 1\nbody {{ return null }}"
            ))
            .expect("definition");
            root.functions.insert(definition.identity(), definition);
        }
        let function = *root.functions.keys().next().expect("nonempty fixture");
        let root = Arc::new(root);
        let identity = root.identity();
        let documents = BTreeMap::from([(identity, Arc::clone(&root))]);
        let link = Link {
            helper_fills: Default::default(),
            document: identity,
            fills: BTreeMap::new(),
        }
        .identity();
        let instances = BTreeMap::from([(
            link,
            PreparedInstance {
                helper_operations: Default::default(),
                link,
                document: identity,
                slots: BTreeMap::new(),
            },
        )]);
        let library = PreparedLibrary::new(Arc::new(doc::FunctionRegistry::default()));
        let admission = library
            .admit(&root, KernelDataLimits::standard(), &doc::Selection::Fresh)
            .unwrap_or_else(|_| panic!("valid root"));
        IMPORT_SOURCE_VISITS.with(|count| count.set(0));
        DEFINITION_COPIES.with(|count| count.set(0));
        for _ in 0..32 {
            admission
                .executable(
                    &root,
                    &library,
                    &documents,
                    &BTreeSet::from([function]),
                    &instances,
                )
                .unwrap_or_else(|missing| panic!("selected executable: {}", missing.0));
        }
        let visits = IMPORT_SOURCE_VISITS.with(|count| count.get());
        let copies = DEFINITION_COPIES.with(|count| count.get());
        assert!(
            visits <= 64 && copies <= 1,
            "selected preparation repeated source work: {visits} import-source visits, {copies} definition copies"
        );
    }

    /// FIG-7109: repointing an import of the same retained documents and
    /// function selection cannot reuse the target its helper compiled to.
    #[test]
    fn an_imported_helper_tracks_the_resolved_function() {
        use crate::laws::embedder::{ROOMY, Setup, World, start_machine};
        use crate::{End, Machine, Program, Step};
        use lash_kernel_doc::plan::{HelperPlan, HelperTemplate};
        let fixture = crate::laws::embedder::library(true);
        let mut registry = (*fixture.registry).clone();
        let mut definitions = BTreeMap::new();
        let mut ids = Vec::new();
        for (name, answer, entry) in [
            (
                "law.first",
                1,
                (|_: &[doc::Value], _: &[doc::PreparedNative]| Some(doc::Value::Int(1)))
                    as doc::plan::ImmediateEntry,
            ),
            (
                "law.second",
                2,
                (|_: &[doc::Value], _: &[doc::PreparedNative]| Some(doc::Value::Int(2)))
                    as doc::plan::ImmediateEntry,
            ),
        ] {
            let definition = doc::parse_definition(&format!(
                "function {name}(x: Any) -> Any\nkernel 1\ncharge 3\nbody {{ return {answer} }}"
            ))
            .expect("helper");
            let id = registry
                .register(definition.clone(), None)
                .expect("register helper");
            definitions.insert(id, definition);
            ids.push(id);
            registry
                .install_plans(&[Box::leak(Box::new(HelperPlan {
                    name,
                    function: id,
                    callees: &[],
                    literals: &[],
                    fields: &[],
                    records: &[],
                    registers: 1,
                    slots: &[],
                    closures: &[],
                    sites: &[],
                    run: |_, _| unreachable!("the law uses the certified immediate path"),
                    template: Some(HelperTemplate {
                        metered: None,
                        entry: Some(entry),
                        callees: &[],
                        depth: 1,
                        heap: None,
                        forwards: Default::default(),
                    }),
                }))])
                .expect("install template");
        }
        let mut helpers = Document::new(doc::NumberPolicy::BySpelling, vec![]);
        helpers.main = None;
        helpers.functions = definitions;
        for (name, id) in ["first", "second"].into_iter().zip(&ids) {
            helpers.exports.insert(
                Name::new(name),
                doc::Export::Function(doc::FunctionExport {
                    function: *id,
                    signature: registry
                        .get(id)
                        .expect("helper")
                        .definition()
                        .signature
                        .clone(),
                    convention: doc::StartConvention::Direct,
                    asynchronous: false,
                    rest: false,
                }),
            );
        }
        let helper_link = Link {
            helper_fills: Default::default(),
            document: helpers.identity(),
            fills: BTreeMap::new(),
        };
        let mut root = crate::laws::embedder::document(
            "kernel 1\nnumbers by_spelling\nimport 0 code law.helper(x: Any) -> Any direct\nmain { let i = 0 let s = 0 while num.lt(i, 4) { let x = invoke import 0(i) set s = num.add(s, x) set i = num.add(i, 1) } return s }",
            &registry,
        );
        root.imports.retain(|slot, _| *slot == ImportSlot(0));
        let root = Arc::new(root);
        let documents = BTreeMap::from([
            (root.identity(), Arc::clone(&root)),
            (helpers.identity(), Arc::new(helpers)),
        ]);
        let library = PreparedLibrary::new(Arc::new(registry));
        let mut previous = None;
        for (name, expected) in [("first", 4), ("second", 8)] {
            let link = Link {
                helper_fills: Default::default(),
                document: root.identity(),
                fills: BTreeMap::from([(
                    ImportSlot(0),
                    Fill::Export {
                        link: helper_link.identity(),
                        export: lash_kernel_value::ExportName {
                            name: Name::new(name),
                            member: None,
                        },
                    },
                )]),
            };
            let program = Program::prepare(
                link.identity(),
                documents.clone(),
                &BTreeMap::from([
                    (link.identity(), link),
                    (helper_link.identity(), helper_link.clone()),
                ]),
                doc::heap::PageId([8; 32]),
                library.clone(),
                lash_kernel_doc::Selection::Fresh,
            )
            .expect("linked program");
            let unused = ids[usize::from(name == "first")];
            assert!(
                !program.functions.contains(&unused),
                "retaining a helper document made its unselected export executable"
            );
            let admission = library
                .admit(&root, KernelDataLimits::standard(), &doc::Selection::Fresh)
                .unwrap_or_else(|_| panic!("admitted"));
            let exe = admission
                .executable(
                    &root,
                    &library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .unwrap_or_else(|_| panic!("compiled"));
            if let Some(previous) = previous.replace(Arc::clone(&exe.own)) {
                assert!(
                    !Arc::ptr_eq(&previous, &exe.own),
                    "a repointed import recompiles even with the same retained documents and functions"
                );
            }
            let mut machine = start_machine(program, ROOMY, Setup::default().start).expect("start");
            match machine.run(&mut World::default(), u64::MAX).expect("run") {
                Step::Ended(End::Finished(finished)) => {
                    assert_eq!(finished.result, doc::Datum::Int(expected.into()))
                }
                other => panic!("helper loop did not finish: {other:?}"),
            }
        }
    }

    /// Immutable identity validation does no repeated token encoding, and
    /// changing either the root or a retained Document must still refuse.
    #[test]
    fn linked_validation_identifies_immutable_documents_once_and_rejects_mutation() {
        let document = Arc::new(Document::new(doc::NumberPolicy::BySpelling, vec![]));
        let identity = document.identity();
        let link = Link {
            helper_fills: Default::default(),
            document: identity,
            fills: BTreeMap::new(),
        };
        let root = link.identity();
        let library = PreparedLibrary::new(Arc::new(doc::FunctionRegistry::default()));
        let program = crate::Program::prepare(
            root,
            BTreeMap::from([(identity, document)]),
            &BTreeMap::from([(root, link)]),
            doc::heap::PageId([3; 32]),
            library,
            lash_kernel_doc::Selection::Fresh,
        )
        .expect("valid immutable program");
        DOCUMENT_ENCODINGS.with(|count| count.set(0));
        program.validate_links().expect("same first start");
        program.validate_links().expect("same next start");
        assert_eq!(
            DOCUMENT_ENCODINGS.with(|count| count.replace(0)),
            0,
            "validating unchanged linked content encoded its Documents again"
        );
        let mut changed = program.clone();
        Arc::make_mut(&mut changed.document)
            .private_bindings
            .insert(lash_kernel_value::Name::new("changed"));
        assert!(
            changed.validate_links().is_err(),
            "root mutation reused an old identity"
        );
        let mut changed = program.clone();
        Arc::make_mut(
            changed
                .documents
                .get_mut(&identity)
                .expect("retained document"),
        )
        .private_bindings
        .insert(lash_kernel_value::Name::new("changed"));
        assert!(
            changed.validate_links().is_err(),
            "retained mutation reused an old identity"
        );
    }

    /// FIG-6931's compile-once rule survives linked preparation: equal code
    /// content shares immutable tables, while each execution keeps its own
    /// operation uses. Selected function sets remain part of the code key.
    #[test]
    fn immutable_linked_code_is_compiled_once_and_instance_links_stay_distinct() {
        let mut document = Document::new(doc::NumberPolicy::BySpelling, vec![]);
        let signature = Signature {
            params: vec![],
            result: Type::Null,
        };
        document.imports.insert(
            ImportSlot(0),
            Import {
                name: QualifiedName {
                    package: lash_kernel_value::PackagePath::parse("host").expect("package"),
                    member: lash_kernel_value::Segment::new("tick").expect("member"),
                    type_member: None,
                },
                kind: ImportKind::Operation {
                    signature: signature.clone(),
                },
            },
        );
        let definition = doc::parse_definition(
            "function retained() -> Null\nkernel 1\nmetering statements\nbody { return null }",
        )
        .expect("retained function");
        let function = definition.identity();
        document.functions.insert(function, definition);
        let document = Arc::new(document);
        let identity = document.identity();
        let documents = BTreeMap::from([(identity, Arc::clone(&document))]);
        let functions = BTreeSet::from([function]);
        let mut instances = Vec::new();
        for contract in ["first", "second"] {
            let link = Link {
                helper_fills: Default::default(),
                document: identity,
                fills: BTreeMap::from([(
                    ImportSlot(0),
                    Fill::Operation(OperationRequirement::new(
                        signature.clone(),
                        ContractId::new(contract),
                    )),
                )]),
            };
            let link = link.identity();
            instances.push(BTreeMap::from([(
                link,
                PreparedInstance {
                    helper_operations: Default::default(),
                    link,
                    document: identity,
                    slots: BTreeMap::from([(
                        ImportSlot(0),
                        PreparedSlot::Operation(OperationUse {
                            link,
                            slot: (ImportSlot(0)).into(),
                        }),
                    )]),
                },
            )]));
        }
        let library = PreparedLibrary::new(Arc::new(doc::FunctionRegistry::default()));
        let admission = library
            .admit(
                &document,
                KernelDataLimits::standard(),
                &doc::Selection::Fresh,
            )
            .unwrap_or_else(|_| panic!("valid linked code"));
        let first = admission
            .executable(&document, &library, &documents, &functions, &instances[0])
            .unwrap_or_else(|missing| panic!("first executable: {}", missing.0));
        let second = admission
            .executable(&document, &library, &documents, &functions, &instances[1])
            .unwrap_or_else(|missing| panic!("same code under another Link: {}", missing.0));
        assert!(
            Arc::ptr_eq(&first.own, &second.own),
            "immutable linked code was compiled again"
        );
        assert_eq!(
            first.instances.keys().copied().collect::<Vec<_>>(),
            instances[0].keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(
            second.instances.keys().copied().collect::<Vec<_>>(),
            instances[1].keys().copied().collect::<Vec<_>>()
        );
        assert_ne!(
            first.instances.keys().next(),
            second.instances.keys().next()
        );
        let narrowed = admission
            .executable(
                &document,
                &library,
                &documents,
                &BTreeSet::new(),
                &instances[0],
            )
            .unwrap_or_else(|missing| {
                panic!("an execution without the retained function: {}", missing.0)
            });
        assert!(first.own.declared.contains_key(&function));
        assert!(!narrowed.own.declared.contains_key(&function));
        assert!(!Arc::ptr_eq(&first.own, &narrowed.own));
        let lifetime = Arc::downgrade(&document);
        drop((first, second, narrowed, admission, documents, document));
        assert!(
            lifetime.upgrade().is_none(),
            "code preparation must not retain its source Document"
        );
    }
}
