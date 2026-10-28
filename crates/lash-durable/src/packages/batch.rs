//! Validate a namespace once, then acquire its union under one owner fence.
use super::*;
use lash_vocabulary::package::PublishedContent;
use std::collections::{BTreeMap, BTreeSet};

pub(super) async fn acquire<S: PackageStorage + ?Sized>(
    store: &S,
    claim: &ReferrerClaim,
    holds: &[PackageHold<'_>],
) -> Result<(), DurableError> {
    let mut records = Vec::with_capacity(holds.len());
    let mut prepared = BTreeMap::new();
    for hold in holds {
        let (reference, bytes, document) = match hold {
            PackageHold::Document { id, body } => {
                if *id
                    != lash_kernel_value::names::DocumentId::from_bytes(
                        lash_kernel_value::tokens::logical_digest(body),
                    )
                {
                    return Err(PackageRefusal::Identity(id.to_string()).into());
                }
                (id.to_string(), Some(body.to_vec()), true)
            }
            PackageHold::Link { record, artifacts } => {
                let stored = StoredLink {
                    record: (*record).clone(),
                    artifacts: artifacts.iter().cloned().collect(),
                };
                (link_reference(&record.id), Some(encode(&stored)?), false)
            }
            PackageHold::Package(record) => {
                (package_reference(&record.id), Some(encode(record)?), false)
            }
            PackageHold::Publish(record) => (
                publication_reference(&record.key()),
                Some(encode(record)?),
                false,
            ),
            PackageHold::RetainLink(id) => (link_reference(id), None, false),
            PackageHold::Publication(record) => {
                let reference = publication_reference(&record.key());
                let bytes = store
                    .load_package_record(&reference)
                    .await?
                    .ok_or_else(|| PackageRefusal::Identity(reference.clone()))?;
                let stored: PublicationRecord = decode(&bytes)?;
                if stored != **record {
                    return Err(PackageRefusal::Identity(reference).into());
                }
                if let Some(previous) = prepared.insert(reference.clone(), bytes.clone())
                    && previous != bytes
                {
                    return Err(PackageRefusal::Immutable(reference).into());
                }
                (reference, None, false)
            }
        };
        if let Some(bytes) = &bytes
            && let Some(previous) = prepared.insert(reference.clone(), bytes.clone())
            && previous != *bytes
        {
            return Err(PackageRefusal::Immutable(reference).into());
        }
        records.push(PackageRecordHold {
            reference,
            bytes,
            document,
            dependencies: Vec::new(),
        });
    }
    for hold in holds {
        if let PackageHold::Publish(record) = hold {
            validate_publication(store, record, &mut prepared).await?;
        }
    }
    let dependencies = closure_with(
        store,
        records.iter().map(|r| r.reference.clone()).collect(),
        &prepared,
    )
    .await?;
    let supplied = records
        .iter()
        .map(|r| r.reference.clone())
        .collect::<BTreeSet<_>>();
    if let Some(first) = records.first_mut() {
        first.dependencies = dependencies.difference(&supplied).cloned().collect();
    }
    store.hold_package_batch(claim, &records).await
}

async fn validate_publication<S: PackageStorage + ?Sized>(
    store: &S,
    record: &PublicationRecord,
    prepared: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), DurableError> {
    if record.path.as_slice().first() != Some(&record.context.root) {
        return Err(PackageRefusal::Publication.into());
    }
    match &record.content {
        PublishedContent::Package(id) => {
            let reference = package_reference(id);
            if let std::collections::btree_map::Entry::Vacant(entry) =
                prepared.entry(reference.clone())
            {
                let bytes = store
                    .load_package_record(&reference)
                    .await?
                    .ok_or_else(|| PackageRefusal::Missing(reference.clone()))?;
                entry.insert(bytes);
            }
            let package: PackageRecord = decode(&prepared[&reference])?;
            if package.id != *id || package.package.identity() != *id {
                return Err(PackageRefusal::Identity(reference).into());
            }
            let links: BTreeSet<_> = package
                .package
                .members
                .values()
                .filter_map(|member| match &member.kind {
                    lash_kernel_value::package::MemberKind::Export { link, .. } => Some(*link),
                    _ => None,
                })
                .collect();
            if package.package.path != record.path || links != record.code_links {
                return Err(PackageRefusal::Publication.into());
            }
        }
        PublishedContent::Deferred(package) => {
            if package.path != record.path || !record.code_links.is_empty() {
                return Err(PackageRefusal::Publication.into());
            }
        }
    }
    Ok(())
}
