//! The host's completion secrets (ADR 0132 §6). Lash has no default: a host
//! configures the secret its wait keys are HMAC-minted under, and a durable
//! backend built without one is refused.

use std::path::Path;

use anyhow::Context as _;
use lash::durable::{CompletionKeySecrets, KeyVersion, SecretBytes};

/// The variable a deployment sets its secret in: at least 64 hex digits.
pub const COMPLETION_SECRET_ENV: &str = "LASH_COMPLETION_SECRET";

/// `LASH_COMPLETION_SECRET` as key version 1. Without it, the secret stored
/// at `persist`, minted there on first use so keys survive a restart; with
/// no `persist`, a fresh secret for this process alone, whose keys die with
/// it.
pub fn completion_secrets(persist: Option<&Path>) -> anyhow::Result<CompletionKeySecrets> {
    let hex = match std::env::var(COMPLETION_SECRET_ENV) {
        Ok(hex) => hex,
        Err(_) => match persist {
            Some(path) if path.exists() => std::fs::read_to_string(path)
                .with_context(|| format!("read the completion secret {}", path.display()))?,
            Some(path) => {
                let hex = fresh_secret();
                std::fs::write(path, &hex)
                    .with_context(|| format!("store the completion secret {}", path.display()))?;
                hex
            }
            None => fresh_secret(),
        },
    };
    let hex = hex.trim();
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|at| {
            hex.get(at..at + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .with_context(|| format!("{COMPLETION_SECRET_ENV} is not hex"))?;
    CompletionKeySecrets::new(
        KeyVersion(1),
        vec![(KeyVersion(1), SecretBytes::new(bytes))],
    )
    .context("the completion secret is refused")
}

/// 64 hex digits from two v4 UUIDs: 244 random bits.
fn fresh_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
