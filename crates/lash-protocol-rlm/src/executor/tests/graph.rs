//! The laws of a session's paged state (FIG-6193): saved functions run
//! with what they froze after a later rebinding and a cold restore, a
//! working state is adopted whole and a rolled-back cell never promotes
//! what it captured, states restored from one descriptor share its pages
//! but not their heaps, and a host read of more pages than the page cache
//! holds completes.

use std::sync::Arc;

use lash_core_store::execution_state::{ExecutionGraphCapture, ExecutionGraphPurpose};

use super::saved_functions::NoTools;
use super::{
    CodeModeExecutionState, SESSION, TURN, finish_of, open_host, typescript_services,
    typescript_state,
};
use crate::testing::{DurableHost, cell_context, commit_working, run_cell};

async fn cell(
    state: &mut CodeModeExecutionState,
    host: &DurableHost,
    key: &'static str,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, Arc::new(NoTools)),
        &typescript_services(),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// A fresh session restored from `state`'s descriptor, which `host`
/// committed as its turn's working state.
async fn restored(host: &DurableHost, state: &CodeModeExecutionState) -> CodeModeExecutionState {
    let graph = state
        .hydrated_execution_graph()
        .await
        .expect("the session holds a descriptor");
    let mut restored = typescript_state();
    restored
        .restore_execution_graph(&graph, ExecutionGraphPurpose::Working)
        .expect("the descriptor restores");
    let _ = host;
    restored
}

/// The root and node count a capture sends.
fn sent(capture: &ExecutionGraphCapture) -> (lash_core_store::checkpoint_graph::NodeId, usize) {
    let ExecutionGraphCapture::Replace { root, nodes } = capture else {
        panic!("a session with bindings captures a root");
    };
    (*root, nodes.len())
}

/// Frozen captures and session carry: a function saved in one cell keeps
/// what it read when a later cell rebinds it, mutually recursive saved
/// functions call one another, and an alias holds the function it was
/// bound to. Restored cold from the committed descriptor alone, in a
/// session that never ran the cells, every function runs with what it
/// froze. The rebinding's commit sends fewer pages than the cell that
/// made the functions.
#[tokio::test(flavor = "multi_thread")]
async fn saved_functions_keep_what_they_froze_across_a_rebinding_and_a_cold_restore() {
    let host = open_host().await;
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        "let rate = 3;\nfunction isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); }\nfunction isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); }\nconst scale = (x: number) => x * rate;\nconst alias = scale;",
    )
    .await;
    let (first, made) = sent(&commit_working(&mut state, &host).await);

    cell(&mut state, &host, "exec-code:1", "rate = 100;").await;
    let (second, rebound) = sent(&commit_working(&mut state, &host).await);
    assert_ne!(second, first, "the rebinding is the session's");
    assert!(
        rebound < made,
        "the rebinding sends only the pages it changed: {rebound} of {made}"
    );
    let mut cold = restored(&host, &state).await;
    drop(state);
    let called = cell(
        &mut cold,
        &host,
        "exec-code:2",
        "await control.finish([isEven(4), isOdd(4), scale(2), alias(2), rate]);",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!([true, false, 6, 6, 100])
    );
}

/// PC-WORKING, adoption half: the state a cell left is installed into a
/// fresh executor before the next cell, its first capture then sends no
/// page, and a next cell that is cancelled after its state was captured
/// leaves the working state exactly as it was: its staged capture is
/// dropped, never promoted.
#[tokio::test(flavor = "multi_thread")]
async fn a_working_state_is_adopted_whole_and_a_cancelled_cell_never_promotes_its_capture() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        "const kept = [1, 2];\nfunction twice(x: number) { return 2 * x; }",
    )
    .await;
    let (working, _) = sent(&commit_working(&mut source, &host).await);
    let mut state = restored(&host, &source).await;
    drop(source);

    let adopted = state
        .capture_execution_graph(ExecutionGraphPurpose::Working)
        .await
        .expect("capture");
    assert_eq!(
        adopted,
        ExecutionGraphCapture::Replace {
            root: working,
            nodes: Default::default(),
        },
        "the restored state is the working state, every page already committed"
    );
    state.acknowledge_execution_state_capture().await;

    let speculative = Box::pin(super::super::execute_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", Arc::new(NoTools)),
        lash_core::ExecRequest {
            code: "kept.push(3);\nconst extra = twice(21);".to_owned(),
        },
        &typescript_services(),
        crate::projection::CodeModeProjectedBindings::default(),
    ))
    .await;
    assert!(speculative.error().is_none(), "{:?}", speculative.error());
    state.mark_code_execution_response_returned();
    let (root, _) = sent(
        &state
            .capture_execution_graph(ExecutionGraphPurpose::Working)
            .await
            .expect("capture the speculative state"),
    );
    assert_ne!(root, working, "the cell changed the heap");
    state.terminate_code_execution();

    assert_eq!(
        state
            .capture_execution_graph(ExecutionGraphPurpose::Working)
            .await
            .expect("capture"),
        ExecutionGraphCapture::Replace {
            root: working,
            nodes: Default::default(),
        },
        "the cancelled cell's captured heap is not the session's"
    );
    assert!(
        !state.names().contains(&lash_kernel_doc::Name::new("extra")),
        "the cancelled cell's binding is gone"
    );
    let read = cell(
        &mut state,
        &host,
        "exec-code:2",
        "await control.finish([kept, twice(4)]);",
    )
    .await;
    assert_eq!(finish_of(&read), serde_json::json!([[1, 2], 8]));
}

