//! Deferred tools: a search observation links a tool for the next cell, and
//! a link the cell cannot make reaches the model as a typed link error.

use super::*;

/// A provider whose call `n` answers `respond(n, serialized request)`.
fn inspecting_provider(
    respond: impl Fn(usize, String) -> String + Send + Sync + 'static,
) -> ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("workbench-harness")
        .complete(move |request| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let request =
                serde_json::to_string(&request.messages).expect("serialize the provider request");
            let cell = respond(call, request);
            async move { Ok(text_response(&cell)) }
        })
        .build()
        .into_handle()
}

/// The assistant rows the page renders for the current session.
async fn assistant_rows(state: &AppState) -> Vec<String> {
    state_rows(&read_state(state, None).await.expect("read the state"))
        .into_iter()
        .filter(|(role, _)| role == "assistant")
        .map(|(_, text)| text)
        .collect()
}

/// A `tools.search` observation links the tool it found, so the next cell
/// of the same turn calls it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_search_observation_enables_next_block_call() {
    let provider = inspecting_provider(|call, request| match call {
        0 => r#"<typescript>
const matches = await tools.search({ query: "text checksum", limit: 1 });
console.log(JSON.stringify(matches));
</typescript>"#
            .to_string(),
        1 => {
            assert!(request.contains("text.sha256"), "{request}");
            r#"<typescript>
const result = await text.sha256({ text: "restart proof" });
finish(result.digest);
</typescript>"#
                .to_string()
        }
        other => panic!("unexpected deferred round-trip provider call {other}"),
    });
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    run_turn(
        state,
        "Find the checksum utility, then checksum restart proof.",
    )
    .await;
    assert_eq!(
        assistant_rows(state).await,
        vec!["6aaa2c8b150bc016006f9d88df2e273adea1965bc4e8c66f87357b62a8e3afc9".to_string()]
    );
    workbench.shutdown().await;
}

/// A tool discovered in a cell cannot be linked by that same cell, and a
/// path no one serves is a link error: both reach the model, and the turn
/// recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_block_discovery_cannot_relink_and_unknown_paths_report_link_errors() {
    let provider = inspecting_provider(|call, request| match call {
        0 => r#"<typescript>
const matches = await tools.search({ query: "text checksum", limit: 1 });
const result = await text.sha256({ text: "too soon" });
finish(result.digest);
</typescript>"#
            .to_string(),
        1 => {
            assert!(request.contains("text.sha256"), "{request}");
            assert!(request.contains("link"), "{request}");
            r#"<typescript>
const result = await mystery.not_real({});
finish(result);
</typescript>"#
                .to_string()
        }
        2 => {
            assert!(request.contains("mystery.not_real"), "{request}");
            assert!(request.contains("link"), "{request}");
            finish_cell("typed link failures observed")
        }
        other => panic!("unexpected deferred link-error provider call {other}"),
    });
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    run_turn(state, "Exercise deferred link failures.").await;
    assert_eq!(
        assistant_rows(state).await,
        vec!["typed link failures observed".to_string()]
    );
    workbench.shutdown().await;
}
