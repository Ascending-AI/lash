//! Immutable package and publication records.

pub mod linked {
    use lash_vocabulary::package::*;
    use serde::{Deserialize, Serialize};
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
}

pub use linked::*;