/// Fork isolation: two sessions restored from one descriptor read its
/// pages, but each has a heap of its own: what one cell changes, the other
/// never sees.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_restored_from_one_descriptor_share_its_pages_but_not_their_heaps() {
    let host = open_host().await;
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        &format!(
            "const notes = [\"{}\"];\nfunction label(x: string) {{ return 'n:' + x; }}",
            "x".repeat(1_000)
        ),
    )
    .await;
    commit_working(&mut source, &host).await;
    let mut left = restored(&host, &source).await;
    let mut right = restored(&host, &source).await;
    drop(source);
    cell(
        &mut left,
        &host,
        "exec-code:1",
        "notes.push(label('more'));",
    )
    .await;
    let seen = cell(
        &mut right,
        &host,
        "exec-code:2",
        "await control.finish(notes.length);",
    )
    .await;
    assert_eq!(
        finish_of(&seen),
        serde_json::json!(1),
        "the right heap is its own"
    );
    let grown = cell(
        &mut left,
        &host,
        "exec-code:3",
        "await control.finish(notes[1]);",
    )
    .await;
    assert_eq!(finish_of(&grown), serde_json::json!("n:more"));
}

/// Session page authority (FIG-6193): the cells of a turn read the
/// session's committed pages under one lease, which the first read takes
/// and every later cell reuses, so N cells make one lease write; and a
/// cell that rebinds one binding of many commits only the pages it
/// changed, never the session's whole heap.
#[tokio::test(flavor = "multi_thread")]
async fn a_turns_cells_share_one_lease_and_commit_only_the_pages_they_changed() {
    let host = open_host().await;
    let mut state = typescript_state();
    let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
    for index in 0..256 {
        patch.set_default.insert(
            format!("b{index}"),
            serde_json::json!(format!("{index}:{}", "x".repeat(1024))),
        );
    }
    state
        .patch_globals(&patch, &std::collections::BTreeSet::new())
        .await
        .expect("bind the session's values");
    let (_, whole) = sent(&commit_working(&mut state, &host).await);
    for (key, code) in [
        ("exec-code:0", "const r0 = b0.length;"),
        ("exec-code:1", "const r1 = b1.length;"),
        ("exec-code:2", "const r2 = b2.length;"),
        ("exec-code:3", "b7 = 'changed';"),
    ] {
        cell(&mut state, &host, key, code).await;
    }
    assert_eq!(
        state.lease_writes(),
        1,
        "the turn's cells read under the one lease the first read took"
    );
    let (_, changed) = sent(&commit_working(&mut state, &host).await);
    assert!(
        changed * 8 < whole,
        "the turn commits the pages its cells changed: {changed} of the session's {whole}"
    );
}

/// `len` letters whose pages never repeat one another, so content
/// addressing shares none of them.
fn distinct_text(seed: u64, len: usize) -> String {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + u8::try_from(state % 26).expect("a letter"))
        })
        .collect()
}

/// What a session restored cold from `source`'s committed state lists of
/// its bindings, reading through `cache`, with the pages it fetched; none
/// when the listing does not finish within a minute.
async fn listed(
    host: &DurableHost,
    source: &CodeModeExecutionState,
    cache: lash_vm_broker::kernel::pages::cache::PageCache,
) -> (
    Option<Vec<(String, crate::session_globals::SessionGlobalKind)>>,
    u64,
) {
    let mut cold = restored(host, source).await.caching(cache);
    let listing = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        cold.bound_variable_values(
            &std::collections::BTreeSet::new(),
            host.session_pages().as_ref(),
        ),
    )
    .await
    .ok()
    .map(|listing| listing.expect("the bindings read"));
    (listing, cold.page_fetches())
}

/// FIG-6419: a host projection that reads more pages than the page cache
/// holds completes. Listing bindings that hold twelve times a 1 MiB cache
/// in distinct text reads each page it needs from the store once, as many
/// as a cache that never evicts, and lists exactly what that cache lists.
#[tokio::test(flavor = "multi_thread")]
async fn a_projection_larger_than_the_page_cache_fetches_each_page_once() {
    use lash_vm_broker::kernel::pages::cache::PageCache;
    const CACHE: u64 = 1 << 20;
    let host = open_host().await;
    let mut state = typescript_state();
    let mut patch = lash_rlm_types::RlmGlobalsPatchPluginBody::default();
    for index in 0..48 {
        patch.set_default.insert(
            format!("b{index}"),
            serde_json::json!(distinct_text(index, 256 << 10)),
        );
    }
    state
        .patch_globals(&patch, &std::collections::BTreeSet::new())
        .await
        .expect("bind the session's values");
    commit_working(&mut state, &host).await;

    let unbounded = PageCache::new(u64::MAX);
    let (expected, distinct) = listed(&host, &state, unbounded.clone()).await;
    let expected = expected.expect("a cache that never evicts lists the session");
    assert!(
        unbounded.usage().1 > 10 * CACHE,
        "the projection reads ten caches' worth of pages: {:?}",
        unbounded.usage()
    );
    let (listing, fetches) = listed(&host, &state, PageCache::new(CACHE)).await;
    assert!(
        fetches <= distinct + 2,
        "each page is fetched once: {fetches} fetches of {distinct} distinct pages"
    );
    assert_eq!(
        listing.expect("a 1 MiB cache lists the session"),
        expected,
        "the listing is the unbounded cache's"
    );
}
