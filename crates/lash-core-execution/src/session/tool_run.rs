//! The invocation that owns a logical Run serves calls from every cell.
//! Requests carry values; the borrowed coordinator and issued X bodies stay
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
        parent: Option<Box<crate::RuntimeInvocation>>,
        environment: Box<crate::ProcessExecutionEnvSpec>,
        attribution: Box<super::execution_context::ToolObservationAttribution>,
        reply: Reply<Result<ToolRunAggregateCursor, SingletonRunError>>,
    },
    Consume {
        cursor: ToolRunAggregateCursor,
        consumer: ToolAggregateConsumer,
        wait: bool,
        host_control: bool,
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
        environment: crate::ProcessExecutionEnvSpec,
        attribution: super::execution_context::ToolObservationAttribution,
    ) -> Result<ToolRunAggregateCursor, RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Admit {
                request,
                parent: parent.map(Box::new),
                environment: Box::new(environment),
                attribution: Box::new(attribution),
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
        host_control: bool,
    ) -> Result<ToolRunAggregatePoll, RuntimeEffectControllerError> {
        let (reply, receive) = reply();
        self.0
            .send(Request::Consume {
                cursor,
                consumer,
                wait,
                host_control,
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
    pub(super) fn tool_run_owner(&self) -> Option<ToolRunOwner> {
        self.tool_run.clone().map(ToolRunOwner)
    }

    pub(super) fn with_tool_run_owner(mut self, owner: &ToolRunOwner) -> Self {
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
    pub fn drive_tool_run<F, Fut>(
        &self,
        materials: Option<Arc<dyn crate::store::ToolMaterialStore>>,
        program: F,
    ) -> impl std::future::Future<Output = Result<Fut::Output, RuntimeEffectControllerError>>
    where
        F: FnOnce(Self) -> Fut,
        Fut: std::future::Future,
    {
        Box::pin(async move {
            let materials = materials.or_else(|| self.tool_material_store());
            let owner = self
                .logical_run()
                .map(|address| {
                    crate::EffectOpener::turn(address.session_id.clone(), address.turn_id.clone())
                })
                .or_else(|| crate::EffectOpener::for_scope(&self.admitted_scope()).ok())
                .ok_or_else(|| {
                    RuntimeEffectControllerError::from(ContinuationRefusal::ForeignOwner)
                })?;
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
                .or_else(|| self.inherited_process_execution_env_ref())
            {
                self.process_env_store
                    .acquire_process_execution_env(&claim, &environment)
                    .await
                    .map_err(crate::PluginError::from)
                    .map_err(RuntimeEffectControllerError::from)?;
                Some(environment)
            } else {
                None
            };
            let handlers = Arc::new(ProductionToolHandlers::new(
                self.clone(),
                materials.clone(),
                environment.clone(),
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
                .map_err(SingletonRunError::into_controller_error)?
                .with_admitted_environment(environment);
            let bodies = run.bodies();
            let (send, mut receive) = channel();
            let mut context = self.clone();
            context.tool_run = Some(ToolRunChannel(send));
            let future = program(context);
            tokio::pin!(future);
            let mut closed = false;
            bodies
                .beside(async {
                    loop {
                        // Bodies of issued X progress beside the program and
                        // every request; their results are awaited only inside
                        // coordinator frames, so replay registers its command
                        // prefix before awaiting any unfinished X.
                        let event = select(future.as_mut(), Box::pin(receive.recv())).await;
                        match event {
                            Either::Left((output, _)) => break Ok(output),
                            Either::Right((
                                Some(Request::Admit {
                                    request,
                                    parent,
                                    environment,
                                    attribution,
                                    reply,
                                }),
                                _,
                            )) => {
                                let result = handlers
                                    .admit_aggregate(
                                        &mut run,
                                        request,
                                        parent.map(|parent| *parent),
                                        *environment,
                                        *attribution,
                                    )
                                    .await;
                                let _ = reply.send(result);
                            }
                            Either::Right((
                                Some(Request::Consume {
                                    cursor,
                                    consumer,
                                    wait,
                                    host_control,
                                    reply,
                                }),
                                _,
                            )) => {
                                let result = handlers
                                    .consume_aggregate(
                                        &mut run,
                                        cursor,
                                        consumer,
                                        wait,
                                        host_control,
                                    )
                                    .await;
                                let _ = reply.send(result);
                            }
                            Either::Right((Some(Request::Capture { reason, reply }), _)) => {
                                let result = match materials.as_deref() {
                                    Some(materials) => {
                                        state.capture_run(&mut run, reason, materials).await
                                    }
                                    None => Err(ContinuationRefusal::UnretainedMaterial.into()),
                                };
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
                            Either::Right((None, _)) => break Err(owner_gone()),
                        }
                    }
                })
                .await
        })
    }
}
