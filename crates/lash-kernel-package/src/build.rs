//! Fresh fulfilment chooses compatible signatures; existing Links retain their pins.
use crate::{LinkRefusal, LinkedContent};
use lash_kernel_doc::{Document, Export, FunctionExport, FunctionTarget, Node};

use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::{names::*, package::*, types::Type};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReachSelection {
    AllExports,
    Callable(FunctionId),
    Start(StartSelector),
    Function(ExportName),
    Type(ExportName),
}
/// Trusted accepted advertisements. Contexts come from publication, never a guest value.
pub struct Namespace<'a> {
    pub advertisements: Vec<(&'a PublicationContext, &'a PackageAdvertisement)>,
}
fn invalid(message: impl Into<String>) -> LinkRefusal {
    LinkRefusal::InvalidDocument {
        message: message.into(),
    }
}
fn path(advertisement: &PackageAdvertisement) -> &PackagePath {
    match advertisement {
        PackageAdvertisement::Enumerated(p) => &p.path,
        PackageAdvertisement::Deferred(p) => &p.path,
    }
}
fn visible(
    visibility: Visibility,
    owner: &PublicationContext,
    request: &PublicationContext,
    package: &PackagePath,
    own_package: Option<&PackagePath>,
) -> bool {
    visibility == Visibility::Public || (owner == request && own_package == Some(package))
}
fn scope_visible(owner: &PublicationContext, consumer: &PublicationContext) -> bool {
    owner.domain == consumer.domain
        && (owner.scope == PublicationScope::Host || owner.scope == consumer.scope)
}
impl Namespace<'_> {
    /// Validate roots, ownership, closed leaves and deferred prefix exclusion as one view.
    pub fn validate(&self) -> Result<(), LinkRefusal> {
        for (index, (context, advertisement)) in self.advertisements.iter().enumerate() {
            let here = path(advertisement);
            if here.first() != Some(&context.root) {
                return Err(LinkRefusal::NotOwner { path: here.clone() });
            }
            for (other_context, other) in &self.advertisements[..index] {
                let there = path(other);
                if here == there {
                    return Err(LinkRefusal::PathConflict { path: here.clone() });
                }
                if context.domain != other_context.domain {
                    return Err(LinkRefusal::NotOwner { path: here.clone() });
                }
                if here.first() == there.first()
                    && (context.publisher != other_context.publisher
                        || context.scope != other_context.scope)
                {
                    return Err(LinkRefusal::NotOwner { path: here.clone() });
                }
                let prefix =
                    here.starts_with(there.as_slice()) || there.starts_with(here.as_slice());
                if prefix
                    && (matches!(advertisement, PackageAdvertisement::Deferred(_))
                        || matches!(other, PackageAdvertisement::Deferred(_)))
                {
                    return Err(LinkRefusal::PathConflict { path: here.clone() });
                }
                for (short, long, ad) in [(here, there, *advertisement), (there, here, *other)] {
                    if long.starts_with(short.as_slice())
                        && long.len() > short.len()
                        && let PackageAdvertisement::Enumerated(package) = ad
                        && package.members.contains_key(&long[short.len()])
                    {
                        return Err(LinkRefusal::PathConflict { path: long.clone() });
                    }
                }
            }
        }
        Ok(())
    }
    pub fn fulfil(
        &self,
        content: &LinkedContent,
        document: DocumentId,
        context: &PublicationContext,
        own_package: Option<&PackagePath>,
        attachment: Option<&Attachment>,
    ) -> Result<Link, LinkRefusal> {
        self.validate()?;
        let source = content.document(document)?;
        let mut fills = BTreeMap::new();
        for (slot, import) in &source.imports {
            let mut chosen = None;
            for (owner, ad) in &self.advertisements {
                if !scope_visible(owner, context) {
                    continue;
                }
                match ad {
                    PackageAdvertisement::Enumerated(package)
                        if package.path == import.name.package =>
                    {
                        let member = package.members.get(&import.name.member).ok_or_else(|| {
                            LinkRefusal::MissingImport {
                                slot: *slot,
                                name: import.name.clone(),
                            }
                        })?;
                        if !visible(
                            member.visibility,
                            owner,
                            context,
                            &package.path,
                            own_package,
                        ) {
                            return Err(LinkRefusal::NotVisible {
                                name: import.name.clone(),
                            });
                        }
                        if let Some(attachment) = attachment
                            && !attachment.packages.contains(&package.path)
                            && !attachment.members.contains(&import.name)
                        {
                            return Err(LinkRefusal::NotVisible {
                                name: import.name.clone(),
                            });
                        }
                        let fill = match &member.kind {
                            MemberKind::Operation(offer) if import.name.type_member.is_none() => {
                                Fill::Operation(offer.requirement.clone())
                            }
                            MemberKind::Operation(_) => {
                                return Err(LinkRefusal::KindMismatch {
                                    slot: *slot,
                                    name: import.name.clone(),
                                });
                            }
                            MemberKind::Export { link, export } => {
                                let mut export = export.clone();
                                if let Some(member) = &import.name.type_member {
                                    if export.member.is_some() {
                                        return Err(LinkRefusal::KindMismatch {
                                            slot: *slot,
                                            name: import.name.clone(),
                                        });
                                    }
                                    export.member = Some(member.clone());
                                }
                                Fill::Export {
                                    link: *link,
                                    export,
                                }
                            }
                        };
                        compatible(content, *slot, import, &fill)?;
                        chosen = Some(fill);
                        break;
                    }
                    PackageAdvertisement::Deferred(package)
                        if import.name.package.starts_with(package.path.as_slice()) =>
                    {
                        if !visible(
                            package.visibility,
                            owner,
                            context,
                            &package.path,
                            own_package,
                        ) {
                            return Err(LinkRefusal::NotVisible {
                                name: import.name.clone(),
                            });
                        }
                        if let Some(attachment) = attachment
                            && !attachment.deferred.contains(&package.path)
                        {
                            return Err(LinkRefusal::UndeclaredDeferred {
                                path: package.path.clone(),
                            });
                        }
                        let ImportKind::Operation { signature } = &import.kind else {
                            return Err(LinkRefusal::KindMismatch {
                                slot: *slot,
                                name: import.name.clone(),
                            });
                        };
                        if !signature.params.iter().all(|param| param.ty == Type::Json)
                            || signature.result != Type::Json
                        {
                            return Err(LinkRefusal::SignatureMismatch {
                                slot: *slot,
                                name: import.name.clone(),
                            });
                        }
                        chosen = Some(Fill::Deferred {
                            provider: Box::new(DeferredGrant {
                                provider: package.provider.clone(),
                                context: (*owner).clone(),
                                path: package.path.clone(),
                                visibility: package.visibility,
                                signature: signature.clone(),
                                config: package.config.clone(),
                                resolution_policy: package.resolution_policy,
                            }),
                        });
                        break;
                    }
                    _ => {}
                }
            }
            fills.insert(
                *slot,
                chosen.ok_or_else(|| LinkRefusal::MissingImport {
                    slot: *slot,
                    name: import.name.clone(),
                })?,
            );
        }
        if let Some(attachment) = attachment {
            let mut pending: Vec<_> = fills
                .values()
                .filter_map(|fill| match fill {
                    Fill::Export { link, .. } => Some(*link),
                    _ => None,
                })
                .collect();
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !seen.insert(id) {
                    continue;
                }
                for fill in content.link(id)?.fills.values() {
                    match fill {
                        Fill::Deferred { provider }
                            if !attachment.deferred.contains(&provider.path) =>
                        {
                            return Err(LinkRefusal::UndeclaredDeferred {
                                path: provider.path.clone(),
                            });
                        }
                        Fill::Export { link, .. } => pending.push(*link),
                        _ => {}
                    }
                }
            }
        }
        Ok(Link { document, fills })
    }
    /// Validate a source execution's authored Link against its accepted
    /// publications. Only public members may supply authored cell Imports.
    /// Previously admitted package internals keep their own publication proof.
    pub fn validate_authored_origin(
        &self,
        content: &LinkedContent,
        link: &Link,
    ) -> Result<(), LinkRefusal> {
        self.validate()?;
        content.check_link(link)?;
        for (slot, import) in &content.document(link.document)?.imports {
            let pinned = &link.fills[slot];
            let found = self.advertisements.iter().any(|(context, ad)| match ad {
                PackageAdvertisement::Enumerated(package)
                    if package.path == import.name.package =>
                {
                    package
                        .members
                        .get(&import.name.member)
                        .is_some_and(|member| {
                            if member.visibility != Visibility::Public {
                                return false;
                            }
                            match (&member.kind, pinned) {
                                (MemberKind::Operation(offer), Fill::Operation(requirement)) => {
                                    import.name.type_member.is_none()
                                        && offer.requirement == *requirement
                                }
                                (
                                    MemberKind::Export { link, export },
                                    Fill::Export {
                                        link: held,
                                        export: target,
                                    },
                                ) => {
                                    let mut expected = export.clone();
                                    if let Some(member) = &import.name.type_member {
                                        if expected.member.is_some() {
                                            return false;
                                        }
                                        expected.member = Some(member.clone());
                                    }
                                    link == held && expected == *target
                                }
                                _ => false,
                            }
                        })
                }
                PackageAdvertisement::Deferred(package) => match pinned {
                    Fill::Deferred { provider } => {
                        package.visibility == Visibility::Public
                            && import.name.package.starts_with(package.path.as_slice())
                            && provider.context == **context
                            && provider.path == package.path
                            && provider.visibility == package.visibility
                            && provider.provider == package.provider
                            && provider.config == package.config
                            && provider.resolution_policy == package.resolution_policy
                    }
                    _ => false,
                },
                _ => false,
            });
            if !found {
                return Err(LinkRefusal::NotVisible {
                    name: import.name.clone(),
                });
            }
        }
        Ok(())
    }
    /// A live offer may serve a fresh Import while failing an existing C1 pin.
    pub fn validate_link(
        &self,
        content: &LinkedContent,
        link: &Link,
        context: &PublicationContext,
        own_package: Option<&PackagePath>,
        attachment: Option<&Attachment>,
    ) -> Result<(), LinkRefusal> {
        content.check_link(link)?;
        let offered = self.fulfil(content, link.document, context, own_package, attachment)?;
        for (slot, pinned) in &link.fills {
            match (pinned, &offered.fills[slot]) {
                (Fill::Operation(before), Fill::Operation(now))
                    if before.contract != now.contract =>
                {
                    return Err(LinkRefusal::ContractMismatch {
                        use_: OperationUse {
                            link: link.identity(),
                            slot: *slot,
                        },
                        pinned: before.contract.clone(),
                        offered: now.contract.clone(),
                    });
                }
                (Fill::Operation(_), Fill::Operation(_)) => {}
                (before, now) if before == now => {}
                _ => {
                    return Err(invalid(format!(
                        "pinned fill {slot:?} differs from the accepted member"
                    )));
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn compatible(
    content: &LinkedContent,
    slot: ImportSlot,
    import: &Import,
    fill: &Fill,
) -> Result<(), LinkRefusal> {
    let wrong = || LinkRefusal::KindMismatch {
        slot,
        name: import.name.clone(),
    };
    let signature = || LinkRefusal::SignatureMismatch {
        slot,
        name: import.name.clone(),
    };
    match (&import.kind, fill) {
        (
            ImportKind::Operation {
                signature: expected,
            },
            Fill::Operation(requirement),
        ) => {
            if lash_vocabulary::type_relations::serves(&requirement.signature, expected).is_none() {
                Ok(())
            } else {
                Err(signature())
            }
        }
        (
            ImportKind::Operation {
                signature: expected,
            },
            Fill::Deferred { provider },
        ) => {
            if &provider.signature != expected
                || !import.name.package.starts_with(provider.path.as_slice())
                || !expected.params.iter().all(|p| p.ty == Type::Json)
                || expected.result != Type::Json
            {
                Err(signature())
            } else {
                Ok(())
            }
        }
        (kind, Fill::Export { link, export }) => match (kind, content.export(*link, export)?) {
            (
                ImportKind::Code {
                    signature: expected,
                    convention,
                },
                Export::Function(function),
            ) => {
                if function.convention.semantic() != *convention {
                    return Err(LinkRefusal::ConventionMismatch {
                        slot,
                        name: import.name.clone(),
                    });
                }
                if lash_vocabulary::type_relations::serves(&function.signature, expected).is_some()
                {
                    return Err(signature());
                }
                Ok(())
            }
            (ImportKind::Type { brand }, Export::Type(ty)) if *brand == ty.brand => Ok(()),
            (ImportKind::Error { kind: expected }, Export::Error { kind }) if *expected == kind => {
                Ok(())
            }
            _ => Err(wrong()),
        },
        _ => Err(wrong()),
    }
}
impl LinkedContent {
    /// Check canonical Brands and prevent an internal member from becoming a public alias.
    pub fn validate_publication(
        &self,
        context: &PublicationContext,
        package: &Package,
        snapshot: &Namespace<'_>,
    ) -> Result<(), LinkRefusal> {
        snapshot.validate()?;
        if package.path.first() != Some(&context.root) {
            return Err(LinkRefusal::NotOwner {
                path: package.path.clone(),
            });
        }
        for (owner, ad) in &snapshot.advertisements {
            if path(ad).first() == package.path.first() && (**owner != *context) {
                return Err(LinkRefusal::NotOwner {
                    path: package.path.clone(),
                });
            }
        }
        let candidate = PackageAdvertisement::Enumerated(package.clone());
        let mut ads: Vec<_> = snapshot
            .advertisements
            .iter()
            .copied()
            .filter(|(_, ad)| path(ad) != &package.path)
            .collect();
        ads.push((context, &candidate));
        let publication = Namespace {
            advertisements: ads,
        };
        publication.validate()?;
        for (name, member) in &package.members {
            let MemberKind::Export { link, export } = &member.kind else {
                continue;
            };
            self.retain_reach(*link)?;
            // Only a previously accepted public Export carries publication authority.
            // An arbitrary structurally valid Link does not authorize its pinned internal fills.
            let accepted = snapshot.advertisements.iter().any(|(owner, ad)| {
                scope_visible(owner, context)
                    && match ad {
                        PackageAdvertisement::Enumerated(original) => original
                            .members
                            .values()
                            .any(|m| m.visibility == Visibility::Public && m.kind == member.kind),
                        PackageAdvertisement::Deferred(_) => false,
                    }
            });
            if !accepted {
                publication.validate_link(
                    self,
                    self.link(*link)?,
                    context,
                    Some(&package.path),
                    None,
                )?;
            }
            let target = self.export(*link, export)?;
            let brand = match &target {
                Export::Type(ty) => Some(&ty.brand),
                Export::Error { kind } => Some(kind),
                Export::Function(_) => None,
            };
            if let Some(brand) = brand
                && (brand.package != package.path || brand.name != *name || export.member.is_some())
            {
                return Err(LinkRefusal::NotOwner {
                    path: package.path.clone(),
                });
            }
            for (owner, ad) in &snapshot.advertisements {
                let PackageAdvertisement::Enumerated(p) = ad else {
                    continue;
                };
                for original in p.members.values() {
                    if original.kind == member.kind
                        && original.visibility == Visibility::Internal
                        && (p.path != package.path
                            || **owner != *context
                            || member.visibility != Visibility::Internal)
                    {
                        return Err(LinkRefusal::NotVisible {
                            name: QualifiedName {
                                package: package.path.clone(),
                                member: name.clone(),
                                type_member: None,
                            },
                        });
                    }
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn follow_function(
    document: &Document,
    root: FunctionId,
    required: &mut lash_kernel_check::Requirements,
) {
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if required.functions.contains_key(&id) {
            continue;
        }
        let Some(function) = document.functions.get(&id) else {
            continue;
        };
        required.functions.insert(id, function.name.clone());
        if let Some(body) = function.body() {
            let node = Node::Block(&body.block);
            required.imports.extend(node.imports_within());
            pending.extend(node.functions_within());
        }
    }
}
pub(crate) fn function_reach(
    document: &Document,
    function: &FunctionExport,
) -> lash_kernel_check::Requirements {
    let mut required = lash_kernel_check::Requirements::default();
    follow_function(document, function.function, &mut required);
    for target in function.convention.targets() {
        match target {
            FunctionTarget::Local(id) => follow_function(document, *id, &mut required),
            FunctionTarget::Import(slot) => {
                required.imports.insert(*slot);
            }
        }
    }
    required
}
