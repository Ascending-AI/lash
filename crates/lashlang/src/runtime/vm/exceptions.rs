use std::sync::Arc;

use crate::span::Span;

use super::super::{
    ErrorKind, ExecutionHost, Instruction, RuntimeError, Value, record_with_capacity,
    tool_failure_fields,
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

    /// The pending completion the body would otherwise have resumed or rethrown is replaced by
    /// the jump that is leaving, so it is dropped here.
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

    /// Where an operation's ECMA-specified throw becomes the error object it
    /// throws (see [`RuntimeError::ecma_error`]): from here on it is an
    /// ordinary thrown value, exactly as if the guest had written
    /// `throw new TypeError(message)` at the failing expression.
    pub(super) fn ecma_throw(&mut self, error: RuntimeError) -> Result<RuntimeError, RuntimeError> {
        let Some((class, message)) = error.ecma_error() else {
            return Ok(error);
        };
        let value = self
            .heap
            .allocate_error(ErrorKind::from(class), Some(message), None, None)?;
        Ok(RuntimeError::UncaughtException { value })
    }

    pub(super) fn throw_runtime_error(
        &mut self,
        error: &RuntimeError,
        instruction_ip: usize,
        span: Option<Span>,
    ) -> Result<bool, RuntimeError> {
        let value = self.runtime_error_value(error, instruction_ip)?;
        // A thrown value is its own completion: a cleanup chain that ends with
        // nothing catching it re-raises the value, as it would an explicit
        // `throw`. Only a routed substrate failure keeps its typed origin.
        let origin = (!matches!(error, RuntimeError::UncaughtException { .. })).then(|| {
            Box::new(PendingErrorOrigin {
                error: error.clone(),
                instruction_ip,
                span,
            })
        });
        self.throw_value(value, origin)
    }

    #[expect(
        clippy::expect_used,
        reason = "one thrown value produces one heap import, established by import_values above, per the message"
    )]
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

    #[expect(
        clippy::expect_used,
        reason = "the iterator depth was checked against the handler above in the while condition, per the message"
    )]
    fn unwind_to_handler(&mut self, handler: &ExceptionHandler) -> Result<(), RuntimeError> {
        if handler.frame_depth > self.frames.len() {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "handler frame depth exceeds the active stack".into(),
            });
        }
        while self.frames.len() > handler.frame_depth {
            self.unwind_exception_frame()?;
        }
        // An instruction suspended for a guest `valueOf`/`toString` at or
        // above the handler's frame was abandoned by this throw; its answers
        // must not reach the next run of the same instruction.
        self.abandon_guest_coercions(handler.frame_depth);
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
        self.active_function = frame.function;
        self.ip = frame.return_ip;
        Ok(())
    }

    pub(super) fn runtime_error_value(
        &mut self,
        error: &RuntimeError,
        instruction_ip: usize,
    ) -> Result<Value, RuntimeError> {
        if let RuntimeError::UncaughtException { value } = error {
            return Ok(value.clone());
        }
        if let Some((class, message)) = error.ecma_error() {
            return self
                .heap
                .allocate_error(ErrorKind::from(class), Some(message), None, None);
        }
        if matches!(
            error,
            RuntimeError::CannotAssignField { actual, .. }
                | RuntimeError::CannotAssignIndex { actual }
                if matches!(actual.as_str(), "RegExp" | "Map" | "Set" | "Date" | "function")
                    || ErrorKind::from_name(actual).is_some()
        ) {
            return self.heap.allocate_error(
                ErrorKind::TypeError,
                Some(error.to_string()),
                None,
                None,
            );
        }
        // Calling a value without [[Call]] raises a TypeError in ECMA-262; the
        // message keeps the substrate's naming ("attempted to call a
        // non-function value") inside the guest-visible error.
        if matches!(
            error,
            RuntimeError::NonFunctionCall { .. } | RuntimeError::IncompatibleReceiver { .. }
        ) {
            return self.heap.allocate_error(
                ErrorKind::TypeError,
                Some(error.to_string()),
                None,
                None,
            );
        }
        // Reading a member off `undefined`/`null` is ECMA's most common
        // TypeError; the substrate's CannotReadField/CannotIndex names it.
        if let RuntimeError::CannotReadField { field, actual }
        | RuntimeError::CannotAssignField { field, actual } = error
            && matches!(actual.as_str(), "undefined" | "null")
        {
            return self.heap.allocate_error(
                ErrorKind::TypeError,
                Some(format!(
                    "Cannot read properties of {actual} (reading '{field}')"
                )),
                None,
                None,
            );
        }
        if let RuntimeError::CannotIndex { actual } = error
            && matches!(actual.as_str(), "undefined" | "null")
        {
            return self.heap.allocate_error(
                ErrorKind::TypeError,
                Some(format!("Cannot read properties of {actual}")),
                None,
                None,
            );
        }
        // Runtime paths name a native error by prefixing its message
        // (`"TypeError: ..."` from the nullish-receiver stdlib guard,
        // `"RangeError: Invalid array length"`, ...). The guest-visible
        // value is that error kind, so `e instanceof TypeError` answers.
        if let RuntimeError::ValidationFailed { reason } = error {
            for (prefix, kind) in [
                ("TypeError: ", ErrorKind::TypeError),
                ("RangeError: ", ErrorKind::RangeError),
                ("SyntaxError: ", ErrorKind::SyntaxError),
                ("ReferenceError: ", ErrorKind::ReferenceError),
            ] {
                if let Some(message) = reason.strip_prefix(prefix) {
                    return self
                        .heap
                        .allocate_error(kind, Some(format!("{message}")), None, None);
                }
            }
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

        let brand = if error.is_effect_failure() {
            ErrorKind::EffectError
        } else {
            ErrorKind::RuntimeError
        };
        let structured_host_error = error.execution_host_error().and_then(tool_failure_fields);
        let message = error
            .execution_host_error()
            .filter(|error| error.tool_failure_code().is_some())
            .map_or_else(|| error.to_string(), |error| error.message().to_string());
        // A catch clause holds an idiomatic JavaScript error: `instanceof
        // Error`, an informative `String(error)`, and a `message` a model can
        // read. That is an Error object of the guest's own value model, not a
        // record shaped like one, so the typed payload rides on `cause` — the
        // one ECMA-documented slot an error carries for exactly this.
        let mut cause = structured_host_error.unwrap_or_else(|| {
            let mut cause = record_with_capacity(1);
            cause.insert("code".to_string(), Value::String(error.code().into()));
            cause
        });
        cause.insert("details".to_string(), Value::Record(Arc::new(details)));
        self.heap.allocate_error(
            brand,
            Some(message),
            Some(Value::Record(Arc::new(cause))),
            None,
        )
    }

    fn effect_operation_name(&self, instruction_ip: usize) -> Option<String> {
        match self.chunk.code.get(instruction_ip)? {
            Instruction::ResourceCall { operation, .. }
            | Instruction::ResourceCallUnwrap { operation, .. } => {
                Some(self.chunk.names[*operation].text.to_string())
            }
            Instruction::ResourceOperationBatch(_) | Instruction::ResourceOperationListBatch(_) => {
                Some("resource_batch".to_string())
            }
            // A handle await is an await whether the front end could type the
            // binding as a handle or had to classify it at runtime: the author
            // wrote `await`, and the error detail names what they wrote.
            Instruction::AwaitHandle
            | Instruction::AwaitHandleUnwrap
            | Instruction::AwaitPending => Some("await".to_string()),
            Instruction::SleepFor => Some("sleep_for".to_string()),
            Instruction::SleepUntil => Some("sleep_until".to_string()),
            Instruction::ProcessWaitSignal { .. } => Some("wait_signal".to_string()),
            Instruction::Print => Some("print".to_string()),
            Instruction::ProcessYield => Some("yield".to_string()),
            Instruction::Finish => Some("finish".to_string()),
            Instruction::ProcessFail => Some("fail".to_string()),
            _ => None,
        }
    }
}
