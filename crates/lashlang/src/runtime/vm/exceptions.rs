use std::sync::Arc;

use crate::lexer::Span;

use super::super::{
    ErrorKind, ExecutionHost, Instruction, RuntimeError, Value, record_with_capacity,
};
use super::Vm;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ExceptionHandler {
    pub(super) handler_ip: usize,
    pub(super) finally_ip: Option<usize>,
    pub(super) catches: bool,
    pub(super) frame_depth: usize,
    pub(super) frame_function: Option<usize>,
    pub(super) stack_depth: usize,
    pub(super) iterator_depth: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct FinallyState {
    pub(super) completion: FinallyCompletion,
    pub(super) handler_depth: usize,
    pub(super) frame_depth: usize,
    pub(super) frame_function: Option<usize>,
    pub(super) stack_depth: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum FinallyCompletion {
    Normal {
        resume_ip: usize,
    },
    Throw {
        value: Value,
        /// The runtime failure this throw was raised from, if any. A value
        /// thrown by an explicit `throw` has none; a routed `RuntimeError`
        /// carries itself here so that a cleanup chain which ends without a
        /// catch re-raises the original error instead of an exception record.
        origin: Option<Box<PendingErrorOrigin>>,
    },
}

/// The typed failure a pending throw was raised from, with the attribution the
/// trap needs if the unwind ends with nothing catching it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingErrorOrigin {
    pub(super) error: RuntimeError,
    pub(super) instruction_ip: usize,
    pub(super) span: Option<Span>,
}

/// What a `finally` escaped with when no handler took the pending throw.
pub(super) struct FinallyEscape {
    pub(super) value: Value,
    pub(super) origin: Option<Box<PendingErrorOrigin>>,
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(super) fn push_exception_handler(
        &mut self,
        handler_ip: usize,
        finally_ip: Option<usize>,
        catches: bool,
    ) {
        self.handlers.push(ExceptionHandler {
            handler_ip,
            finally_ip,
            catches,
            frame_depth: self.frames.len(),
            frame_function: self.active_function,
            stack_depth: self.stack.len(),
            iterator_depth: self.iter_stack.len(),
        });
    }

    pub(super) fn pop_exception_handler(&mut self) -> Result<(), RuntimeError> {
        let Some(handler) = self.handlers.pop() else {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler stack underflow".into(),
            });
        };
        if handler.frame_depth != self.frames.len()
            || handler.frame_function != self.active_function
        {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler was popped from a different frame".into(),
            });
        }
        Ok(())
    }

    pub(super) fn enter_finally(&mut self, finally_ip: usize, resume_ip: usize) {
        self.finally_stack.push(FinallyState {
            completion: FinallyCompletion::Normal { resume_ip },
            handler_depth: self.handlers.len(),
            frame_depth: self.frames.len(),
            frame_function: self.active_function,
            stack_depth: self.stack.len(),
        });
        self.ip = finally_ip;
    }

    /// Leaves a running `finally` body by an abrupt completion. The pending
    /// completion the body would otherwise have resumed or rethrown is
    /// replaced by the jump that is leaving, so it is dropped here.
    pub(super) fn abandon_finally(&mut self) -> Result<(), RuntimeError> {
        let Some(finally) = self.finally_stack.pop() else {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "finally stack underflow".into(),
            });
        };
        if finally.frame_depth != self.frames.len()
            || finally.frame_function != self.active_function
        {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "finally was abandoned in a different frame".into(),
            });
        }
        self.stack.truncate(finally.stack_depth);
        Ok(())
    }

    pub(super) fn abandon_finally_keep_value(&mut self) -> Result<(), RuntimeError> {
        let value = self.pop_stack()?;
        self.abandon_finally()?;
        self.stack.push(value);
        Ok(())
    }

    pub(super) fn finish_finally(&mut self) -> Result<Option<FinallyEscape>, RuntimeError> {
        let Some(finally) = self.finally_stack.pop() else {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "finally stack underflow".into(),
            });
        };
        if finally.frame_depth != self.frames.len()
            || finally.frame_function != self.active_function
        {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "finally completed in a different frame".into(),
            });
        }
        self.stack.truncate(finally.stack_depth);
        match finally.completion {
            FinallyCompletion::Normal { resume_ip } => {
                self.ip = resume_ip;
                Ok(None)
            }
            FinallyCompletion::Throw { value, origin } => {
                if self.throw_value(value.clone(), origin.clone())? {
                    Ok(None)
                } else {
                    Ok(Some(FinallyEscape { value, origin }))
                }
            }
        }
    }

    pub(super) fn has_exception_scope(&self) -> bool {
        !self.handlers.is_empty() || !self.finally_stack.is_empty()
    }

    pub(super) fn throw_runtime_error(
        &mut self,
        error: &RuntimeError,
        instruction_ip: usize,
        span: Option<Span>,
    ) -> Result<bool, RuntimeError> {
        let value = self.runtime_error_value(error, instruction_ip)?;
        self.throw_value(
            value,
            Some(Box::new(PendingErrorOrigin {
                error: error.clone(),
                instruction_ip,
                span,
            })),
        )
    }

    pub(super) fn throw_value(
        &mut self,
        value: Value,
        origin: Option<Box<PendingErrorOrigin>>,
    ) -> Result<bool, RuntimeError> {
        let mut imported = self.heap.import_values(vec![value], 0)?;
        let value = imported
            .pop()
            .expect("one thrown value produces one imported value");

        loop {
            let catch_index = self.handlers.iter().rposition(|handler| handler.catches);

            // A throw which cannot be caught by a handler installed from inside
            // the current finally replaces that finally's pending completion.
            if let Some(escaped) = self
                .finally_stack
                .pop_if(|finally| catch_index.is_none_or(|index| index < finally.handler_depth))
            {
                self.stack.truncate(escaped.stack_depth);
                continue;
            }

            let cleanup_floor = catch_index.map_or(0, |index| index + 1);
            let cleanup_index = (cleanup_floor..self.handlers.len())
                .rev()
                .find(|index| self.handlers[*index].finally_ip.is_some());
            if let Some(cleanup_index) = cleanup_index {
                let handler = self.handlers[cleanup_index].clone();
                self.handlers.truncate(cleanup_index);
                self.unwind_to_handler(&handler)?;
                let finally_ip = handler
                    .finally_ip
                    .expect("cleanup selection requires a finally target");
                self.finally_stack.push(FinallyState {
                    completion: FinallyCompletion::Throw { value, origin },
                    handler_depth: self.handlers.len(),
                    frame_depth: self.frames.len(),
                    frame_function: self.active_function,
                    stack_depth: handler.stack_depth,
                });
                self.ip = finally_ip;
                return Ok(true);
            }

            if let Some(catch_index) = catch_index {
                let handler = self.handlers[catch_index].clone();
                self.handlers.truncate(catch_index);
                self.unwind_to_handler(&handler)?;
                self.stack.push(value);
                self.ip = handler.handler_ip;
                return Ok(true);
            }

            self.handlers.clear();
            return Ok(false);
        }
    }

    fn unwind_to_handler(&mut self, handler: &ExceptionHandler) -> Result<(), RuntimeError> {
        if handler.frame_depth > self.frames.len() {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler frame depth exceeds the active stack".into(),
            });
        }
        while self.frames.len() > handler.frame_depth {
            self.unwind_exception_frame()?;
        }
        if self.active_function != handler.frame_function {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler frame identity does not match the active frame".into(),
            });
        }
        if handler.stack_depth > self.stack.len() || handler.iterator_depth > self.iter_stack.len()
        {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler restore depth exceeds active VM state".into(),
            });
        }
        self.stack.truncate(handler.stack_depth);
        while self.iter_stack.len() > handler.iterator_depth {
            let iterator = self
                .iter_stack
                .pop()
                .expect("iterator depth was checked above");
            self.slots
                .restore_temporary(iterator.binding, iterator.restore);
        }
        Ok(())
    }

    fn unwind_exception_frame(&mut self) -> Result<(), RuntimeError> {
        let frame = self
            .frames
            .pop()
            .ok_or(RuntimeError::InvalidExceptionState {
                reason: "frame stack underflow during exception unwind".into(),
            })?;
        self.stack.truncate(frame.operand_stack_base);
        self.slots = frame.slots;
        self.iter_stack = frame.iter_stack;
        self.extras_heapified = frame.extras_heapified;
        self.active_function = frame.function;
        self.ip = frame.return_ip;
        Ok(())
    }

    fn runtime_error_value(
        &mut self,
        error: &RuntimeError,
        instruction_ip: usize,
    ) -> Result<Value, RuntimeError> {
        if matches!(
            error,
            RuntimeError::CannotAssignField { actual, .. }
                | RuntimeError::CannotAssignIndex { actual }
                if matches!(actual.as_str(), "RegExp" | "Map" | "Set" | "Date")
                    || ErrorKind::from_name(actual).is_some()
        ) {
            return self
                .heap
                .allocate_error(ErrorKind::TypeError, error.to_string(), None, None);
        }
        let mut details = record_with_capacity(3);
        details.insert(
            "kind".to_string(),
            Value::String(if error.is_effect_failure() {
                "effect".into()
            } else {
                "runtime".into()
            }),
        );
        details.insert(
            "instruction".to_string(),
            Value::Number(instruction_ip as f64),
        );
        if let Some(operation) = self.effect_operation_name(instruction_ip) {
            details.insert("operation".to_string(), Value::String(operation.into()));
        }

        let mut record = record_with_capacity(4);
        record.insert(
            "name".to_string(),
            Value::String(if error.is_effect_failure() {
                "EffectError".into()
            } else {
                "RuntimeError".into()
            }),
        );
        record.insert(
            "message".to_string(),
            Value::String(error.to_string().into()),
        );
        record.insert("code".to_string(), Value::String(error.code().into()));
        record.insert("details".to_string(), Value::Record(Arc::new(details)));
        Ok(Value::Record(Arc::new(record)))
    }

    fn effect_operation_name(&self, instruction_ip: usize) -> Option<String> {
        match self.chunk.code.get(instruction_ip)? {
            Instruction::ResourceCall { operation, .. }
            | Instruction::ResourceCallUnwrap { operation, .. } => {
                Some(self.chunk.names[*operation].text.to_string())
            }
            Instruction::ResourceOperationBatch(_) => Some("resource_batch".to_string()),
            Instruction::AwaitHandle | Instruction::AwaitHandleUnwrap => Some("await".to_string()),
            Instruction::StartProcess { .. } => Some("start".to_string()),
            Instruction::SleepFor => Some("sleep_for".to_string()),
            Instruction::SleepUntil => Some("sleep_until".to_string()),
            Instruction::ProcessWaitSignal { .. } => Some("wait_signal".to_string()),
            Instruction::ProcessSignalRun { .. } => Some("signal_run".to_string()),
            Instruction::CancelHandle => Some("cancel".to_string()),
            Instruction::Print => Some("print".to_string()),
            Instruction::ProcessYield => Some("yield".to_string()),
            Instruction::ProcessWake => Some("wake".to_string()),
            Instruction::Finish => Some("finish".to_string()),
            Instruction::ProcessFail => Some("fail".to_string()),
            _ => None,
        }
    }
}
