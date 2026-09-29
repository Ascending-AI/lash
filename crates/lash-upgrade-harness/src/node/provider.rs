//! The scripted provider a serving node drives turns with.
//!
//! Every model call is an effect the legs count: with an effects log the
//! provider appends one JSON line per call, naming the build and generation
//! that made it and the text of the message it answered. A message that
//! names a gate (`hold:<name>`) is held at the provider until the test
//! releases it: the provider writes `reached-<name>` into the gate
//! directory and answers only once `release-<name>` exists there. So a leg
//! can stop a turn inside its journal, move deployments around it, and let
//! it go, and still count every effect rather than time anything.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::identity::BuildLabel;

/// Where a serving node records and holds its model calls.
#[derive(Clone, Debug, Default, Args)]
pub struct ProviderArgs {
    /// Append one [`EffectRecord`] line per model call to this file.
    #[arg(long)]
    pub effects_log: Option<PathBuf>,
    /// Hold a message that names `hold:<gate>` until `release-<gate>`
    /// exists in this directory.
    #[arg(long)]
    pub gate_dir: Option<PathBuf>,
}

/// One model call, as the effects log records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub build: BuildLabel,
    pub generation: String,
    /// The text of the newest message the call answered.
    pub message: String,
}

/// The marker a message carries to be held at the provider.
const HOLD: &str = "hold:";

/// How often a held call looks for its release.
const GATE_POLL: Duration = Duration::from_millis(50);

/// The gate a message names, if any: the word after `hold:`.
pub fn gate_of(message: &str) -> Option<&str> {
    let start = message.find(HOLD)? + HOLD.len();
    let rest = &message[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// The file a held call writes when it reaches the provider.
pub fn reached_file(gate_dir: &Path, gate: &str) -> PathBuf {
    gate_dir.join(format!("reached-{gate}"))
}

/// The file whose existence releases a held call.
pub fn release_file(gate_dir: &Path, gate: &str) -> PathBuf {
    gate_dir.join(format!("release-{gate}"))
}

/// Every effect a log holds, in the order they were appended.
pub fn read_effects(path: &Path) -> Result<Vec<EffectRecord>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .with_context(|| format!("decode an effect of {}: {line}", path.display()))
        })
        .collect()
}

impl ProviderArgs {
    /// The arguments that give another process the same provider.
    pub(super) fn to_args(&self) -> Vec<std::ffi::OsString> {
        let mut args = Vec::new();
        if let Some(log) = &self.effects_log {
            args.push("--effects-log".into());
            args.push(log.clone().into_os_string());
        }
        if let Some(dir) = &self.gate_dir {
            args.push("--gate-dir".into());
            args.push(dir.clone().into_os_string());
        }
        args
    }

    /// Record one model call and, when its message names a gate, hold it
    /// until the gate is released.
    pub(super) async fn observe(
        &self,
        build: BuildLabel,
        generation: &str,
        message: &str,
    ) -> Result<()> {
        if let Some(log) = &self.effects_log {
            let mut line = serde_json::to_vec(&EffectRecord {
                build,
                generation: generation.to_owned(),
                message: message.to_owned(),
            })?;
            line.push(b'\n');
            // One append per record: O_APPEND keeps the lines of several
            // nodes sharing one log whole.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
                .and_then(|mut file| file.write_all(&line))
                .with_context(|| format!("append to {}", log.display()))?;
        }
        if let (Some(dir), Some(gate)) = (&self.gate_dir, gate_of(message)) {
            std::fs::write(reached_file(dir, gate), generation)
                .with_context(|| format!("mark gate {gate} reached"))?;
            while !release_file(dir, gate).exists() {
                tokio::time::sleep(GATE_POLL).await;
            }
        }
        Ok(())
    }
}

/// The text of the newest message of a model request.
pub(super) fn newest_message(request: &lash_core::llm::types::LlmRequest) -> String {
    request
        .messages
        .last()
        .and_then(|message| serde_json::to_value(message).ok())
        .map(|value| collect_text(&value))
        .unwrap_or_default()
}

/// Every string a message's JSON holds, joined: the provider only looks for
/// its markers in it.
fn collect_text(value: &serde_json::Value) -> String {
    let mut text = String::new();
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::String(part) => {
                text.push_str(part);
                text.push(' ');
            }
            serde_json::Value::Array(items) => stack.extend(items.iter().rev()),
            serde_json::Value::Object(fields) => stack.extend(fields.values().rev()),
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::gate_of;

    #[test]
    fn a_message_names_its_gate() {
        assert_eq!(gate_of("first turn hold:drive-1 please"), Some("drive-1"));
        assert_eq!(gate_of("hold:a_b"), Some("a_b"));
        assert_eq!(gate_of("no gate here"), None);
        assert_eq!(gate_of("hold: spaced"), None);
    }
}
