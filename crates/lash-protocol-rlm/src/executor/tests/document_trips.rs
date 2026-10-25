//! The law of a cell document's trips through the host (FIG-6257): the
//! worker hands the host the document's token body, and the host decodes it
//! once and passes the decoded document, its identity and those bytes to
//! the envelope, the store and the run. A saved function's description is
//! decoded once where the session reads it. Only its source call shape
//! reaches later lowering; its defining document stays on the execution path.
//! Nothing on the way is JSON.

use std::sync::Arc;

use lash_kernel_doc::document_work::{self, DocumentWork};

use super::{
    CellTools, SESSION, TURN, finish_of, open_host, typescript_services, typescript_state,
};
use crate::testing::{cell_context, run_cell};

/// A turn's host work runs on this test's one runtime thread, which is what
/// the counts are kept for; the worker is another process.
#[tokio::test]
async fn a_cells_document_is_decoded_once_on_the_host_and_never_as_json() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = super::super::CellServices {
        documents: Some(host.documents.clone()),
        ..typescript_services()
    };
    let mut state = typescript_state();
    let _ = document_work::take();

    let defined = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "function advance(value) { return value + 2; }",
    )
    .await;
    assert!(defined.error().is_none(), "{:?}", defined.error());
    assert_eq!(
        document_work::take(),
        DocumentWork {
            // The cell's document, and the saved function's own document
            // as the session reads its description at the cell's end.
            token_decodes: 2,
            ..DocumentWork::default()
        },
        "a cell that saves a function"
    );

    let called = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &services,
        "await control.finish(advance(40));",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!(42));
    assert_eq!(
        document_work::take(),
        DocumentWork {
            token_decodes: 1,
            ..DocumentWork::default()
        },
        "a cell lowered against a saved function"
    );
}
