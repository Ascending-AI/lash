//! L3 (FIG-5172): a sent input's turn runs on the core's node, through the
//! production turn driver, and its handle answers the committed reply.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sent_input_runs_on_the_cores_node_and_answers_its_reply() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session_id = lash_sansio::SessionId::try_from("facade-turn".to_owned()).expect("id");
    let session = core
        .session(session_id)
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created");
    let output = session
        .send(crate::TurnInput::text("hello"))
        .output()
        .await
        .expect("the turn answers");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("echo: hello"));
    core.shutdown().await.expect("shutdown");
}
