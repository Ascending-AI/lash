//! Immutable package and publication records.

pub mod linked {
    use lash_vocabulary::{names::*, package::*};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeSet;
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct LinkRecord {
        pub id: LinkId,
        pub link: Link,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct PackageRecord {
        pub id: PackageId,
        pub package: Package,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(
        tag = "kind",
        content = "value",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    pub enum PublishedContent {
        Package(PackageId),
        Deferred(Box<DeferredPackage>),
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct PublicationRecord {
        pub context: PublicationContext,
        pub path: PackagePath,
        pub content: PublishedContent,
        pub code_links: BTreeSet<LinkId>,
        pub imported_publications: Vec<PublicationKey>,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct PublicationKey {
        pub domain: PublicationDomain,
        pub scope: PublicationScope,
        pub publisher: PublisherInstance,
        pub path: PackagePath,
        pub content_digest: [u8; 32],
    }
}

pub use linked::*;
