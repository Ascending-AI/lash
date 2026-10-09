use super::{
    Diagnostic, DiagnosticCode, MAX_SOURCE_BYTES, ParserStack, Program, guard_source_nesting,
    guard_source_size, parse_source,
};

/// One parser thread per frontend, with a stack sufficient for every admitted
/// source. The thread owns no parser or guest data between requests.
#[derive(Default)]
pub(crate) struct Parser {
    thread: Option<ParseThread>,
    pub(crate) spawns: usize,
    stack: ParserStack,
}

struct ParseThread {
    requests: Option<std::sync::mpsc::SyncSender<String>>,
    responses: Option<std::sync::mpsc::Receiver<Result<Program, Diagnostic>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Parser {
    pub(crate) fn with_stack(stack: ParserStack) -> Self {
        Self {
            stack,
            ..Self::default()
        }
    }

    pub(crate) fn parse(&mut self, source: &str) -> Result<Program, Diagnostic> {
        guard_source_size(source)?;
        guard_source_nesting(source)?;
        if self.thread.is_none() {
            // Keep the same arithmetic no-abort guarantee as the standalone
            // parser, reserving once for the largest source instead of once
            // per cell. Untouched stack pages consume address space, not RSS.
            let stack_size = self
                .stack
                .size(MAX_SOURCE_BYTES)
                .filter(|size| *size > 0)
                .ok_or_else(|| {
                    Diagnostic::new(
                        DiagnosticCode::ParseResourcesUnavailable,
                        "parser stack reservation is zero or overflows",
                        None,
                    )
                })?;
            let (requests, incoming) = std::sync::mpsc::sync_channel::<String>(0);
            let (outgoing, responses) = std::sync::mpsc::sync_channel(0);
            let handle = std::thread::Builder::new()
                .name("typescript-parse".to_owned())
                .stack_size(stack_size)
                .spawn(move || {
                    for source in incoming {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            parse_source(&source)
                        })).unwrap_or_else(|_| Err(parser_failed()));
                        drop(source);
                        if outgoing.send(result).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|error| Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    format!("the TypeScript parser could not reserve {stack_size} bytes of stack for a {}-byte source: {error}", source.len()),
                    None,
                ))?;
            self.spawns += 1;
            self.thread = Some(ParseThread {
                requests: Some(requests),
                responses: Some(responses),
                handle: Some(handle),
            });
        }
        let thread = self.thread.as_ref().ok_or_else(parser_failed)?;
        thread
            .requests
            .as_ref()
            .ok_or_else(parser_failed)?
            .send(source.to_owned())
            .map_err(|_| parser_failed())?;
        thread
            .responses
            .as_ref()
            .ok_or_else(parser_failed)?
            .recv()
            .map_err(|_| parser_failed())?
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
        // Close both rendezvous channels before joining, including the result
        // channel so an abandoned result cannot keep the thread blocked.
        self.requests.take();
        self.responses.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
