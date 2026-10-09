use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HeapId(u64);

impl HeapId {
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn from_counter(counter: u64) -> Self {
        Self(counter)
    }
}
