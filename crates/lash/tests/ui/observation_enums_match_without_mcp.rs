//! TOOL-LIVE-FEATURES: observation matches need no plugin vocabulary.
use lash::observe::SessionObservationStreamItem;
use lash::process::ProcessObservationStreamItem;
use lash::{ToolLiveFact, ToolLiveItem, ToolLivePayload};

fn main() {
    let _: fn(SessionObservationStreamItem) = |item| match item {
        SessionObservationStreamItem::Event(_) => (),
        SessionObservationStreamItem::Gap { .. } => (),
    };
    let _: fn(ProcessObservationStreamItem) = |item| match item {
        ProcessObservationStreamItem::Event(_) => (),
        ProcessObservationStreamItem::Gap { .. } => (),
    };
    let _: fn(ToolLiveItem) = |item| match item {
        ToolLiveItem::Emission { emission, .. } => match emission.payload {
            ToolLivePayload::Fact { fact } => match fact {
                ToolLiveFact::Progress { .. } => (),
                ToolLiveFact::Log { .. } => (),
            },
            ToolLivePayload::Omitted { .. } => (),
        },
        ToolLiveItem::Gap { .. } => (),
    };
}
