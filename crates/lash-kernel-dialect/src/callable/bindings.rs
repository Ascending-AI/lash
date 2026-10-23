//! Stable, collision-free spellings for a retained external binding table.
use super::*;
use lash_kernel_doc as k;
use std::collections::BTreeMap;

pub fn aliases(
    base: &CallableSourceBase,
    legal: impl Fn(&str) -> bool,
) -> Result<BTreeMap<Name, Name>, PrintError> {
    let document = base
        .document
        .to_document()
        .map_err(|e| PrintError::Invalid(e.to_string()))?;
    let plan = crate::NamePlan::new(&document, legal);
    let mut taken = plan.names.values().cloned().collect::<BTreeSet<_>>();
    let mut out = BTreeMap::new();
    for name in base.function.captures.keys() {
        let alias = if let Some(alias) = plan.names.get(name) {
            alias.clone()
        } else {
            let mut next = 1;
            loop {
                let candidate = format!("capture_{next}");
                next += 1;
                if taken.insert(candidate.clone()) {
                    break candidate;
                }
            }
        };
        out.insert(name.clone(), Name::new(alias));
    }
    Ok(out)
}
/// Restore lexical identities immediately after lowering, before comparing
/// free-name sets or publishing code. Source never supplies their values.
pub fn restore(
    mut lowered: crate::Lowered,
    aliases: &BTreeMap<Name, Name>,
) -> Result<crate::Lowered, CallableReadError> {
    let names = aliases
        .iter()
        .filter(|(a, b)| a != b)
        .map(|(a, b)| (b.clone(), a.clone()))
        .collect::<BTreeMap<_, _>>();
    if names.is_empty() {
        return Ok(lowered);
    }
    let original = lowered.document.document();
    let mut document = original.clone();
    if let Some(main) = &mut document.main {
        super::equivalence::rename(k::NodeMut::Block(main), &names);
    }
    for definition in document.functions.values_mut() {
        for param in &mut definition.signature.params {
            if let Some(name) = names.get(&param.name) {
                param.name.clone_from(name);
            }
        }
        if let Some(body) = definition.body_mut() {
            super::equivalence::rename(k::NodeMut::Block(&mut body.block), &names);
        }
    }
    let ids = document
        .reidentify_functions()
        .map_err(|e| Diagnostic::from(PrintError::Invalid(e)))?;
    let site = |site: &Site| {
        let mut site = site.clone();
        if let lash_vocabulary::sites::Unit::Library(id) = &mut site.unit {
            *id = ids.get(id).copied().unwrap_or(*id);
        }
        site
    };
    let entries = super::all_sites(original)
        .into_iter()
        .map(|from| k::Survivor::new(from.clone(), site(&from), false))
        .collect();
    let correspondence = Correspondence::of(original.identity(), document.identity(), entries)
        .ok_or(k::CallableEditRefusal::EntryMissing)?;
    lowered.captions = lowered
        .captions
        .rekey(&correspondence, original, &document)
        .ok_or(k::CallableEditRefusal::EntryMissing)?
        .captions;
    lowered.source_map = lowered
        .source_map
        .and_then(|map| map.rekey(&correspondence));
    lowered.written = lowered
        .written
        .into_iter()
        .map(|(at, mut written)| {
            for caption in &mut written.captions {
                if let k::CaptionTarget::Site(at) = &mut caption.target {
                    *at = site(at);
                }
            }
            (site(&at), written)
        })
        .collect();
    lowered.document = k::IdentifiedDocument::new(document);
    Ok(lowered)
}

/// The fragment declaration cannot shadow one of its frozen lexical bindings.
pub fn entry_name(
    base: &CallableSourceBase,
    legal: impl Fn(&str) -> bool,
) -> Result<Name, PrintError> {
    let document = base
        .document
        .to_document()
        .map_err(|e| PrintError::Invalid(e.to_string()))?;
    let plan = crate::NamePlan::new(&document, &legal);
    if let Some(name) = base
        .function
        .signature_origin
        .as_ref()
        .and_then(|origin| origin.name.as_ref())
        && legal(name)
        && !aliases(base, &legal)?
            .values()
            .any(|alias| alias.as_str() == name)
    {
        return Ok(Name::new(name));
    }

    let mut taken = plan.names.into_values().collect::<BTreeSet<_>>();
    taken.extend(
        aliases(base, &legal)?
            .into_values()
            .map(|name| name.to_string()),
    );
    if !taken.contains("workflow") {
        return Ok(Name::new("workflow"));
    }
    let mut next = 1;
    loop {
        let candidate = format!("workflow_{next}");
        next += 1;
        if !taken.contains(&candidate) {
            return Ok(Name::new(candidate));
        }
    }
}
