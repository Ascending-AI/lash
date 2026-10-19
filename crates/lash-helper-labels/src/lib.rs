//! Package-owned display facts, without execution semantics.
//!
//! The package crate owns each display entry and its key. This crate supplies
//! the plain data and the two-column label reader; it owns no library inventory.

use std::collections::BTreeMap;

/// Display facts for one package member or one private library definition.
///
/// Each comparison holds for exactly the arguments the function answers
/// `true` for, under its own equality, order or truth, in parameter order.
/// A reply receiver takes one effect reply and returns that reply in its
/// dialect's representation; it neither selects a part nor converts its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDisplay {
    pub title: String,
    pub comparison: Option<Comparison>,
    pub reply: bool,
}

impl MemberDisplay {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            comparison: None,
            reply: false,
        }
    }
}

/// What a comparison function decides. A binary comparison takes its left
/// operand first: `Contains` holds when the first argument holds the
/// second, `StartsWith` when the first begins with the second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// The negation of its one operand's truth.
    Not,
    Contains,
    StartsWith,
    EndsWith,
}

impl Comparison {
    /// How many arguments the comparison reads.
    pub fn arity(self) -> usize {
        match self {
            Self::Not => 1,
            Self::Eq
            | Self::Ne
            | Self::Lt
            | Self::Le
            | Self::Gt
            | Self::Ge
            | Self::Contains
            | Self::StartsWith
            | Self::EndsWith => 2,
        }
    }
}

/// A malformed shipped side file: a build defect, never guest input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for LabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "helper label line {}: {}", self.line, self.message)
    }
}
impl std::error::Error for LabelError {}

/// Read a package's `name<TAB>title` side file through its key parser.
///
/// The package supplies the interpretation of a name: a canonical member path
/// for a host package, or a definition name for a built-in package. Duplicate
/// parsed keys and empty titles are refused rather than silently overwritten.
pub fn read_labels<K: Ord>(
    data: &str,
    mut parse_key: impl FnMut(&str) -> Option<K>,
) -> Result<BTreeMap<K, MemberDisplay>, LabelError> {
    let mut labels = BTreeMap::new();
    for (index, row) in data.lines().enumerate() {
        let fail = |message: &str| LabelError {
            line: index + 1,
            message: message.into(),
        };
        let (name, title) = row
            .split_once('\t')
            .ok_or_else(|| fail("expected name and title"))?;
        let name = parse_key(name).ok_or_else(|| fail("invalid display key"))?;
        if title.trim().is_empty() || title.contains('\t') {
            return Err(fail("expected one non-empty title"));
        }
        if labels.insert(name, MemberDisplay::new(title)).is_some() {
            return Err(fail("duplicate display key"));
        }
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HELP-LABEL: duplicate names and empty titles never silently become an entry.
    #[test]
    fn side_files_refuse_duplicate_keys_and_empty_titles() {
        let read = |data| read_labels(data, |key| Some(key.to_owned()));
        let labels = read("helper\tReviewed title\nother\tOther title\n").unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels["helper"].title, "Reviewed title");
        assert!(read("helper\tTitle\nhelper\tOther\n").is_err());
        assert!(read("helper\t  \n").is_err());
        assert!(read("helper\tOne\tTwo\n").is_err());
        assert!(read_labels("bad\tTitle", |_| None::<String>).is_err());
        assert!(
            read_labels("one\tFirst\ntwo\tSecond", |_| Some(0)).is_err(),
            "the parsed member identity, rather than the input spelling, owns an entry"
        );
    }
}
