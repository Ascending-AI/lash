/// A closed range of journal keys, compared bytewise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordedKeyRange {
    pub lower: String,
    pub upper: String,
    pub group_key_prefix: String,
}

/// The effect and group keys recorded in one scope and range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordedKeys {
    pub replay_keys: Vec<String>,
    pub group_keys: Vec<String>,
    pub closing_outcome: Option<String>,
}
