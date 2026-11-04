//! Owned boundary values retain exact content and provenance, not prices.
use super::*;
use crate::paged::FrozenRef;
use lash_kernel_doc::heap::PageValue;
use lash_kernel_value::tokens::encode_body;

#[derive(Clone)]
pub enum PublicationValue {
    Native(Value),
    Prepared(crate::arena::PreparedValueProof),
    Page(PageValue),
    Frozen(FrozenRef),
    Capture(ArenaHandle),
    Existing(Owner),
    Frame,
    /// A break or continue departs with no value.
    Departure,
}
impl PublicationValue {
    pub(in crate::paged::logical) fn root(&self) -> RootValue<'_> {
        match self {
            Self::Native(value) => RootValue::Value(value),
            Self::Prepared(value) => RootValue::Prepared(value),
            Self::Page(value) => RootValue::Page(value),
            Self::Frozen(value) => RootValue::Frozen(value),
            Self::Capture(handle) => RootValue::Capture(*handle),
            Self::Existing(owner) => RootValue::Existing(*owner),
            Self::Frame => RootValue::Frame,
            Self::Departure => RootValue::Departure,
        }
    }
    pub(in crate::paged::logical) fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(a), Self::Native(b)) => a.same(b),
            (Self::Prepared(a), Self::Prepared(b)) => a.same(b),
            (Self::Page(a), Self::Page(b)) => encode_body(a) == encode_body(b),
            (Self::Frozen(a), Self::Frozen(b)) => a.root.id == b.root.id && a.summary == b.summary,
            (Self::Capture(a), Self::Capture(b)) => a == b,
            (Self::Existing(a), Self::Existing(b)) => a == b,
            (Self::Frame, Self::Frame) | (Self::Departure, Self::Departure) => true,
            _ => false,
        }
    }
    pub(in crate::paged::logical) fn handles(&self, out: &mut Vec<ArenaHandle>) {
        match self {
            Self::Native(value) => out.extend(value.handle()),
            Self::Prepared(value) => out.extend_from_slice(value.graph().dependencies()),
            Self::Capture(handle) => out.push(*handle),
            _ => {}
        }
    }
}

impl From<RootValue<'_>> for PublicationValue {
    fn from(value: RootValue<'_>) -> Self {
        match value {
            RootValue::Value(value) => Self::Native(*value),
            RootValue::Prepared(value) => Self::Prepared((*value).clone()),
            RootValue::Page(value) => Self::Page(value.clone()),
            RootValue::Frozen(value) => Self::Frozen(*value),
            RootValue::Existing(owner) => Self::Existing(owner),
            RootValue::Frame => Self::Frame,
            RootValue::Capture(handle) => Self::Capture(handle),
            RootValue::Departure => Self::Departure,
        }
    }
}
