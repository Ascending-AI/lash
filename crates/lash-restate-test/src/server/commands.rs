//! Commands an attempt writes: the invoker's pre-validation and every
//! server-side reaction to a stored command.

use std::sync::Arc;

use bytes::Bytes;

use super::Shared;
use super::catalog::ServiceKind;
use super::ids::decode_awakeable_id;
use super::model::{InvKey, Outcome, PromiseState, Status, Target, TimerAction, Waiter};
use crate::protocol::generated::{
    self as pb, AttachInvocationCommandMessage, CallCommandMessage, ClearStateCommandMessage,
    CompleteAwakeableCommandMessage, CompletePromiseCommandMessage,
    GetInvocationOutputCommandMessage, GetLazyStateCommandMessage, GetPromiseCommandMessage,
    OneWayCallCommandMessage, OutputCommandMessage, PeekPromiseCommandMessage,
    SendSignalCommandMessage, SetStateCommandMessage, SleepCommandMessage, notification_template,
};
use crate::protocol::{CANCEL_SIGNAL_ID, Frame, MessageType};

use super::processor::*;

impl State {
    /// The invoker's command pre-validation: call targets resolve, promise
    /// commands run in a workflow, state reads have a key and state writes
    /// hold the key's lock.
    pub(super) fn precondition(
        &self,
        sh: &Arc<Shared>,
        key: InvKey,
        frame: &Frame,
    ) -> Result<(), String> {
        let invocation = &self.invocations[key.0];
        let command_index = invocation
            .journal
            .iter()
            .filter(|entry| entry.frame.ty.is_command())
            .count();
        let failed = |command: &str, reason: String| {
            format!(
                "cannot process command {command} (command index {command_index}) because of a failed precondition: {reason}"
            )
        };
        let resolve = |service: &str, handler: &str, key: &str, command: &str| match sh
            .catalog()
            .resolve(service, handler)
        {
            Ok(spec) if !spec.kind.is_keyed() && !key.is_empty() => Err(failed(
                command,
                format!("the service {service} is not keyed but a key was given"),
            )),
            Ok(_) => Ok(()),
            Err(_) => Err(failed(
                command,
                format!("the service handler {service}/{handler} was not found"),
            )),
        };
        match frame.ty {
            MessageType::CallCommand => {
                let call = frame
                    .decode::<CallCommandMessage>()
                    .map_err(|error| error.to_string())?;
                resolve(&call.service_name, &call.handler_name, &call.key, "Call")
            }
            MessageType::OneWayCallCommand => {
                let send = frame
                    .decode::<OneWayCallCommandMessage>()
                    .map_err(|error| error.to_string())?;
                resolve(
                    &send.service_name,
                    &send.handler_name,
                    &send.key,
                    "OneWayCall",
                )
            }
            MessageType::GetPromiseCommand
            | MessageType::PeekPromiseCommand
            | MessageType::CompletePromiseCommand => {
                if invocation.spec.service_kind == ServiceKind::Workflow {
                    Ok(())
                } else {
                    Err(failed(
                        "promise",
                        format!("{} is not a workflow", invocation.target.service),
                    ))
                }
            }
            MessageType::GetLazyStateCommand | MessageType::GetLazyStateKeysCommand => {
                if invocation.spec.kind.is_keyed() {
                    Ok(())
                } else {
                    Err(failed("GetState", "the service has no state".into()))
                }
            }
            MessageType::SetStateCommand
            | MessageType::ClearStateCommand
            | MessageType::ClearAllStateCommand => {
                if invocation.spec.kind.takes_lock() {
                    Ok(())
                } else {
                    Err(failed(
                        "SetState",
                        "the handler is not allowed to write state".into(),
                    ))
                }
            }
            _ => Ok(()),
        }?;
        decodes(frame)
    }

