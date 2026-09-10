use super::ResponsesStreamingToolCall;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum ResponsesPartSlotIdentity {
    OutputIndex(usize),
    ItemId(String),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ResponsesPartKind {
    Message,
    Reasoning,
    ToolCall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponsesPartSlotAllocation {
    Resolve,
    ReuseCurrent,
    Fresh,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ResponsesPartSlotKey {
    pub(super) kind: ResponsesPartKind,
    pub(super) identity: ResponsesPartSlotIdentity,
}

#[derive(Clone, Debug)]
pub(crate) enum ResponsesPartSlot {
    Message(usize),
    Reasoning(usize),
    ToolCall(ResponsesStreamingToolCall),
}

impl ResponsesPartSlot {
    pub(super) fn kind(&self) -> ResponsesPartKind {
        match self {
            Self::Message(_) => ResponsesPartKind::Message,
            Self::Reasoning(_) => ResponsesPartKind::Reasoning,
            Self::ToolCall(_) => ResponsesPartKind::ToolCall,
        }
    }
}
