use std::sync::{Arc, Condvar, Mutex};

use super::{
    Diagnostic, DiagnosticCode, ParserStack, Program, SourceLimits, guard_source_nesting,
    guard_source_size, parse_source,
};

/// One parser thread per frontend, with a stack sufficient for every admitted
/// source. The thread owns no parser or guest data between requests.
#[derive(Default)]
pub(crate) struct Parser {
    thread: Option<ParseThread>,
    pub(crate) spawns: usize,
    stack: ParserStack,
    limits: SourceLimits,
}

struct ParseThread {
    exchange: Arc<Exchange>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Exchange {
    state: Mutex<ParseState>,
    ready: Condvar,
}

#[derive(Default)]
enum ParseState {
    #[default]
    Idle,
    Request(String),
    Response(Result<Program, Diagnostic>),
    Closed,
}

impl Exchange {
    fn serve(&self, limits: SourceLimits) {
        loop {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while matches!(*state, ParseState::Idle | ParseState::Response(_)) {
                state = self
                    .ready
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let ParseState::Request(source) = std::mem::replace(&mut *state, ParseState::Idle)
            else {
                return;
            };
            drop(state);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_source(&source, limits)
            }))
            .unwrap_or_else(|_| Err(parser_failed()));
            drop(source);
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(*state, ParseState::Closed) {
                return;
            }
            *state = ParseState::Response(result);
            self.ready.notify_all();
        }
    }
}

impl Parser {
    pub(crate) fn with_stack(stack: ParserStack) -> Self {
        Self {
            stack,
            ..Self::default()
        }
    }

    pub(crate) fn with_limits(mut self, limits: SourceLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(crate) fn limits(&self) -> SourceLimits {
        self.limits
    }

    pub(crate) fn parse(&mut self, source: &str) -> Result<Program, Diagnostic> {
        guard_source_size(source, self.limits)?;
        guard_source_nesting(source, self.limits.max_nesting())?;
        if self.thread.is_none() {
            // Keep the same arithmetic no-abort guarantee as the standalone
            // parser, reserving once for the largest source instead of once
            // per cell. Untouched stack pages consume address space, not RSS.
            let stack_size = self
                .stack
                .size(self.limits.max_bytes())
                .filter(|size| *size > 0)
                .ok_or_else(|| {
                    Diagnostic::new(
                        DiagnosticCode::ParseResourcesUnavailable,
                        "parser stack reservation is zero or overflows",
                        None,
                    )
                })?;
            let limits = self.limits;
            // One serial caller and one parser share a reusable slot. Channel
            // rendezvous waiters otherwise allocate only when a handoff blocks.
            let exchange = Arc::new(Exchange::default());
            let serving = Arc::clone(&exchange);
            let handle = std::thread::Builder::new()
                .name("typescript-parse".to_owned())
                .stack_size(stack_size)
                .spawn(move || serving.serve(limits))
                .map_err(|error| Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    format!("the TypeScript parser could not reserve {stack_size} bytes of stack for a {}-byte source: {error}", source.len()),
                    None,
                ))?;
            self.spawns += 1;
            self.thread = Some(ParseThread {
                exchange,
                handle: Some(handle),
            });
        }
        let thread = self.thread.as_ref().ok_or_else(parser_failed)?;
        let exchange = &thread.exchange;
        let mut state = exchange
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, ParseState::Closed) {
            return Err(parser_failed());
        }
        *state = ParseState::Request(source.to_owned());
        exchange.ready.notify_all();
        while !matches!(*state, ParseState::Response(_) | ParseState::Closed) {
            state = exchange
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        match std::mem::replace(&mut *state, ParseState::Idle) {
            ParseState::Response(result) => result,
            _ => Err(parser_failed()),
        }
    }
}

fn parser_failed() -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::SyntaxError,
        "the TypeScript parser failed while reading this source",
        None,
    )
}

impl Drop for ParseThread {
    fn drop(&mut self) {
        *self
            .exchange
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ParseState::Closed;
        self.exchange.ready.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
