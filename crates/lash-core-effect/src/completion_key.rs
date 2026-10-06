use crate::AwaitEventKey;

/// Result of preparing an externally routable tool completion key.
pub enum CompletionKeyPreparation {
    NotNeeded,
    Unsupported,
    Issued(AwaitEventKey),
}
