//! Complete enumeration is the authority for destructive reclamation (ADR 0067 §5).
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use crate::StoreError;
use crate::artifact_referrer::ArtifactReferrerKind;

mod sealed {
    pub trait Sealed {}
}

/// A closed inventory of the sources a destructive boundary protects.
pub trait EnumerationSource: sealed::Sealed + Copy + Ord + Debug {
    const SCOPE: &'static str;
    fn all() -> Vec<Self>;
}

/// Whether a source has another page. Only the terminal page exhausts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnumerationProgress {
    More,
    Exhausted,
}

/// SQLite's shared blob table also stores artifacts behind namespace pointers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqliteBlobRootSource {
    Checkpoints,
    ArtifactPointers,
    CheckpointComponents,
}
impl sealed::Sealed for SqliteBlobRootSource {}
impl EnumerationSource for SqliteBlobRootSource {
    const SCOPE: &'static str = "SQLite blob roots";
    fn all() -> Vec<Self> {
        vec![
            Self::Checkpoints,
            Self::ArtifactPointers,
            Self::CheckpointComponents,
        ]
    }
}

/// PostgreSQL's session blobs are separate from its artifact byte table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PostgresBlobRootSource {
    Checkpoints,
    CheckpointComponents,
}
impl sealed::Sealed for PostgresBlobRootSource {}
impl EnumerationSource for PostgresBlobRootSource {
    const SCOPE: &'static str = "PostgreSQL blob roots";
    fn all() -> Vec<Self> {
        vec![Self::Checkpoints, Self::CheckpointComponents]
    }
}

impl sealed::Sealed for ArtifactReferrerKind {}
impl EnumerationSource for ArtifactReferrerKind {
    const SCOPE: &'static str = "artifact referrers";
    fn all() -> Vec<Self> {
        Self::ALL.to_vec()
    }
}

pub type CompleteArtifactReferrers =
    CompleteEnumeration<ArtifactReferrerKind, (ArtifactReferrerKind, String)>;

/// An unfinished scan. There is no conversion from its accumulated rows to a
/// witness. Each source starts at page zero and must reach its terminal page.
#[derive(Debug)]
pub struct ReclamationEnumeration<S: EnumerationSource, T: Ord> {
    pages: BTreeMap<S, (usize, bool)>,
    values: BTreeSet<T>,
    invalid: bool,
}
impl<S: EnumerationSource, T: Ord> Default for ReclamationEnumeration<S, T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<S: EnumerationSource, T: Ord> ReclamationEnumeration<S, T> {
    pub fn new() -> Self {
        Self {
            pages: S::all()
                .into_iter()
                .map(|source| (source, (0, false)))
                .collect(),
            values: BTreeSet::new(),
            invalid: false,
        }
    }

    /// Record one fully read page, in order. A failed read must return its
    /// error before calling this, and a limited query must carry `More` until
    /// its continuation is exhausted.
    pub fn page(
        &mut self,
        source: S,
        page: usize,
        values: impl IntoIterator<Item = T>,
        progress: EnumerationProgress,
    ) -> Result<(), StoreError> {
        if self.invalid {
            return Err(Self::incomplete("a prior page was invalid".into()));
        }
        let Some((next, exhausted)) = self.pages.get_mut(&source) else {
            self.invalid = true;
            return Err(Self::incomplete(format!("unrecognized source {source:?}")));
        };
        if *exhausted || *next != page {
            self.invalid = true;
            return Err(Self::incomplete(format!(
                "out-of-order page {page} for {source:?}"
            )));
        }
        *next = next
            .checked_add(1)
            .ok_or_else(|| Self::incomplete(format!("page overflow for {source:?}")))?;
        *exhausted = progress == EnumerationProgress::Exhausted;
        self.values.extend(values);
        Ok(())
    }

    pub fn finish(self) -> Result<CompleteEnumeration<S, T>, StoreError> {
        if self.invalid {
            return Err(Self::incomplete("a prior page was invalid".into()));
        }
        if let Some((source, _)) = self.pages.iter().find(|(_, (_, exhausted))| !exhausted) {
            return Err(Self::incomplete(format!(
                "source {source:?} has not exhausted its pages"
            )));
        }
        Ok(CompleteEnumeration {
            values: self.values,
            source: std::marker::PhantomData,
        })
    }

    fn incomplete(unfinished: String) -> StoreError {
        StoreError::IncompleteEnumeration {
            scope: S::SCOPE,
            unfinished,
        }
    }
}

/// Proof that every source and page in `S` was read. No raw-set constructor,
/// default, deserializer, or mutable access exists.
#[derive(Debug, PartialEq, Eq)]
pub struct CompleteEnumeration<S: EnumerationSource, T: Ord> {
    values: BTreeSet<T>,
    source: std::marker::PhantomData<S>,
}
impl<S: EnumerationSource, T: Ord> CompleteEnumeration<S, T> {
    pub fn contains(&self, value: &T) -> bool {
        self.values.contains(value)
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, T> {
        self.values.iter()
    }
    pub fn values(&self) -> &BTreeSet<T> {
        &self.values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn partial_scans<S: EnumerationSource>() {
        for omitted in S::all() {
            for progress in [None, Some(EnumerationProgress::More)] {
                let mut scan = ReclamationEnumeration::<S, String>::new();
                for source in S::all() {
                    if source == omitted {
                        if let Some(progress) = progress {
                            scan.page(source, 0, ["live".into()], progress)
                                .expect("valid enumeration page");
                        }
                    } else {
                        scan.page(source, 0, [], EnumerationProgress::Exhausted)
                            .expect("valid enumeration page");
                    }
                }
                assert!(
                    matches!(scan.finish(), Err(StoreError::IncompleteEnumeration { scope, .. }) if scope == S::SCOPE)
                );
            }
        }
    }

    #[test]
    fn every_skipped_source_and_truncated_page_refuses_a_witness() {
        partial_scans::<SqliteBlobRootSource>();
        partial_scans::<PostgresBlobRootSource>();
        partial_scans::<ArtifactReferrerKind>();
    }

    #[test]
    fn only_exhausted_ordered_pages_mint_a_witness() {
        let mut scan = ReclamationEnumeration::<PostgresBlobRootSource, String>::new();
        scan.page(
            PostgresBlobRootSource::Checkpoints,
            0,
            ["head".into()],
            EnumerationProgress::More,
        )
        .expect("valid enumeration page");
        scan.page(
            PostgresBlobRootSource::Checkpoints,
            1,
            ["anchor".into()],
            EnumerationProgress::Exhausted,
        )
        .expect("valid enumeration page");
        scan.page(
            PostgresBlobRootSource::CheckpointComponents,
            0,
            ["child".into()],
            EnumerationProgress::Exhausted,
        )
        .expect("valid enumeration page");
        let complete = scan.finish().expect("valid enumeration page");
        assert_eq!(complete.len(), 3);
        assert!(complete.contains(&"anchor".into()));
        let mut scan = ReclamationEnumeration::<PostgresBlobRootSource, String>::new();
        for source in PostgresBlobRootSource::all() {
            scan.page(source, 0, [], EnumerationProgress::Exhausted)
                .expect("valid enumeration page");
        }
        assert!(
            scan.page(
                PostgresBlobRootSource::Checkpoints,
                0,
                [],
                EnumerationProgress::Exhausted
            )
            .is_err()
        );
        assert!(
            scan.finish().is_err(),
            "an ignored invalid-page result cannot mint a witness"
        );
    }
}