    pub(super) fn apply_command(
        &mut self,
        sh: &Arc<Shared>,
        key: InvKey,
        frame: &Frame,
    ) -> Result<(), String> {
        let error = |error: crate::protocol::FrameError| error.to_string();
        match frame.ty {
            MessageType::OutputCommand => {
                let output = frame.decode::<OutputCommandMessage>().map_err(error)?;
                self.invocations[key.0].output = Some(match output.result {
                    Some(pb::output_command_message::Result::Value(value)) => {
                        Outcome::Success(value.content)
                    }
                    Some(pb::output_command_message::Result::Failure(failure)) => {
                        Outcome::Failure(failure)
                    }
                    None => Outcome::Success(Bytes::new()),
                });
            }
            MessageType::GetLazyStateCommand => {
                let get = frame
                    .decode::<GetLazyStateCommandMessage>()
                    .map_err(error)?;
                let name = String::from_utf8_lossy(&get.key).into_owned();
                let value = self.invocations[key.0]
                    .target
                    .service_key()
                    .and_then(|service_key| self.keys.get(&service_key))
                    .and_then(|record| record.state.get(&name).cloned());
                self.notify(
                    sh,
                    key,
                    MessageType::GetLazyStateCompletionNotification,
                    notification_template::Id::CompletionId(get.result_completion_id),
                    match value {
                        Some(content) => {
                            notification_template::Result::Value(pb::Value { content })
                        }
                        None => notification_template::Result::Void(pb::Void {}),
                    },
                );
            }
            MessageType::GetLazyStateKeysCommand => {
                let get = frame
                    .decode::<pb::GetLazyStateKeysCommandMessage>()
                    .map_err(error)?;
                let keys = self.invocations[key.0]
                    .target
                    .service_key()
                    .and_then(|service_key| self.keys.get(&service_key))
                    .map(|record| {
                        record
                            .state
                            .keys()
                            .map(|name| Bytes::copy_from_slice(name.as_bytes()))
                            .collect()
                    })
                    .unwrap_or_default();
                self.notify(
                    sh,
                    key,
                    MessageType::GetLazyStateKeysCompletionNotification,
                    notification_template::Id::CompletionId(get.result_completion_id),
                    notification_template::Result::StateKeys(pb::StateKeys { keys }),
                );
            }
            MessageType::SetStateCommand => {
                let set = frame.decode::<SetStateCommandMessage>().map_err(error)?;
                if let Some(service_key) = self.writable_key(key) {
                    let name = String::from_utf8_lossy(&set.key).into_owned();
                    let value = set.value.map(|value| value.content).unwrap_or_default();
                    self.key_record(service_key).state.insert(name, value);
                }
            }
            MessageType::ClearStateCommand => {
                let clear = frame.decode::<ClearStateCommandMessage>().map_err(error)?;
                if let Some(service_key) = self.writable_key(key) {
                    let name = String::from_utf8_lossy(&clear.key).into_owned();
                    self.key_record(service_key).state.remove(&name);
                }
            }
            MessageType::ClearAllStateCommand => {
                if let Some(service_key) = self.writable_key(key) {
                    self.key_record(service_key).state.clear();
                }
            }
            MessageType::GetPromiseCommand => {
                let get = frame.decode::<GetPromiseCommandMessage>().map_err(error)?;
                let service_key = self.workflow_key(key)?;
                let record = self.key_record(service_key);
                match record.promises.get_mut(&get.key) {
                    Some(PromiseState::Completed(outcome)) => {
                        let result = Self::outcome_result(outcome);
                        self.notify(
                            sh,
                            key,
                            MessageType::GetPromiseCompletionNotification,
                            notification_template::Id::CompletionId(get.result_completion_id),
                            result,
                        );
                    }
                    Some(PromiseState::Pending(waiters)) => {
                        waiters.push((key, get.result_completion_id));
                    }
                    None => {
                        record.promises.insert(
                            get.key,
                            PromiseState::Pending(vec![(key, get.result_completion_id)]),
                        );
                    }
                }
            }
            MessageType::PeekPromiseCommand => {
                let peek = frame.decode::<PeekPromiseCommandMessage>().map_err(error)?;
                let service_key = self.workflow_key(key)?;
                let result = match self.key_record(service_key).promises.get(&peek.key) {
                    Some(PromiseState::Completed(outcome)) => Self::outcome_result(outcome),
                    _ => notification_template::Result::Void(pb::Void {}),
                };
                self.notify(
                    sh,
                    key,
                    MessageType::PeekPromiseCompletionNotification,
                    notification_template::Id::CompletionId(peek.result_completion_id),
                    result,
                );
            }
            MessageType::CompletePromiseCommand => {
                let complete = frame
                    .decode::<CompletePromiseCommandMessage>()
                    .map_err(error)?;
                let service_key = self.workflow_key(key)?;
                let outcome = match complete.completion {
                    Some(pb::complete_promise_command_message::Completion::CompletionValue(
                        value,
                    )) => Outcome::Success(value.content),
                    Some(pb::complete_promise_command_message::Completion::CompletionFailure(
                        failure,
                    )) => Outcome::Failure(failure),
                    None => return Err("a promise completion carried no value".into()),
                };
                let reply = self.complete_promise(sh, service_key, &complete.key, outcome);
                self.notify(
                    sh,
                    key,
                    MessageType::CompletePromiseCompletionNotification,
                    notification_template::Id::CompletionId(complete.result_completion_id),
                    reply,
                );
            }
            MessageType::SleepCommand => {
                let sleep = frame.decode::<SleepCommandMessage>().map_err(error)?;
                let fire_at = self
                    .now_ms
                    .saturating_add(wall_delay_ms(sleep.wake_up_time, self.frame_received_us));
                self.add_timer(
                    fire_at,
                    TimerAction::Sleep {
                        invocation: key,
                        completion_id: sleep.result_completion_id,
                    },
                );
            }
            MessageType::CallCommand => {
                let call = frame.decode::<CallCommandMessage>().map_err(error)?;
                let submission = Submission {
                    target: Target {
                        service: call.service_name.clone(),
                        handler: call.handler_name.clone(),
                        key: Some(call.key.clone()),
                    },
                    input: call.parameter.clone(),
                    headers: call.headers.clone(),
                    idempotency_key: call.idempotency_key.clone(),
                    start_at_ms: None,
                    parent: Some(key),
                };
                let (callee, submitted) = self
                    .submit(sh, submission)
                    .map_err(|refusal| refusal.message())?;
                self.invocations[key.0].children.push(callee);
                let printed = self.invocations[callee.0].id.as_str().to_owned();
                self.notify(
                    sh,
                    key,
                    MessageType::CallInvocationIdCompletionNotification,
                    notification_template::Id::CompletionId(call.invocation_id_notification_idx),
                    notification_template::Result::InvocationId(printed),
                );
                if submitted == Submitted::WorkflowRunExists {
                    let (code, message) = WORKFLOW_ALREADY_INVOKED;
                    self.notify(
                        sh,
                        key,
                        MessageType::CallCompletionNotification,
                        notification_template::Id::CompletionId(call.result_completion_id),
                        Self::fail_notification(code, message),
                    );
                } else {
                    self.add_waiter(
                        sh,
                        callee,
                        Waiter::Call {
                            caller: key,
                            completion_id: call.result_completion_id,
                        },
                    );
                }
            }
            MessageType::OneWayCallCommand => {
                let send = frame.decode::<OneWayCallCommandMessage>().map_err(error)?;
                let start_at_ms = (send.invoke_time > 0).then(|| {
                    self.now_ms
                        .saturating_add(wall_delay_ms(send.invoke_time, self.frame_received_us))
                });
                let submission = Submission {
                    target: Target {
                        service: send.service_name.clone(),
                        handler: send.handler_name.clone(),
                        key: Some(send.key.clone()),
                    },
                    input: send.parameter.clone(),
                    headers: send.headers.clone(),
                    idempotency_key: send.idempotency_key.clone(),
                    start_at_ms,
                    parent: Some(key),
                };
                let (callee, _) = self
                    .submit(sh, submission)
                    .map_err(|refusal| refusal.message())?;
                self.invocations[key.0].children.push(callee);
                let printed = self.invocations[callee.0].id.as_str().to_owned();
                self.notify(
                    sh,
                    key,
                    MessageType::CallInvocationIdCompletionNotification,
                    notification_template::Id::CompletionId(send.invocation_id_notification_idx),
                    notification_template::Result::InvocationId(printed),
                );
            }
            MessageType::SendSignalCommand => {
                let signal = frame.decode::<SendSignalCommandMessage>().map_err(error)?;
                let Some(target) = self.lookup(&signal.target_invocation_id) else {
                    return Ok(());
                };
                let result = match signal.result {
                    Some(pb::send_signal_command_message::Result::Void(void)) => {
                        notification_template::Result::Void(void)
                    }
                    Some(pb::send_signal_command_message::Result::Value(value)) => {
                        notification_template::Result::Value(value)
                    }
                    Some(pb::send_signal_command_message::Result::Failure(failure)) => {
                        notification_template::Result::Failure(failure)
                    }
                    None => notification_template::Result::Void(pb::Void {}),
                };
                match signal.signal_id {
                    Some(pb::send_signal_command_message::SignalId::Idx(CANCEL_SIGNAL_ID)) => {
                        self.cancel(sh, target);
                    }
                    Some(pb::send_signal_command_message::SignalId::Idx(index)) => {
                        self.notify(
                            sh,
                            target,
                            MessageType::SignalNotification,
                            notification_template::Id::SignalId(index),
                            result,
                        );
                    }
                    Some(pb::send_signal_command_message::SignalId::Name(name)) => {
                        self.notify(
                            sh,
                            target,
                            MessageType::SignalNotification,
                            notification_template::Id::SignalName(name),
                            result,
                        );
                    }
                    None => {}
                }
            }
            MessageType::AttachInvocationCommand => {
                let attach = frame
                    .decode::<AttachInvocationCommandMessage>()
                    .map_err(error)?;
                match self.resolve_target(attach.target.map(AttachTarget::from)) {
                    Some(target) => self.add_waiter(
                        sh,
                        target,
                        Waiter::Attach {
                            caller: key,
                            completion_id: attach.result_completion_id,
                        },
                    ),
                    None => self.notify(
                        sh,
                        key,
                        MessageType::AttachInvocationCompletionNotification,
                        notification_template::Id::CompletionId(attach.result_completion_id),
                        Self::fail_notification(404, "invocation not found"),
                    ),
                }
            }
            MessageType::GetInvocationOutputCommand => {
                let get = frame
                    .decode::<GetInvocationOutputCommandMessage>()
                    .map_err(error)?;
                let result = match self
                    .resolve_target(get.target.map(AttachTarget::from))
                    .map(|target| &self.invocations[target.0].status)
                {
                    Some(Status::Completed(outcome)) => Self::outcome_result(outcome),
                    Some(_) => notification_template::Result::Void(pb::Void {}),
                    None => Self::fail_notification(404, "invocation not found"),
                };
                self.notify(
                    sh,
                    key,
                    MessageType::GetInvocationOutputCompletionNotification,
                    notification_template::Id::CompletionId(get.result_completion_id),
                    result,
                );
            }
            MessageType::CompleteAwakeableCommand => {
                let complete = frame
                    .decode::<CompleteAwakeableCommandMessage>()
                    .map_err(error)?;
                let result = match complete.result {
                    Some(pb::complete_awakeable_command_message::Result::Value(value)) => {
                        notification_template::Result::Value(value)
                    }
                    Some(pb::complete_awakeable_command_message::Result::Failure(failure)) => {
                        notification_template::Result::Failure(failure)
                    }
                    None => return Err("an awakeable completion carried no value".into()),
                };
                self.complete_awakeable(sh, &complete.awakeable_id, result);
            }
            // Run, eager-state reads, input and custom commands are journaled
            // and replayed; the server does nothing else with them.
            _ => {}
        }
        Ok(())
    }

