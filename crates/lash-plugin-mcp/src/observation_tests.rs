//! TOOL-LIVE-LIVENESS: a long call survives later completed registrations.
use crate::observations::{McpRoutePolicy, Routes};
use futures_util::StreamExt;
use lash_core::{ToolLive, ToolLiveFact, ToolLiveFilter, ToolLiveItem, ToolLivePayload};
use std::{sync::Arc, time::Duration};

#[tokio::test(start_paused = true)]
async fn a_twenty_minute_call_keeps_its_route_through_one_thousand_later_calls() {
    let window = Arc::new(lash_core::ToolLiveMemoryWindow::default());
    let lane = ToolLive::new(Some(window), None, Default::default());
    let context = lash_core::testing::mock_attempt_context().with_tool_live(&lane);
    let mut feed = lane
        .subscribe(ToolLiveFilter::Call(context.live().address().clone()))
        .await
        .unwrap();
    let mut routes = Routes::new("fixture", McpRoutePolicy::default());
    let token = rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(1));
    routes.register(
        token.clone(),
        rmcp::model::NumberOrString::Number(1),
        &context,
    );
    for id in 2..=1001 {
        let request = rmcp::model::NumberOrString::Number(id);
        routes.register(
            rmcp::model::ProgressToken(request.clone()),
            request.clone(),
            &context,
        );
        routes.end(&request);
    }
    tokio::time::advance(Duration::from_secs(20 * 60)).await;
    routes.progress(rmcp::model::ProgressNotificationParam::new(
        token.clone(),
        100.0,
    ));
    // A later-polled stale task cannot regress MCP's increasing gauge.
    routes.progress(rmcp::model::ProgressNotificationParam::new(token, 99.0));
    context.live().finish();
    loop {
        if let ToolLiveItem::Emission { emission, .. } = feed.next().await.unwrap().unwrap()
            && let ToolLivePayload::Fact {
                fact: ToolLiveFact::Progress { progress, .. },
            } = emission.payload
        {
            assert_eq!(progress, 100.0);
            break;
        }
    }
}
