use super::{Capture, ContentCategory, ContentPolicy};
use crate::{
    PolicyOperand, PolicyRelation, PolicyValue, TraceDetail, TraceIds, TracePolicyError,
    TracePolicyField, TracePolicySelection, TracePolicySnapshot, TraceSpanKind, TraceSpanKindSet,
};

/// Runs before a record or optional leaf is built. It must not block or re-enter.
pub trait CaptureSelector: Send + Sync + 'static {
    fn select(&self, at: &SelectorView<'_>, narrow: &mut TraceNarrowing);
}
pub struct SelectorView<'a> {
    pub kind: TraceSpanKind,
    pub ids: &'a TraceIds,
    pub policy: &'a TracePolicySnapshot,
}
/// An execution-local narrowing, inherited by children of a leaf, including
/// a skipped leaf. No method can restore consent or turn a kind back on.
#[derive(Debug, PartialEq, Eq)]
pub struct TraceNarrowing {
    content: ContentPolicy,
    off: TraceSpanKindSet,
}
impl TraceNarrowing {
    pub(crate) fn new(policy: &TracePolicySnapshot) -> Self {
        let mut off = TraceSpanKindSet::of(&[]);
        for kind in TraceSpanKind::ALL {
            if !policy.detail.builds(kind) {
                off = off.with(kind);
            }
        }
        Self {
            content: policy.content,
            off,
        }
    }
    pub(crate) fn inherited(&self) -> Self {
        Self {
            content: self.content,
            off: self.off,
        }
    }
    pub fn omit(&mut self, category: ContentCategory) {
        self.content = self.content.with(category, Capture::Omitted);
    }
    pub fn omit_all(&mut self) {
        self.content = ContentPolicy::omitted();
    }
    pub fn skip(&mut self, kind: TraceSpanKind) -> Result<(), TracePolicyError> {
        if kind.is_container() {
            return Err(TracePolicyError::Structural {
                field: TracePolicyField::Selector(kind),
                selection: TracePolicySelection::Off,
            });
        }
        if let Some(child) = kind
            .children()
            .iter()
            .find(|c| *c != kind && !self.off.contains(*c))
        {
            return Err(TracePolicyError::Relation {
                field: TracePolicyField::Selector(kind),
                value: PolicyValue::Detail(TraceDetail::Off),
                relation: PolicyRelation::ParentOffChildOn,
                other: PolicyOperand::Field(TracePolicyField::Selector(child)),
                other_value: PolicyValue::Detail(TraceDetail::Standard),
            });
        }
        self.off = self.off.with(kind);
        Ok(())
    }
    pub fn content(&self) -> ContentPolicy {
        self.content
    }
    pub fn builds(&self, kind: TraceSpanKind) -> bool {
        !self.off.contains(kind)
    }
}
