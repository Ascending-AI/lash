//! A scenario's turn arrives the way a host's does: the session's pending
//! input row and its actor's wake commit together, and the session actor's
//! mail drain admits it as the run its source key names.

use lash_core::runtime::durable::session::{TurnError, TurnRow};
use lash_core::{Message, MessageRole, Part, facade_support::shared_parts};
use lash_core_execution::Backend;
use lash_core_execution::{InputItem, PendingTurnInputDraft, TurnInput, TurnInputIngress};
use lash_sansio::{InputId, SessionId, TurnId};

/// Admit `session` to the catalog at its creation head, and send it `text`
/// as the input run `run` takes.
pub async fn send_turn(
    backend: &Backend,
    session: &SessionId,
    run: &TurnId,
    text: &str,
) -> Result<InputId, String> {
    let catalog: std::sync::Arc<dyn lash_core_execution::RuntimeStore> =
        backend.session_store_factory();
    lash_core_store::testing::store_fixtures::admit_conformance_session(&catalog, session).await;
    queue_turn(backend, session, run, text).await
}

/// Send the admitted `session` `text` as the input run `run` takes: it
/// waits in the session's mail behind what the session already holds.
pub async fn queue_turn(
    backend: &Backend,
    session: &SessionId,
    run: &TurnId,
    text: &str,
) -> Result<InputId, String> {
    backend
        .session_store_factory()
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session.clone(),
                TurnInputIngress::NextTurn,
                TurnInput::text(text),
            )
            .with_source_key(run.as_str()),
        )
        .await
        .map(|input| input.input_id)
        .map_err(|error| format!("send the turn's input: {error}"))
}

/// The messages `row`'s turn starts from: one user message per input its
/// admission took, read back from the session's store.
pub async fn admitted_messages(
    backend: &Backend,
    row: &TurnRow,
) -> Result<Vec<Message>, TurnError> {
    let catalog = backend.session_store_factory();
    let mut messages = Vec::with_capacity(row.admission.input_ids().len());
    for input in row.admission.input_ids() {
        let read = catalog
            .pending_turn_input(&row.session, input)
            .await
            .map_err(|error| TurnError::Exec(error.to_string()))?
            .ok_or_else(|| TurnError::Exec(format!("input {input} is not stored")))?;
        let text = read
            .input
            .input
            .items
            .iter()
            .filter_map(|item| match item {
                InputItem::Text { text } => Some(text.as_str()),
                InputItem::Attachment { .. } => None,
            })
            .collect::<String>();
        let id = format!("input-{input}");
        messages.push(Message {
            id: id.clone(),
            role: MessageRole::User,
            parts: shared_parts(vec![Part::text(format!("{id}.p0"), text, None)]),
            origin: None,
            reply_marker: None,
        });
    }
    Ok(messages)
}
