//! The invocation that owns a logical Run serves calls from every cell.
//! Requests carry values; the Run and its running calls stay on the owner's
//! stack until it closes.

use std::sync::Arc;

use futures_util::future::Either;
mod mailbox;
use mailbox::{Reply, Sender, channel, reply};

use super::execution_context::RuntimeExecutionContext;
use super::runtime_ops::RuntimeExecutionContextRuntimeOps as _;
use crate::RuntimeEffectControllerError;
use crate::session::tool_execution::{
    ToolAggregateConsumer, ToolAggregateRequest, ToolRunAggregateCursor, ToolRunAggregatePoll,
};
use crate::tool_dispatch::{ProductionToolHandlers, SingletonRunError};
use crate::tool_run::RunCutRefusal;

#[derive(Clone)]
pub(super) struct ToolRunChannel(Sender<Request>);

/// A request channel to the invocation's stack-owned tool Run.
/// It carries no controller, Run or running call.
#[derive(Clone)]
pub struct ToolRunOwner(
    ToolRunChannel,
    crate::runtime::process::LanguageCallAttributions,
);

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
    Close(Reply<Result<(), SingletonRunError>>),
}

fn owner_gone() -> RuntimeEffectControllerError {
    SingletonRunError::from(RunCutRefusal::InvocationFailed).into_controller_error()
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
        self.tool_run
            .clone()
            .map(|channel| ToolRunOwner(channel, Arc::clone(&self.language_calls)))
    }

    pub(super) fn with_tool_run_owner(mut self, owner: &ToolRunOwner) -> Self {
        self.tool_run = Some(owner.0.clone());
        self.language_calls = Arc::clone(&owner.1);
        self
    }

    /// Drive a logical owner beside its program: its calls run in memory
    /// beside it, each to its own end. The program explicitly closes at
    /// logical termination. Dropping this frame on worker loss drops every
    /// unfinished call; the admitted execution that ran the program owns
    /// its recovery (ADR 0132 §5, §8).
    pub fn drive_tool_run<F, Fut>(
        &self,
        program: F,
    ) -> impl std::future::Future<Output = Result<Fut::Output, RuntimeEffectControllerError>>
    where
        F: FnOnce(Self) -> Fut,
        Fut: std::future::Future,
    {
        Box::pin(async move {
            let owner = self
                .logical_run()
                .map(|address| {
                    crate::EffectOpener::turn(address.session_id.clone(), address.turn_id.clone())
                })
                .or_else(|| crate::EffectOpener::for_scope(&self.admitted_scope()).ok())
                .ok_or_else(|| {
                    RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                        "a tool Run needs a logical owner: a turn or a process",
                    )
                })?;
            let state = self.opener_state();
            let scoped = self.dispatch.effect_controller.clone();
            let claim = super::execution_context::execution_claim_of(scoped.execution_scope())
                .map_err(RuntimeEffectControllerError::from)?;
            let environment = if let Some(environment) = self.inherited_process_execution_env_ref()
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
            let handlers = Arc::new(ProductionToolHandlers::new(self.clone(), environment));
            let mut run = state
                .open_run(
                    scoped.execution_scope().clone(),
                    Arc::clone(&self.dispatch.clock),
                )
                .map_err(SingletonRunError::into_controller_error)?;
            let (send, mut receive) = channel();
            let mut context = self.clone();
            context.tool_run = Some(ToolRunChannel(send));
            // Heap the program so nested child turns do not retain their
            // largest polling states in the owner's frame.
            let mut future = Box::pin(program(context));
            let mut closed = false;
            loop {
                // Calls progress beside the program and every request.
                let event = tokio::select! {
                    output = future.as_mut() => Either::Left(output),
                    request = receive.recv() => Either::Right(request),
                    () = run.next_end() => continue,
                };
                match event {
                    Either::Left(output) => break Ok(output),
                    Either::Right(Some(Request::Admit {
                        request,
                        parent,
                        environment,
                        attribution,
                        reply,
                    })) => {
                        let result = Box::pin(handlers.admit_aggregate(
                            &mut run,
                            &owner,
                            request,
                            parent.map(|parent| *parent),
                            *environment,
                            *attribution,
                        ))
                        .await;
                        let _ = reply.send(result);
                    }
                    Either::Right(Some(Request::Consume {
                        cursor,
                        consumer,
                        wait,
                        host_control,
                        reply,
                    })) => {
                        let result = Box::pin(handlers.consume_aggregate(
                            &mut run,
                            &owner,
                            cursor,
                            consumer,
                            wait,
                            host_control,
                        ))
                        .await;
                        let _ = reply.send(result);
                    }
                    Either::Right(Some(Request::Close(reply))) => {
                        let result = if closed {
                            Ok(())
                        } else {
                            Box::pin(run.close()).await
                        };
                        closed = result.is_ok();
                        if result.is_ok() {
                            state.finish_run();
                        }
                        let _ = reply.send(result);
                    }
                    Either::Right(None) => break Err(owner_gone()),
                }
            }
        })
    }
}
