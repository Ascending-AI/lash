//! The invocation that owns a logical Run serves calls from every cell.
//! Requests carry values; the borrowed coordinator and issued X handles stay
//! on the owner's stack until Closing or a retained physical cut.

use std::sync::Arc;

use futures_util::future::{Either, select};
mod mailbox;
use mailbox::{Reply, Sender, channel, reply};

use super::execution_context::RuntimeExecutionContext;
use super::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use crate::session::tool_execution::{
    ToolAggregateConsumer, ToolAggregateRequest, ToolRunAggregateCursor, ToolRunAggregatePoll,
};
use crate::tool_dispatch::{ProductionToolHandlers, SingletonRunError};
use crate::tool_run::{ContinuationRefusal, SegmentOrdinal};
use crate::{BoundaryReason, RuntimeEffectControllerError};

#[derive(Clone)]
pub(super) struct ToolRunChannel(Sender<Request>);

/// A request channel to the invocation's stack-owned tool Run.
/// It carries no controller, coordinator or issued attempt future.
#[derive(Clone)]
pub struct ToolRunOwner(ToolRunChannel);

impl ToolRunOwner {
    pub async fn capture_tool_run(
        &self,
        reason: BoundaryReason,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.0.capture(reason).await
    }
}

enum Request {
    Admit {
        request: ToolAggregateRequest,
        parent: Option<crate::RuntimeInvocation>,
        reply: Reply<Result<ToolRunAggregateCursor, SingletonRunError>>,
    },
    Consume {
        cursor: ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
        wait: bool,
        reply: Reply<Result<ToolRunAggregatePoll, SingletonRunError>>,
    },
    Capture {
        reason: BoundaryReason,
        reply: Reply<Result<(), SingletonRunError>>,
    },
    Close(Reply<Result<(), SingletonRunError>>),
}

fn owner_gone() -> RuntimeEffectControllerError {
    ContinuationRefusal::NotQuiescent.into()
}

impl ToolRunChannel {
    pub(super) async fn admit(
        &self,
        request: ToolAggregateRequest,
        parent: Option<crate::RuntimeInvocation>,
    ) -> Result<ToolRunAggregateCursor, RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Admit {
                request,
                parent,
                reply,
            })
            .map_err(|_| owner_gone())?;
        receive
            .await
            .map_err(|_| owner_gone())?
            .map_err(SingletonRunError::into_controller_error)
    }

    pub(super) async fn consume(
        &self,
        cursor: ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
        wait: bool,
    ) -> Result<ToolRunAggregatePoll, RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Consume {
                cursor,
                consumer,
                wait,
                reply,
            })
            .map_err(|_| owner_gone())?;
        receive
            .await
            .map_err(|_| owner_gone())?
            .map_err(SingletonRunError::into_controller_error)
    }

    async fn capture(&self, reason: BoundaryReason) -> Result<(), RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Capture { reason, reply })
            .map_err(|_| owner_gone())?;
        receive
            .await
            .map_err(|_| owner_gone())?
            .map_err(SingletonRunError::into_controller_error)
    }

    pub(super) async fn close(&self) -> Result<(), RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Close(reply))
            .map_err(|_| owner_gone())?;
        receive
            .await
            .map_err(|_| owner_gone())?
            .map_err(SingletonRunError::into_controller_error)
    }
}

impl<'run> RuntimeExecutionContext<'run> {
    /// Whether an enclosing invocation already owns this context's Run.
    pub fn has_tool_run_owner(&self) -> bool {
        self.tool_run.is_some()
    }

    /// Share the enclosing invocation's request channel with a phase context.
    pub fn tool_run_owner(&self) -> Option<ToolRunOwner> {
        self.tool_run.clone().map(ToolRunOwner)
    }

    pub fn with_tool_run_owner(mut self, owner: &ToolRunOwner) -> Self {
        self.tool_run = Some(owner.0.clone());
        self
    }

    /// Quiesce and retain all cells' issued work before publishing VM state.
    pub async fn capture_tool_run(
        &self,
        reason: BoundaryReason,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.tool_run
            .as_ref()
            .ok_or_else(owner_gone)?
            .capture(reason)
            .await
    }

