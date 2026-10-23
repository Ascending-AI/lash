//! Typed limits of an edit to a frozen callable environment.
use lash_vocabulary::{Name, sites::Site};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CallableEditRefusal {
    #[error("the required captures changed")]
    CaptureSetChanged {
        added: BTreeSet<Name>,
        removed: BTreeSet<Name>,
    },
    #[error("the entry adapter or token shape changed")]
    EntryShapeChanged,
    #[error("the edited entry is missing")]
    EntryMissing,
    #[error("the retained import declarations changed")]
    ImportsChanged,
    #[error("the edit is outside the callable at {site}")]
    OutsideEditableRegion { site: Site },
}