    /// The `(service, key)` whose state `key` may write: an exclusive
    /// object handler's or a workflow run's. Shared handlers write nothing.
    pub(super) fn writable_key(&self, key: InvKey) -> Option<(String, String)> {
        let invocation = &self.invocations[key.0];
        invocation
            .spec
            .kind
            .takes_lock()
            .then(|| invocation.target.service_key())
            .flatten()
    }

    /// The workflow key a promise command addresses (its precondition
    /// already checked the handler is a workflow's).
    pub(super) fn workflow_key(&self, key: InvKey) -> Result<(String, String), String> {
        self.invocations[key.0]
            .target
            .service_key()
            .ok_or_else(|| "a promise command outside a keyed handler".to_owned())
    }

    /// Complete a workflow promise, first writer wins. Returns the
    /// `CompletePromise` reply.
    pub(super) fn complete_promise(
        &mut self,
        sh: &Arc<Shared>,
        service_key: (String, String),
        name: &str,
        outcome: Outcome,
    ) -> notification_template::Result {
        let record = self.key_record(service_key);
        let waiters = match record.promises.get_mut(name) {
            Some(PromiseState::Completed(_)) => {
                return Self::fail_notification(409, "promise was already completed");
            }
            Some(PromiseState::Pending(waiters)) => std::mem::take(waiters),
            None => Vec::new(),
        };
        record
            .promises
            .insert(name.to_owned(), PromiseState::Completed(outcome.clone()));
        let result = Self::outcome_result(&outcome);
        for (waiter, completion_id) in waiters {
            self.notify(
                sh,
                waiter,
                MessageType::GetPromiseCompletionNotification,
                notification_template::Id::CompletionId(completion_id),
                result.clone(),
            );
        }
        notification_template::Result::Void(pb::Void {})
    }

