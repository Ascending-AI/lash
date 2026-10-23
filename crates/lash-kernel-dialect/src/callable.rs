//! Pure source operations over one callable and its frozen external bindings.
use crate::{
    Diagnostic, GeneratedSourceLimits, Library, PrintError, PrintOptions, PrintedSource, Written,
    workflow::WorkflowSourceLimits,
};
use lash_kernel_doc::{
    CallablePresentation, Captions, Correspondence, DocumentBytes, FunctionValue,
};
use lash_vocabulary::{FunctionId, Name, sites::Site};
use serde::{Deserialize, Serialize};
mod bindings;
pub use bindings::{aliases, entry_name, restore};
mod equivalence;
pub use equivalence::equivalent;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallableSourceBase {
    #[serde(with = "document_bytes")]
    pub document: DocumentBytes,
    pub function: FunctionValue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallableSourceRead {
    #[serde(with = "document_bytes")]
    pub document: DocumentBytes,
    pub function: FunctionId,
    pub creation: Option<Site>,
    pub presentation: CallablePresentation,
    pub written: Written,
    pub captions: Captions,
    pub correspondence: Correspondence,
    pub required_captures: BTreeSet<Name>,
}
pub trait CallableSourceCodec {
    fn print(
        &self,
        base: &CallableSourceBase,
        library: &dyn Library,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError>;
    fn read(
        &self,
        source: &str,
        base: &CallableSourceBase,
        library: &dyn Library,
        limits: GeneratedSourceLimits,
        policy: Option<WorkflowSourceLimits>,
    ) -> Result<CallableSourceRead, CallableReadError>;
}
#[derive(Debug, thiserror::Error)]
pub enum CallableReadError {
    #[error("source refused: {0:?}")]
    Source(#[from] Diagnostic),
    #[error(transparent)]
    Edit(#[from] lash_kernel_doc::CallableEditRefusal),
}
mod document_bytes {
    use super::*;
    use serde::{Deserializer, Serializer};
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        grammar: u32,
        id: lash_vocabulary::DocumentId,
        body: Vec<u8>,
    }
    pub fn serialize<S: Serializer>(
        value: &DocumentBytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Wire {
            grammar: value.grammar_version(),
            id: value.id(),
            body: value.as_bytes().to_vec(),
        }
        .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DocumentBytes, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        Ok(DocumentBytes::from_parts(wire.grammar, wire.id, wire.body))
    }
}

/// The selected callable's original, deterministic free binding names.
pub fn required(
    document: &lash_kernel_doc::Document,
    function: FunctionId,
) -> Result<BTreeSet<Name>, lash_kernel_doc::CallableEditRefusal> {
    let definition = document
        .functions
        .get(&function)
        .ok_or(lash_kernel_doc::CallableEditRefusal::EntryMissing)?;
    let body = definition
        .body()
        .ok_or(lash_kernel_doc::CallableEditRefusal::EntryMissing)?;
    let params = definition
        .signature
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect::<Vec<_>>();
    Ok(lash_kernel_doc::snapshot::CodeReads::of(&params, &body.block, &document.functions).free)
}

/// Validate the bounded edit before replacing the selected declaration. The
/// returned value contains no replacement captures or provenance.
pub fn patch(
    base: &CallableSourceBase,
    lowered: crate::Lowered,
    creation: Site,
    baseline: &crate::Lowered,
) -> Result<CallableSourceRead, CallableReadError> {
    use k::CallableEditRefusal as Refusal;
    use lash_kernel_doc as k;
    let original = base
        .document
        .to_document()
        .map_err(|e| Diagnostic::from(PrintError::Invalid(e.to_string())))?;
    let Some(k::Node::Expr(k::Expr::Closure(closure))) = lowered.document.node(&creation) else {
        return Err(Refusal::EntryMissing.into());
    };
    let new_id = closure.function;
    let presentation = closure
        .presentation
        .clone()
        .ok_or(Refusal::EntryShapeChanged)?;
    if presentation.signature.params.len() != base.function.signature.params.len()
        || presentation.convention != base.function.convention
        || presentation.representation != base.function.representation
    {
        return Err(Refusal::EntryShapeChanged.into());
    }
    let written = lowered
        .written
        .get(&creation)
        .cloned()
        .ok_or(Refusal::EntryMissing)?;
    let location = k::FunctionLocation {
        name: None,
        site: Some(creation.clone()),
    };
    if let Some(origin) = &written.signature_origin {
        origin
            .require(&presentation.signature, location)
            .map_err(|error| match error {
                lash_vocabulary::EntrySignatureRefusal::Required(required) => {
                    Diagnostic::from(*required)
                }
                lash_vocabulary::EntrySignatureRefusal::Annotation(refused) => Diagnostic {
                    code: "ENTRY_ANNOTATION_REFUSED".into(),
                    span: refused.span,
                    fact: Some(Box::new(crate::DiagnosticFact::EntryAnnotationRefused(
                        *refused,
                    ))),
                    ..Default::default()
                },
            })?;
    } else {
        k::require_entry_signature(&presentation.signature, location)
            .map_err(|e| Diagnostic::from(*e))?;
    }
    let before = required(&original, base.function.callable.function)?;
    let after = required(&lowered.document, new_id)?;
    let added = after.difference(&before).cloned().collect::<BTreeSet<_>>();
    let removed = before.difference(&after).cloned().collect::<BTreeSet<_>>();
    if !added.is_empty()
        || !removed.is_empty()
        || after
            .iter()
            .any(|name| !base.function.captures.contains_key(name))
    {
        return Err(Refusal::CaptureSetChanged { added, removed }.into());
    }
    // Each function owns its slots. A fragment may use an existing contract;
    // it cannot change that contract or allocate another one.
    for import in lowered.document.imports.values().chain(
        lowered
            .document
            .functions
            .values()
            .filter_map(|f| f.body())
            .flat_map(|b| b.imports.values()),
    ) {
        if !original
            .imports
            .values()
            .chain(
                original
                    .functions
                    .values()
                    .filter_map(|f| f.body())
                    .flat_map(|b| b.imports.values()),
            )
            .any(|old| old == import)
        {
            return Err(Refusal::ImportsChanged.into());
        }
    }
    let sites = all_sites(&original);
    let identity = equivalent(
        &original,
        base.function.callable.function,
        &lowered.document,
        new_id,
    );
    let presentation_unchanged = presentation.signature == base.function.signature;
    if identity.is_some() && presentation_unchanged {
        let correspondence =
            Correspondence::identity(original.identity(), sites).ok_or(Refusal::EntryMissing)?;
        return Ok(CallableSourceRead {
            document: base.document.clone(),
            function: base.function.callable.function,
            creation: base.function.creation.clone(),
            presentation,
            written,
            captions: read_captions(
                base,
                &original,
                &original,
                &lowered,
                baseline,
                &identity.unwrap_or_default(),
                &correspondence,
            )?,
            correspondence,
            required_captures: after,
        });
    }
    let mut document = original.clone();
    let mut additions = lowered.document.document().functions.clone();
    let replacement = additions.remove(&new_id).ok_or(Refusal::EntryMissing)?;
    let old_id = base.function.callable.function;
    document
        .library
        .0
        .extend(lowered.document.library.iter().copied());
    document
        .requires
        .natives
        .extend(lowered.document.requires.natives.iter().copied());
    document.functions.extend(additions);
    document.functions.insert(old_id, replacement);
    // Update presentation at the original creation sites. Code outside the
    // selected declaration is retained and reidentified bottom-up.
    for block in document.main.iter_mut().chain(
        document
            .functions
            .values_mut()
            .filter_map(|f| f.body_mut())
            .map(|body| &mut body.block),
    ) {
        update_presentation(k::NodeMut::Block(block), old_id, &presentation);
    }
    prune_replaced_children(&original, old_id, &mut document);
    let reidentified = document
        .reidentify_functions()
        .map_err(|e| Diagnostic::from(PrintError::Invalid(e)))?;
    let function = reidentified
        .get(&old_id)
        .copied()
        .ok_or(Refusal::EntryMissing)?;
    let map_site = |site: &Site| {
        let mut site = site.clone();
        if let lash_vocabulary::sites::Unit::Library(id) = &mut site.unit {
            *id = reidentified.get(id).copied().unwrap_or(*id);
        }
        site
    };
    let entries = sites
        .into_iter()
        .filter_map(|from| {
            let to = map_site(&from);
            // Only equal surviving subtrees retain a caption. A removed node has
            // no successor; never attach its sidecar to a new statement there.
            (original.node(&from) == document.node(&to)
                || (from.unit == lash_vocabulary::sites::Unit::Library(old_id)
                    && from.path.is_empty()))
            .then(|| k::Survivor::new(from, to, false))
        })
        .collect();
    let correspondence = Correspondence::of(original.identity(), document.identity(), entries)
        .ok_or(Refusal::EntryMissing)?;
    let mut read_functions = reidentified.clone();
    read_functions.insert(new_id, function);
    let captions = read_captions(
        base,
        &original,
        &document,
        &lowered,
        baseline,
        &read_functions,
        &correspondence,
    )?;
    Ok(CallableSourceRead {
        creation: base.function.creation.as_ref().map(map_site),
        document: document.into(),
        function,
        presentation,
        written,
        captions,
        correspondence,
        required_captures: after,
    })
}
/// Retire only lexical descendants of the replaced declaration that no other
/// surviving code references. An obsolete closure must not become a free-standing
/// function checked without the scope that originally bound its captures.
fn prune_replaced_children(
    original: &lash_kernel_doc::Document,
    root: FunctionId,
    document: &mut lash_kernel_doc::Document,
) {
    use lash_kernel_doc as k;
    fn children(document: &k::Document, id: FunctionId) -> BTreeSet<FunctionId> {
        let mut out = BTreeSet::new();
        let Some(body) = document.functions.get(&id).and_then(|f| f.body()) else {
            return out;
        };
        let mut pending = vec![k::Node::Block(&body.block)];
        while let Some(node) = pending.pop() {
            if let k::Node::Expr(k::Expr::Closure(closure)) = node {
                out.insert(closure.function);
            }
            pending.extend(node.children());
        }
        out
    }
    let mut candidates = BTreeSet::new();
    let mut pending = children(original, root).into_iter().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if id != root && candidates.insert(id) {
            pending.extend(children(original, id));
        }
    }
    let mut referenced = BTreeSet::new();
    for (unit, block) in document.units() {
        if !matches!(unit, lash_vocabulary::sites::Unit::Library(id) if candidates.contains(&id)) {
            referenced.extend(k::Node::Block(block).functions_within());
        }
    }
    for export in document.exports.values() {
        match export {
            k::Export::Function(function) => {
                referenced.insert(function.function);
            }
            k::Export::Type(ty) => {
                referenced.insert(ty.checker);
                referenced.extend(ty.members.values().map(|member| member.function().function));
            }
            k::Export::Error { .. } => {}
        }
    }
    let mut pending = referenced.iter().copied().collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if let Some(body) = document.functions.get(&id).and_then(|f| f.body()) {
            for child in k::Node::Block(&body.block).functions_within() {
                if referenced.insert(child) {
                    pending.push(child);
                }
            }
        }
    }
    document
        .functions
        .retain(|id, _| !candidates.contains(id) || referenced.contains(id));
}

fn update_presentation(
    node: lash_kernel_doc::NodeMut<'_>,
    function: FunctionId,
    presentation: &CallablePresentation,
) {
    let mut node = node;
    if let lash_kernel_doc::NodeMut::Expr(lash_kernel_doc::Expr::Closure(closure)) = &mut node
        && closure.function == function
    {
        closure.presentation = Some(presentation.clone());
    }

    for child in node.children() {
        update_presentation(child, function, presentation);
    }
}
pub fn all_sites(document: &lash_kernel_doc::Document) -> Vec<Site> {
    fn walk(node: lash_kernel_doc::Node<'_>, site: Site, out: &mut Vec<Site>) {
        out.push(site.clone());
        for (index, child) in node.children().into_iter().enumerate() {
            walk(child, site.child(index as u32), out);
        }
    }
    let mut out = Vec::new();
    for (unit, block) in document.units() {
        walk(
            lash_kernel_doc::Node::Block(block),
            Site::new(unit, []),
            &mut out,
        );
    }
    out
}

fn read_captions(
    base: &CallableSourceBase,
    original: &lash_kernel_doc::Document,
    document: &lash_kernel_doc::Document,
    lowered: &crate::Lowered,
    baseline: &crate::Lowered,
    read_functions: &std::collections::BTreeMap<FunctionId, FunctionId>,
    correspondence: &Correspondence,
) -> Result<Captions, CallableReadError> {
    use lash_kernel_doc as k;
    fn root(lowered: &crate::Lowered) -> Option<FunctionId> {
        lowered
            .written
            .keys()
            .filter(|s| s.unit == lash_vocabulary::sites::Unit::Main)
            .find_map(|s| match lowered.document.node(s)? {
                k::Node::Expr(k::Expr::Closure(c)) => Some(c.function),
                _ => None,
            })
    }
    fn target(
        target: &k::CaptionTarget,
        ids: &std::collections::BTreeMap<FunctionId, FunctionId>,
        root: FunctionId,
    ) -> Option<k::CaptionTarget> {
        let site = target.site()?;
        let site = match &site.unit {
            lash_vocabulary::sites::Unit::Main => {
                Site::new(lash_vocabulary::sites::Unit::Library(root), [])
            }
            lash_vocabulary::sites::Unit::Library(id) => Site::new(
                lash_vocabulary::sites::Unit::Library(*ids.get(id)?),
                site.path.clone(),
            ),
            _ => return None,
        };
        Some(k::CaptionTarget::Site(site))
    }
    let baseline_root = root(baseline).ok_or(k::CallableEditRefusal::EntryMissing)?;
    let baseline_functions = equivalent(
        original,
        base.function.callable.function,
        &baseline.document,
        baseline_root,
    )
    .ok_or(k::CallableEditRefusal::EntryMissing)?;
    let selected = *read_functions
        .get(&root(lowered).ok_or(k::CallableEditRefusal::EntryMissing)?)
        .ok_or(k::CallableEditRefusal::EntryMissing)?;
    let spelled = baseline
        .captions
        .entries()
        .iter()
        .filter_map(|entry| {
            target(
                &entry.target,
                &baseline_functions,
                base.function.callable.function,
            )
        })
        .collect::<BTreeSet<_>>();
    let carried = base
        .function
        .captions
        .rekey(correspondence, original, document)
        .ok_or(k::CallableEditRefusal::EntryMissing)?;
    let mut entries = carried
        .captions
        .entries()
        .iter()
        .filter(|entry| {
            let old = entry
                .target
                .site()
                .and_then(|s| correspondence.predecessor(s))
                .map(|s| k::CaptionTarget::Site(s.clone()));
            old.is_none_or(|old| !spelled.contains(&old))
        })
        .cloned()
        .collect::<Vec<_>>();
    for entry in lowered.captions.entries() {
        if let Some(target) = target(&entry.target, read_functions, selected) {
            entries.retain(|entry| entry.target != target);
            entries.push(k::CaptionEntry {
                target,
                caption: entry.caption.clone(),
            });
        }
    }
    k::Captions::new(document, entries)
        .map_err(|e| Diagnostic::from(PrintError::Invalid(e.to_string())).into())
}

/// Check source-worker claims against the retained environment before minting
/// an opaque edit, and again before publication under a fresh policy.
pub fn validate_read(
    base: &CallableSourceBase,
    read: &CallableSourceRead,
) -> Result<(), lash_kernel_doc::CallableEditRefusal> {
    use lash_kernel_doc::CallableEditRefusal as Refusal;
    let original = base
        .document
        .to_document()
        .map_err(|_| Refusal::EntryMissing)?;
    let document = read
        .document
        .to_document()
        .map_err(|_| Refusal::EntryMissing)?;
    if original.imports != document.imports
        || document
            .functions
            .values()
            .filter_map(|f| f.body())
            .flat_map(|body| body.imports.iter())
            .any(|(slot, import)| original.imports.get(slot) != Some(import))
    {
        return Err(Refusal::ImportsChanged);
    }
    if read.presentation.convention != base.function.convention
        || read.presentation.representation != base.function.representation
        || read.presentation.signature.params.len() != base.function.signature.params.len()
    {
        return Err(Refusal::EntryShapeChanged);
    }
    let before = required(&original, base.function.callable.function)?;
    let after = required(&document, read.function)?;
    let added = after.difference(&before).cloned().collect::<BTreeSet<_>>();
    let removed = before.difference(&after).cloned().collect::<BTreeSet<_>>();
    if !added.is_empty()
        || !removed.is_empty()
        || after != read.required_captures
        || after
            .iter()
            .any(|name| !base.function.captures.contains_key(name))
    {
        return Err(Refusal::CaptureSetChanged { added, removed });
    }
    if read.correspondence.base != base.document.id()
        || read.correspondence.result != read.document.id()
    {
        return Err(Refusal::OutsideEditableRegion {
            site: Site::new(lash_vocabulary::sites::Unit::Main, []),
        });
    }
    // Rebuild solely from the old document and the selected replacement.
    // A worker cannot alter cell main, siblings, exports or private bindings.
    let mut expected = original.clone();
    let old_id = base.function.callable.function;
    let replacement = document
        .functions
        .get(&read.function)
        .ok_or(Refusal::EntryMissing)?
        .clone();
    let mut pending = vec![read.function];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(definition) = document.functions.get(&id) {
            if id != read.function && !original.functions.contains_key(&id) {
                expected.functions.insert(id, definition.clone());
            }
            if let Some(body) = definition.body() {
                pending.extend(lash_kernel_doc::Node::Block(&body.block).functions_within());
            }
        }
    }
    expected.functions.insert(old_id, replacement);
    for block in expected.main.iter_mut().chain(
        expected
            .functions
            .values_mut()
            .filter_map(|f| f.body_mut())
            .map(|b| &mut b.block),
    ) {
        update_presentation(
            lash_kernel_doc::NodeMut::Block(block),
            old_id,
            &read.presentation,
        );
    }
    prune_replaced_children(&original, old_id, &mut expected);
    expected.library = document.library.clone();
    expected.requires = document.requires.clone();
    expected
        .reidentify_functions()
        .map_err(|_| Refusal::EntryMissing)?;
    if expected != document {
        return Err(Refusal::OutsideEditableRegion {
            site: Site::new(lash_vocabulary::sites::Unit::Main, []),
        });
    }
    Ok(())
}

pub fn printed_dropped(
    base: &CallableSourceBase,
    lowered: &crate::Lowered,
    ids: &std::collections::BTreeMap<FunctionId, FunctionId>,
) -> Vec<lash_kernel_doc::CaptionTarget> {
    use lash_kernel_doc::CaptionTarget;
    let targets = lowered
        .captions
        .entries()
        .iter()
        .filter_map(|entry| {
            let site = entry.target.site()?;
            let site = match &site.unit {
                lash_vocabulary::sites::Unit::Main => Site::new(
                    lash_vocabulary::sites::Unit::Library(base.function.callable.function),
                    [],
                ),
                lash_vocabulary::sites::Unit::Library(id) => Site::new(
                    lash_vocabulary::sites::Unit::Library(*ids.get(id)?),
                    site.path.clone(),
                ),
                _ => return None,
            };
            Some(CaptionTarget::Site(site))
        })
        .collect::<BTreeSet<_>>();
    base.function
        .captions
        .entries()
        .iter()
        .filter(|entry| !targets.contains(&entry.target))
        .map(|entry| entry.target.clone())
        .collect()
}
