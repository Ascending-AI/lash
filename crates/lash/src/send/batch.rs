//! Many inputs, one acceptance (FIG-3842):
//! [`LashSession::send_batch`](crate::LashSession::send_batch).
//!
//! A batch is one request. Its inputs are accepted in one store transaction,
//! under one shared [`RunSpec`] the session interns once. Inputs whose ids are
//! new are enqueued in request order as one contiguous block of the session's
//! ingress: no other producer's input or command lands inside it. An id a
//! stored input already answers with identical content (a retry after a lost
//! reply, even one that settled since) returns that input's handle wherever
//! it sits. An id stored with other content, or one id named twice in the
//! request, refuses the whole request and accepts nothing.
//!
//! The batch promises order and contiguity, not grouping: how the drive
//! groups the block into roots follows the session's batching and ADR 0101's
//! selector, as for any queued inputs. The command lane still drains first at
//! every turn boundary, and inputs sharing the batch's spec may share a root.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use lash_core::runtime::{TurnInputAcceptanceReceipt, TurnInputIngress};
use lash_core::{RunSpec, TurnId};

use super::{HandleShared, SendHandle, SendTarget, refuse_live_turn_context};
use crate::error::Result;
use crate::support::TurnInput;

/// One input of a [`send_batch`](crate::LashSession::send_batch), with the
/// host id it is sent under.
///
/// The id is the input's idempotency key and names the root it starts, as
/// [`SendBuilder::id`](crate::SendBuilder::id) does for one send. An input
/// without one is sent under a fresh id, so only an input with an id is
/// retried by resending its batch.
#[derive(Debug)]
pub struct BatchInput {
    input: TurnInput,
    id: Option<TurnId>,
}

impl BatchInput {
    pub fn new(input: TurnInput) -> Self {
        Self { input, id: None }
    }

    /// The host id this input is sent under.
    pub fn id(mut self, id: impl Into<TurnId>) -> Self {
        self.id = Some(id.into());
        self
    }
}

impl From<TurnInput> for BatchInput {
    fn from(input: TurnInput) -> Self {
        Self::new(input)
    }
}

impl<K: Into<TurnId>> From<(K, TurnInput)> for BatchInput {
    fn from((id, input): (K, TurnInput)) -> Self {
        Self::new(input).id(id)
    }
}

/// Builder for one [`send_batch`](crate::LashSession::send_batch).
///
/// Awaiting it commits the whole acceptance and yields one [`SendHandle`] per
/// input, in request order.
#[must_use = "a SendBatchBuilder does nothing until awaited"]
pub struct SendBatchBuilder {
    target: SendTarget,
    inputs: Vec<BatchInput>,
    run_spec: RunSpec,
}

impl SendBatchBuilder {
    pub(crate) fn new(target: SendTarget, inputs: Vec<BatchInput>) -> Self {
        Self {
            target,
            inputs,
            run_spec: RunSpec::default(),
        }
    }

    /// The one spec every input of the batch runs under, as
    /// [`SendBuilder::run`](crate::SendBuilder::run) sets it for one send.
    /// It is part of every input's submission: a retry of the batch must
    /// carry the same spec.
    pub fn run(mut self, spec: RunSpec) -> Self {
        self.run_spec = spec;
        self
    }

    async fn accept(self) -> Result<Vec<SendHandle>> {
        let Self {
            target,
            inputs,
            run_spec,
        } = self;
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut submissions = Vec::with_capacity(inputs.len());
        for BatchInput { mut input, id } in inputs {
            refuse_live_turn_context(&input)?;
            // As for one send: the host id names the root, and an input sent
            // without one gets a fresh id.
            let id = id
                .or_else(|| input.trace_turn_id.take())
                .unwrap_or_else(crate::turn::fresh_turn_id);
            input.trace_turn_id = None;
            submissions.push((input, Some(id.to_string())));
        }
        let context = target.context().await?;
        let cursor = target.current_cursor();
        let enqueued = context
            .parts
            .ops
            .enqueue_turn_inputs(
                &context.parts.store,
                submissions,
                TurnInputIngress::NextTurn,
                run_spec,
            )
            .await?;
        Ok(enqueued
            .iter()
            .map(|row| SendHandle {
                target: target.clone(),
                receipt: TurnInputAcceptanceReceipt::from(row),
                id: row.source_key.as_deref().map(TurnId::from),
                cursor: cursor.clone(),
                shared: Arc::new(HandleShared::pending()),
            })
            .collect())
    }
}

impl std::future::IntoFuture for SendBatchBuilder {
    type Output = Result<Vec<SendHandle>>;
    type IntoFuture = BoxFuture<'static, Result<Vec<SendHandle>>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.accept())
    }
}