    /// Deliver an awakeable completion to the signal its id names. Returns
    /// whether the owning invocation exists.
    pub fn complete_awakeable(
        &mut self,
        sh: &Arc<Shared>,
        awakeable_id: &str,
        result: notification_template::Result,
    ) -> bool {
        let Some(target) = decode_awakeable_id(awakeable_id) else {
            return false;
        };
        let Some(key) = self.lookup(target.invocation.as_str()) else {
            return false;
        };
        self.notify(
            sh,
            key,
            MessageType::SignalNotification,
            notification_template::Id::SignalId(target.signal_id),
            result,
        );
        true
    }

    pub fn resolve_target(&self, target: Option<AttachTarget>) -> Option<InvKey> {
        match target? {
            AttachTarget::Invocation(printed) => self.lookup(&printed),
            AttachTarget::Workflow { name, key } => self
                .keys
                .get(&(name, key))
                .and_then(|record| record.workflow_run),
            AttachTarget::Idempotent {
                service,
                key,
                handler,
                idempotency_key,
            } => self
                .idempotency
                .get(&(service, key, handler, idempotency_key))
                .copied(),
        }
    }
}

/// Whether `frame` decodes as the command its type names, with the parts
/// the server acts on present. A command that does not is refused before it
/// is journaled: stored, it would fail every replay the same way.
fn decodes(frame: &Frame) -> Result<(), String> {
    fn check<M: prost::Message + Default>(frame: &Frame) -> Result<M, String> {
        frame.decode::<M>().map_err(|error| error.to_string())
    }
    match frame.ty {
        MessageType::OutputCommand => check::<OutputCommandMessage>(frame).map(drop),
        MessageType::GetLazyStateCommand => check::<GetLazyStateCommandMessage>(frame).map(drop),
        MessageType::GetLazyStateKeysCommand => {
            check::<pb::GetLazyStateKeysCommandMessage>(frame).map(drop)
        }
        MessageType::SetStateCommand => check::<SetStateCommandMessage>(frame).map(drop),
        MessageType::ClearStateCommand => check::<ClearStateCommandMessage>(frame).map(drop),
        MessageType::GetPromiseCommand => check::<GetPromiseCommandMessage>(frame).map(drop),
        MessageType::PeekPromiseCommand => check::<PeekPromiseCommandMessage>(frame).map(drop),
        MessageType::CompletePromiseCommand => {
            match check::<CompletePromiseCommandMessage>(frame)?.completion {
                Some(_) => Ok(()),
                None => Err("a promise completion carried no value".into()),
            }
        }
        MessageType::SleepCommand => check::<SleepCommandMessage>(frame).map(drop),
        MessageType::SendSignalCommand => check::<SendSignalCommandMessage>(frame).map(drop),
        MessageType::AttachInvocationCommand => {
            check::<AttachInvocationCommandMessage>(frame).map(drop)
        }
        MessageType::GetInvocationOutputCommand => {
            check::<GetInvocationOutputCommandMessage>(frame).map(drop)
        }
        MessageType::CompleteAwakeableCommand => {
            match check::<CompleteAwakeableCommandMessage>(frame)?.result {
                Some(_) => Ok(()),
                None => Err("an awakeable completion carried no value".into()),
            }
        }
        _ => Ok(()),
    }
}