    /// Drive a logical owner beside its program. The program explicitly closes
    /// at logical termination or captures at handover. Dropping this frame on
    /// worker loss leaves unfinished journal work to engine recovery.
    pub async fn drive_tool_run<F, Fut>(
        &self,
        materials: Option<Arc<dyn crate::store::ToolMaterialStore>>,
        program: F,
    ) -> Result<Fut::Output, RuntimeEffectControllerError>
    where
        F: FnOnce(Self) -> Fut,
        Fut: std::future::Future,
    {
        let materials = materials.or_else(|| self.tool_material_store());
        let owner = self
            .logical_run()
            .map(|address| {
                crate::EffectOpener::turn(address.session_id.clone(), address.turn_id.clone())
            })
            .or_else(|| crate::runtime::effect::opener_for_execution_scope(&self.admitted_scope()))
            .ok_or_else(|| RuntimeEffectControllerError::from(ContinuationRefusal::ForeignOwner))?;
        let state = self.opener_state();
        let segment = self
            .process_event_context()
            .and_then(|context| context.execution_write_authority.segment())
            .or_else(|| {
                state
                    .snapshot()
                    .run
                    .as_ref()
                    .map(|transfer| SegmentOrdinal(transfer.from.0 + 1))
            })
            .unwrap_or(SegmentOrdinal(0));
        let scoped = self.dispatch.effect_controller.clone();
        let available = self.dispatch.plugins.tool_run_revisions();
        let claim = super::execution_context::execution_claim_of(scoped.execution_scope())
            .map_err(RuntimeEffectControllerError::from)?;
        let environment = if let Some(environment) = state
            .snapshot()
            .run
            .as_ref()
            .and_then(|transfer| transfer.environment.clone())
        {
            self.process_env_store
                .acquire_process_execution_env(&claim, &environment)
                .await
                .map_err(crate::PluginError::from)
                .map_err(RuntimeEffectControllerError::from)?;
            environment
        } else {
            self.captured_process_execution_env_ref(&claim)
                .await
                .map_err(RuntimeEffectControllerError::from)?
        };
        let handlers = Arc::new(ProductionToolHandlers::new(
            self.clone(),
            materials.clone(),
            Some(environment),
        ));
        let mut run = state
            .adopt_run(
                &scoped,
                owner,
                segment,
                available,
                handlers.clone(),
                self.dispatch.clock.as_ref(),
            )
            .await
            .map_err(SingletonRunError::into_controller_error)?;
        let (send, mut receive) = channel();
        let mut context = self.clone();
        context.tool_run = Some(ToolRunChannel(send));
        let future = program(context);
        tokio::pin!(future);
        let mut cut = false;
        let mut closed = false;
        loop {
            let event = if cut {
                select(future.as_mut(), Box::pin(receive.recv())).await
            } else {
                run.beside(select(future.as_mut(), Box::pin(receive.recv())))
                    .await
                    .map_err(SingletonRunError::into_controller_error)?
            };
            match event {
                Either::Left((output, _)) => return Ok(output),
                Either::Right((
                    Some(Request::Admit {
                        request,
                        parent,
                        reply,
                    }),
                    _,
                )) => {
                    let result = handlers.admit_aggregate(&mut run, request, parent).await;
                    let _ = reply.send(result);
                }
                Either::Right((
                    Some(Request::Consume {
                        cursor,
                        consumer,
                        wait,
                        reply,
                    }),
                    _,
                )) => {
                    let result = handlers
                        .consume_aggregate(&mut run, cursor, consumer, wait)
                        .await;
                    let _ = reply.send(result);
                }
                Either::Right((Some(Request::Capture { reason, reply }), _)) => {
                    let result = match materials.as_deref() {
                        Some(materials) => state.capture_run(&mut run, reason, materials).await,
                        None => Err(ContinuationRefusal::UnretainedMaterial.into()),
                    };
                    cut = result.is_ok();
                    let _ = reply.send(result);
                }
                Either::Right((Some(Request::Close(reply)), _)) => {
                    let result = if closed { Ok(()) } else { run.close().await };
                    closed = result.is_ok();
                    if result.is_ok() {
                        state.finish_run();
                    }
                    let _ = reply.send(result);
                }
                Either::Right((None, _)) => return Err(owner_gone()),
            }
        }
    }
}
