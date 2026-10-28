//! Explicit helper rewrites preserve every older document and its identities.
use crate::Rewritten;
use lash_kernel_doc::{Document, HelperUpgrade, HelperUpgradeRefusal};

pub fn upgrade_helpers(
    document: &Document,
    upgrade: &HelperUpgrade,
) -> Result<Rewritten, HelperUpgradeRefusal> {
    let invalid = |message: String| HelperUpgradeRefusal::Document { message };
    if !document
        .library
        .iter()
        .chain(&document.requires.natives)
        .any(|id| upgrade.functions.contains_key(id))
    {
        return Ok(Rewritten {
            document: document.clone(),
            correspondence: crate::unchanged(document, document)
                .map_err(|e| invalid(e.to_string()))?,
            functions: document.functions.keys().map(|id| (*id, *id)).collect(),
        });
    }
    let mut next = document.clone();
    crate::replace_functions(&mut next, &upgrade.functions);
    for definition in next.functions.values_mut() {
        if let Some(body) = definition.body_mut() {
            body.functions = std::mem::take(&mut body.functions)
                .into_iter()
                .map(|(id, name)| (upgrade.functions.get(&id).copied().unwrap_or(id), name))
                .collect();
        }
    }
    next.library.0 = next
        .library
        .iter()
        .map(|id| upgrade.functions.get(id).copied().unwrap_or(*id))
        .collect();
    next.requires.natives = next
        .requires
        .natives
        .iter()
        .map(|id| upgrade.functions.get(id).copied().unwrap_or(*id))
        .collect();
    // Availability debts are derived afresh when the successor's exact reach is admitted.
    next.requires.libraries.clear();
    next.requires.helpers = Default::default();
    let functions = next.reidentify_functions().map_err(invalid)?;
    let mut entries = crate::rewrite::unmoved(document);
    for entry in &mut entries {
        if let lash_kernel_value::sites::Unit::Library(id) = &mut entry.to.unit {
            *id = functions.get(id).copied().unwrap_or(*id);
        }
    }
    let correspondence =
        lash_kernel_doc::Correspondence::of(document.identity(), next.identity(), entries)
            .ok_or_else(|| invalid("non-injective helper rewrite".into()))?;
    Ok(Rewritten {
        document: next,
        correspondence,
        functions,
    })
}

/// Rewrite the selected callable only. Frozen nested callables retain their exact origins.
pub fn upgrade_callable(
    value: &lash_kernel_doc::FunctionValue,
    before: &Document,
    rewritten: &Rewritten,
    link: lash_kernel_value::LinkId,
    upgrade: &HelperUpgrade,
) -> Result<lash_kernel_doc::FunctionValue, HelperUpgradeRefusal> {
    check_upgrade_captures(value, upgrade)?;
    let replace = |id: lash_kernel_value::FunctionId| {
        rewritten
            .functions
            .get(&id)
            .or_else(|| upgrade.functions.get(&id))
            .copied()
            .unwrap_or(id)
    };
    let function = replace(value.callable.function);
    if function == value.callable.function && upgrade.changed.contains(&function) {
        return Err(HelperUpgradeRefusal::RetainedCallable { function });
    }
    let mut next = value.clone();
    next.callable = lash_kernel_value::CallableId { link, function };
    for target in next.convention.targets_mut() {
        if let lash_kernel_doc::FunctionTarget::Local(function) = target {
            *function = replace(*function);
        }
    }
    if let Some(site) = &mut next.creation
        && let lash_kernel_value::sites::Unit::Library(function) = &mut site.unit
    {
        *function = replace(*function);
    }
    next.captions = value
        .captions
        .rekey(&rewritten.correspondence, before, &rewritten.document)
        .map(|mapped| mapped.captions)
        .ok_or_else(|| HelperUpgradeRefusal::Document {
            message: "callable captions differ from the defining document".into(),
        })?;
    Ok(next)
}

/// Frozen records use the same exact constructor stamps as the paged heap.
pub fn check_upgrade_captures(
    value: &lash_kernel_doc::FunctionValue,
    upgrade: &HelperUpgrade,
) -> Result<(), HelperUpgradeRefusal> {
    use lash_kernel_doc::{CaptureValue as C, Datum as D};
    fn captured_brand(value: &C) -> bool {
        match value {
            C::Data(value) => brand(value),
            C::Tuple(parts) => {
                parts.len() == 2 && parts.iter().all(|part| matches!(part, C::Data(D::Text(_))))
            }
            _ => false,
        }
    }
    let mut captures: Vec<_> = value.captures.values().collect();
    let mut data: Vec<_> = value.token.iter().flatten().collect();
    while let Some(value) = captures.pop() {
        match value {
            C::Data(value) => data.push(value),
            C::Function(function) => {
                if upgrade.changed.contains(&function.callable.function) {
                    return Err(HelperUpgradeRefusal::RetainedCallable {
                        function: function.callable.function,
                    });
                }
                captures.extend(function.captures.values());
                data.extend(function.token.iter().flatten());
            }
            C::Tuple(values) | C::List(values) | C::Set(values) => captures.extend(values),
            C::Map(entries) => {
                captures.extend(entries.iter().flat_map(|(key, value)| [key, value]))
            }
            C::Record(fields) => {
                if fields
                    .iter()
                    .any(|(key, value)| key == "__lash_brand" && captured_brand(value))
                    && let Some(C::Data(built)) = fields
                        .iter()
                        .find_map(|(key, value)| (key == "__lash_built").then_some(value))
                {
                    constructor(built, upgrade)?;
                }
                captures.extend(fields.iter().map(|(_, value)| value));
            }
            C::Error { data, .. } => captures.push(data),
            C::Reference(_) => {}
        }
    }
    for value in data {
        check_upgrade_data(value, upgrade)?;
    }
    Ok(())
}

/// Start arguments are retained data even before the first checkpoint exists.
pub fn check_upgrade_data(
    value: &lash_kernel_doc::Datum,
    upgrade: &HelperUpgrade,
) -> Result<(), HelperUpgradeRefusal> {
    use lash_kernel_doc::Datum as D;
    let mut data = vec![value];
    while let Some(value) = data.pop() {
        match value {
            D::Tuple(values) | D::List(values) | D::Set(values) => data.extend(values),
            D::Map(entries) => data.extend(entries.iter().flat_map(|(key, value)| [key, value])),
            D::Record(fields) => {
                if fields
                    .iter()
                    .any(|(key, value)| key == "__lash_brand" && brand(value))
                    && let Some(built) = fields
                        .iter()
                        .find_map(|(key, value)| (key == "__lash_built").then_some(value))
                {
                    constructor(built, upgrade)?;
                }
                data.extend(fields.iter().map(|(_, value)| value));
            }
            D::Error(error) => data.push(&error.data),
            _ => {}
        }
    }
    Ok(())
}

fn brand(value: &lash_kernel_doc::Datum) -> bool {
    matches!(value, lash_kernel_doc::Datum::Tuple(parts) if parts.len() == 2 && parts.iter().all(|part| matches!(part, lash_kernel_doc::Datum::Text(_))))
}

fn constructor(
    value: &lash_kernel_doc::Datum,
    upgrade: &HelperUpgrade,
) -> Result<(), HelperUpgradeRefusal> {
    if let lash_kernel_doc::Datum::Bytes(bytes) = value
        && let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        let function = lash_kernel_value::FunctionId::from_bytes(bytes);
        if upgrade.changed.contains(&function) {
            return Err(HelperUpgradeRefusal::BrandedData { function });
        }
    }
    Ok(())
}
