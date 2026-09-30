//! Root witnesses are minted only after every source and continuation is read.
use super::{AttachmentId, AttachmentRootSet};
use crate::StoreError;
use crate::artifact_referrer::ArtifactReferrerKind;
use std::collections::BTreeSet;

/// Every kind that can hold attachments, the remaining edge kinds, and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentRootSource {
    Referrer(ArtifactReferrerKind),
    OtherReferrers,
    PendingWrites,
}
impl AttachmentRootSource {
    pub fn all() -> impl Iterator<Item = Self> {
        ArtifactReferrerKind::ALL
            .into_iter()
            .filter(|kind| kind.holds_attachments())
            .map(Self::Referrer)
            .chain([Self::OtherReferrers, Self::PendingWrites])
    }
}

/// One unproven page. Backends read at most `QUERY_LIMIT` distinct, ordered ids;
/// the extra row distinguishes a terminal page from one with a continuation.
#[derive(Debug)]
pub struct AttachmentRootPage {
    roots: Vec<AttachmentId>,
    next: Option<AttachmentId>,
}
impl AttachmentRootPage {
    pub const QUERY_LIMIT: usize = 257;

    pub fn from_rows(mut roots: Vec<AttachmentId>) -> Result<Self, StoreError> {
        if roots.len() > Self::QUERY_LIMIT || roots.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(incomplete(
                "root page is oversized, duplicated or out of order",
            ));
        }
        let next = if roots.len() == Self::QUERY_LIMIT {
            roots.pop();
            roots.last().cloned()
        } else {
            None
        };
        Ok(Self { roots, next })
    }
}

/// Every live attachment root, with complete source and page coverage. Only
/// the shared collector can construct this; it has no raw-set conversion.
#[derive(Debug, PartialEq, Eq)]
pub struct CompleteAttachmentRoots {
    roots: BTreeSet<AttachmentId>,
}
impl CompleteAttachmentRoots {
    pub fn contains(&self, id: &AttachmentId) -> bool {
        self.roots.contains(id)
    }
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
    pub fn len(&self) -> usize {
        self.roots.len()
    }
    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, AttachmentId> {
        self.roots.iter()
    }
    pub fn values(&self) -> &BTreeSet<AttachmentId> {
        &self.roots
    }
}

pub(super) async fn enumerate<R: AttachmentRootSet + ?Sized>(
    authority: &R,
) -> Result<CompleteAttachmentRoots, StoreError> {
    let mut roots = BTreeSet::new();
    for source in AttachmentRootSource::all() {
        let mut after = None;
        loop {
            let page = authority
                .attachment_root_page(source, after.as_ref())
                .await?;
            if after
                .as_ref()
                .is_some_and(|cursor| page.roots.first().is_some_and(|first| first <= cursor))
            {
                return Err(incomplete("root continuation did not advance"));
            }
            roots.extend(page.roots);
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
    }
    Ok(CompleteAttachmentRoots { roots })
}
fn incomplete(unfinished: &str) -> StoreError {
    StoreError::IncompleteEnumeration {
        scope: "live attachment roots",
        unfinished: unfinished.into(),
    }
}
