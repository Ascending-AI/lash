//! Semantic site qualified by its defining immutable Link.
use lash_kernel_value::package::LinkId;
use lash_kernel_value::sites::Site;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedSite {
    pub link: LinkId,
    pub site: Site,
}
