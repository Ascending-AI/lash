//! Atomic per-line logging for the slack-clone processes.
//!
//! The bot, the platform and the bundled MCP servers all log from concurrent
//! tasks, and their operators (and the full-host E2E judge) read the merged
//! `stdout`+`stderr` stream one *line* at a time. `println!`/`eprintln!` cannot
//! promise that: `Stderr` is unbuffered and `write_fmt` emits one write syscall
//! per format fragment, so a second task writing between two fragments severs
//! the first line mid-way through — the exact failure recorded in FIG-1554,
//! where `slack-clone-bot settled deferred event ` and its `<id>: Replied {…}`
//! tail ended up on different physical lines with an unrelated line wedged
//! between them.
//!
//! [`log_out!`](crate::log_out) and [`log_err!`](crate::log_err) are drop-in
//! replacements: they format the *whole* line (trailing newline included) into
//! a `String` first and then hand it to a single `write_all` taken under one
//! process-wide output lock, so no two log writes from this process can ever
//! interleave — on either stream. The rendered text is byte-identical to what
//! the `println!`/`eprintln!` they replaced produced.

use std::fmt::{self, Write as _};
use std::io;
use std::sync::{Mutex, MutexGuard};

/// Serializes every log write in this process, across both streams.
///
/// `stdout` and `stderr` have independent locks in `std`, and the E2E harness
/// (like most process supervisors) points both at the same file, so per-stream
/// locking is not enough on its own.
static OUTPUT: Mutex<()> = Mutex::new(());

/// Which standard stream a line belongs on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    /// Standard output — the stream `println!` writes to.
    Out,
    /// Standard error — the stream `eprintln!` writes to.
    Err,
}

/// Formats `args` into one complete line, trailing newline included.
///
/// Formatting happens off the output lock: user `Display`/`Debug` code never
/// runs while the process-wide lock is held.
#[must_use]
pub fn render(args: fmt::Arguments<'_>) -> String {
    let mut line = String::new();
    // Writing into a `String` is infallible.
    let _ = line.write_fmt(args);
    line.push('\n');
    line
}

/// Takes the process-wide output lock, recovering a poisoned lock.
///
/// A panic elsewhere must not silence the log.
pub fn lock_output() -> MutexGuard<'static, ()> {
    OUTPUT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Writes one already-rendered line with a single `write_all` and flushes it.
///
/// The caller holds [`lock_output`]; splitting this out keeps the atomicity
/// contract testable against an ordinary writer.
pub fn write_atomic<W: io::Write>(writer: &mut W, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Renders one line and hands it to `sink` under the process-wide output lock.
///
/// This is the whole atomicity contract in one place: render off the lock, then
/// one locked hand-off. [`line`] is this function with the standard streams as
/// the sink, so a test that drives `emit` drives what the macros run — deleting
/// the lock here breaks both.
pub fn emit(args: fmt::Arguments<'_>, sink: impl FnOnce(&str) -> io::Result<()>) -> io::Result<()> {
    let rendered = render(args);
    let _guard = lock_output();
    sink(&rendered)
}

/// Renders and emits one log line atomically.
///
/// Write errors are dropped: a closed pipe must not take a long-running bot
/// down, and there is nowhere left to report it to.
pub fn line(stream: Stream, args: fmt::Arguments<'_>) {
    let _ = emit(args, |rendered| match stream {
        Stream::Out => write_atomic(&mut io::stdout().lock(), rendered),
        Stream::Err => write_atomic(&mut io::stderr().lock(), rendered),
    });
}

/// `println!` that cannot be severed by a concurrent log write.
#[macro_export]
macro_rules! log_out {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Out, ::std::format_args!($($arg)*))
    };
}

/// `eprintln!` that cannot be severed by a concurrent log write.
#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => {
        $crate::log::line($crate::log::Stream::Err, ::std::format_args!($($arg)*))
    };
}
