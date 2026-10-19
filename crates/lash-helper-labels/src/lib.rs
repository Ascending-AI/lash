//! Display data keyed by the name on a document's `use` line.
//!
//! Labels carry no execution semantics and are never part of a function's
//! definition or identity. Reading them links no dialect or helper release.
//! Beside its labels, each library declares which of its functions are
//! comparisons ([`Library::comparisons`]), so a reader shows a condition as
//! the comparison its dialect wrote with no table of dialect names of its own.

use lash_kernel_doc::FunctionName;
use std::collections::BTreeMap;

/// A host-facing title for a library function. Roles belong to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub title: String,
}

/// Labels are keyed by name, including when several retained ids share a name.
pub type FunctionLabels = BTreeMap<FunctionName, Label>;

/// One closed library, with its own reviewed label side file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Library {
    Kernel,
    TypeScript,
    Python,
    Regex,
    Date,
    Url,
}

impl Library {
    pub const ALL: [Self; 6] = [
        Self::Kernel,
        Self::TypeScript,
        Self::Python,
        Self::Regex,
        Self::Date,
        Self::Url,
    ];

    fn data(self) -> &'static str {
        match self {
            Self::Kernel => include_str!("../labels/kernel.tsv"),
            Self::TypeScript => include_str!("../labels/typescript.tsv"),
            Self::Python => include_str!("../labels/python.tsv"),
            Self::Regex => include_str!("../labels/regex.tsv"),
            Self::Date => include_str!("../labels/date.tsv"),
            Self::Url => include_str!("../labels/url.tsv"),
        }
    }

    /// Reads this library's labels, refusing duplicate names and empty titles.
    pub fn labels(self) -> Result<FunctionLabels, LabelError> {
        read(self.data())
    }

    /// This library's functions that decide one comparison of their
    /// arguments, in parameter order: each one's [`Comparison`] holds for
    /// exactly the arguments the function answers `true` for, under the
    /// library's own meaning of equality, order or truth. A function that
    /// takes its operands in another order, packs them, or also converts
    /// them is not listed.
    pub fn comparisons(self) -> &'static [(&'static str, Comparison)] {
        use Comparison::{Contains, EndsWith, Eq, Ge, Gt, Le, Lt, Ne, Not, StartsWith};
        match self {
            Self::Kernel => &[
                ("bool.not", Not),
                ("eq", Eq),
                ("float.eq", Eq),
                ("float.ge", Ge),
                ("float.gt", Gt),
                ("float.le", Le),
                ("float.lt", Lt),
                ("ge", Ge),
                ("gt", Gt),
                ("int.eq", Eq),
                ("int.ge", Ge),
                ("int.gt", Gt),
                ("int.le", Le),
                ("int.lt", Lt),
                ("le", Le),
                ("list.contains", Contains),
                ("lt", Lt),
                ("map.contains", Contains),
                ("num.eq", Eq),
                ("num.ge", Ge),
                ("num.gt", Gt),
                ("num.le", Le),
                ("num.lt", Lt),
                ("record.contains", Contains),
                ("set.contains", Contains),
                ("text.ends_with", EndsWith),
                ("text.starts_with", StartsWith),
                ("tuple.contains", Contains),
            ],
            Self::TypeScript => &[
                ("ts.ge", Ge),
                ("ts.gt", Gt),
                ("ts.le", Le),
                ("ts.lt", Lt),
                ("ts.not", Not),
                ("ts.strict_equals", Eq),
                ("ts.strict_not_equals", Ne),
            ],
            Self::Python => &[
                ("py.contains", Contains),
                ("py.method.endswith", EndsWith),
                ("py.method.startswith", StartsWith),
            ],
            Self::Regex | Self::Date | Self::Url => &[],
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

/// Every shipped library's comparisons, by function name.
pub fn comparisons() -> BTreeMap<&'static str, Comparison> {
    Library::ALL
        .into_iter()
        .flat_map(Library::comparisons)
        .copied()
        .collect()
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

fn read(data: &str) -> Result<FunctionLabels, LabelError> {
    let mut labels = FunctionLabels::new();
    for (index, row) in data.lines().enumerate() {
        let fail = |message: &str| LabelError {
            line: index + 1,
            message: message.into(),
        };
        let (name, title) = row
            .split_once('\t')
            .ok_or_else(|| fail("expected name and title"))?;
        let name = FunctionName::new(name).map_err(|_| fail("invalid function name"))?;
        if title.trim().is_empty() || title.contains('\t') {
            return Err(fail("expected one non-empty title"));
        }
        if labels
            .insert(
                name,
                Label {
                    title: title.into(),
                },
            )
            .is_some()
        {
            return Err(fail("duplicate function name"));
        }
    }
    Ok(labels)
}

/// Reads all shipped labels independently of installed dialect features.
pub fn labels() -> Result<FunctionLabels, LabelError> {
    let mut labels = FunctionLabels::new();
    for library in Library::ALL {
        for (name, label) in library.labels()? {
            if labels.insert(name, label).is_some() {
                return Err(LabelError {
                    line: 0,
                    message: "name shared by label libraries".into(),
                });
            }
        }
    }
    Ok(labels)
}

/// Unknown (including removed) helpers display their bare document name.
pub fn title<'a>(labels: &'a FunctionLabels, name: &'a FunctionName) -> &'a str {
    labels
        .get(name)
        .map_or_else(|| name.as_str(), |label| label.title.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HELP-LABEL: a reader with only kernel-doc labels real TS and Python calls.
    #[test]
    fn a_reader_without_dialects_reads_labels_and_unknown_names() {
        let labels = labels().unwrap();
        for (name, expected) in [
            ("ts.array.map", "TypeScript array map"),
            ("py.str", "Python str"),
        ] {
            assert_eq!(title(&labels, &FunctionName::new(name).unwrap()), expected);
        }
        let unknown = FunctionName::new("future.removed_helper").unwrap();
        assert_eq!(title(&labels, &unknown), "future.removed_helper");
    }

    /// HELP-COMPARE: a library declares comparisons only of functions it
    /// labels, so a comparison name is never a dialect name of the reader's.
    #[test]
    fn a_library_declares_comparisons_only_of_its_labelled_functions() {
        for library in Library::ALL {
            let labels = library.labels().unwrap();
            for (name, _) in library.comparisons() {
                assert!(
                    labels.contains_key(&FunctionName::new(*name).unwrap()),
                    "{library:?} declares {name} a comparison but does not label it"
                );
            }
        }
    }

    /// HELP-LABEL: duplicate names and empty titles never silently become an entry.
    #[test]
    fn side_files_refuse_duplicate_names_and_empty_titles() {
        assert!(read("helper\tTitle\nhelper\tOther\n").is_err());
        assert!(read("helper\t  \n").is_err());
        assert!(read("helper\tOne\tTwo\n").is_err());
    }
}
