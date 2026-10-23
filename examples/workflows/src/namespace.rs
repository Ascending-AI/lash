//! Trusted in-process publication for the source corpus. The same pure
//! fulfiller and namespace projection used by hosts select every Import.
use super::*;
use lash_kernel_package::{LinkedContent, Namespace};
use lash_vocabulary::{names::*, package::*};

pub(crate) struct FixtureNamespace {
    content: LinkedContent,
    helpers: Arc<k::BuildHelpers>,
    advertisements: Vec<(PublicationContext, PackageAdvertisement)>,
}
impl FixtureNamespace {
    pub(crate) fn new(
        effects: BTreeMap<lash_vocabulary::EffectName, lash_vocabulary::OperationRequirement>,
        controls: &BTreeMap<lash_vocabulary::EffectName, lash_vocabulary::TurnControls>,
    ) -> Result<Self> {
        let release = lash::vm::helper_release()?;
        let mut content = LinkedContent::default();
        for package in lash::vm::DIALECT_SERVICE_PACKAGES.iter() {
            content.packages.insert(package.identity(), package.clone());
        }
        let mut packages: BTreeMap<PackagePath, BTreeMap<Segment, Member>> = BTreeMap::new();
        for (name, requirement) in effects {
            let operation = QualifiedOperation::from_effect(&name)?;
            packages.entry(operation.package).or_default().insert(
                operation.name,
                Member {
                    visibility: Visibility::Public,
                    kind: MemberKind::Operation(OperationOffer {
                        requirement,
                        controls: controls.get(&name).cloned().unwrap_or_default(),
                        intents: Default::default(),
                        mode: OperationMode::Waited,
                    }),
                },
            );
        }
        for (path, members) in packages {
            content.insert_package(Package { path, members });
        }
        let advertisements = content
            .packages
            .values()
            .map(|package| {
                (
                    PublicationContext {
                        root: package.path.as_slice()[0].clone(),
                        ..Self::context()
                    },
                    PackageAdvertisement::Enumerated(package.clone()),
                )
            })
            .collect();
        Ok(Self {
            content,
            helpers: Arc::clone(&release.helpers),
            advertisements,
        })
    }
    fn context() -> PublicationContext {
        PublicationContext {
            domain: PublicationDomain("workflow-corpus".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("workflow-corpus".into()),
            root: Segment::new("corpus")
                .unwrap_or_else(|error| panic!("declared corpus root: {error}")),
        }
    }
    pub(crate) fn namespaces(&self) -> Result<lash_kernel_dialect::Namespaces> {
        Ok(lash_kernel_dialect::Namespaces::project(
            self.content.packages.values(),
            Default::default(),
            &|link, name| self.content.export(link, name).ok(),
            &|_| false,
        )?)
    }
    pub(crate) fn link(
        &self,
        document: Document,
    ) -> Result<(LinkedContent, lash_vocabulary::LinkId)> {
        let mut content = self.content.clone();
        let id = content.insert_document(document, self.helpers.as_ref())?;
        let attachment = Attachment {
            packages: self
                .content
                .packages
                .values()
                .map(|p| p.path.clone())
                .collect(),
            members: Default::default(),
            deferred: Default::default(),
        };
        let link = Namespace {
            advertisements: self
                .advertisements
                .iter()
                .map(|(context, advertisement)| (context, advertisement))
                .collect(),
        }
        .fulfil(&content, id, &Self::context(), None, Some(&attachment))?;
        let link = content.insert_link(link)?;
        Ok((content, link))
    }
}
