//! Process-wide policy for panics contained at host-code seams.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};

static LOUD: AtomicBool = AtomicBool::new(false);

/// Production hosts leave this disabled so host panics become typed failures.
/// Test harnesses, simulators, and confidence binaries enable it at startup so
/// the same typed mapping remains visible while the panic still reaches the
/// harness.
pub fn set_loud(loud: bool) -> bool {
    LOUD.swap(loud, Ordering::SeqCst)
}

pub fn is_loud() -> bool {
    LOUD.load(Ordering::SeqCst)
}

pub fn payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Re-raises `payload` only when process-wide loud containment is enabled.
pub fn enforce_loudness(payload: Box<dyn Any + Send>) {
    if is_loud() {
        std::panic::resume_unwind(payload);
    }
}

pub fn enforce_message(code: &'static str, message: &str) {
    enforce_loudness(Box::new(format!("{code}: {message}")));
}
