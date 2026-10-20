//! Opt-in crash-resume evidence: a non-deduplicating Once body and binding hashes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome, ToolProvider,
};
use serde_json::json;
use sha2::{Digest, Sha256};

// One cell, including its two fingerprints, survives either kill. The array
// carries four distinct chunks, 1 MiB of numeric payload before overhead.
// Bounded tool results avoid spending guest fuel on fixture construction.
pub(crate) const CELL: &str = r#"const crashText = "x".repeat(204800);
const crashArray = [
  await crash_fixture.array({part: 0}), await crash_fixture.array({part: 1}),
  await crash_fixture.array({part: 2}), await crash_fixture.array({part: 3})
];
const crashCount = await crash_fixture.increment({});
const crashBefore = {
  text_bytes: crashText.length,
  array_items: crashArray[0].length + crashArray[1].length + crashArray[2].length + crashArray[3].length,
  parts: [
    await crash_fixture.fingerprint({text: crashText, array: crashArray[0]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[1]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[2]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[3]})
  ]
};
console.log("crash-resume parked", crashBefore, crashCount);
await sleep(30000);
const crashAfter = {
  text_bytes: crashText.length,
  array_items: crashArray[0].length + crashArray[1].length + crashArray[2].length + crashArray[3].length,
  parts: [
    await crash_fixture.fingerprint({text: crashText, array: crashArray[0]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[1]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[2]}),
    await crash_fixture.fingerprint({text: crashText, array: crashArray[3]})
  ]
};
await chat.reply(JSON.stringify({marker: "crash-resume finished", before: crashBefore, after: crashAfter, count: crashCount}));"#;

pub(crate) const MIGRATION_SEED: &str = r#"function double(value: number) { return value * 2; }
const migrationSleeper = async (request: unknown) => {
  await sleep(600000);
  return request;
};
const migrationProcess = await processes.start({definition: migrationSleeper, args: {request: {marker: "migration-sweep process"}}});
await chat.reply(JSON.stringify({marker: "migration-sweep seeded", value: double(21), process: migrationProcess}));"#;

pub(crate) const MIGRATION_READ: &str = r#"await chat.reply(JSON.stringify({marker: "migration-sweep resumed", value: double(21), process: migrationProcess}));"#;

pub(crate) fn tools(data_dir: &Path) -> Result<Arc<dyn ToolProvider>> {
    let definitions = [
        ("array", lash::tools::ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN, 0, 0), json!({"type": "object", "properties": {"part": {"type": "integer", "minimum": 0, "maximum": 3}}, "required": ["part"], "additionalProperties": false})),
        ("increment", lash::tools::ExecutionPolicy::Once, json!({"type": "object", "properties": {}, "additionalProperties": false})),
        ("fingerprint", lash::tools::ExecutionPolicy::repeatable(std::num::NonZeroU32::MIN, 0, 0), json!({
            "type": "object", "properties": {
                "text": {"type": "string"},
                "array": {"type": "array", "items": {"type": "number"}}
            }, "required": ["text", "array"], "additionalProperties": false
        })),
    ].into_iter().map(|(name, policy, input)| {
        Ok(ToolDefinition::raw(
            format!("workbench:crash:{name}"), format!("crash_fixture.{name}"),
            "Development crash-resume evidence.", input, json!({}),
        )?.with_execution(Duration::from_secs(120))
            .with_execution_policy(policy))
    }).collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(StaticToolProvider::new(
        definitions,
        Evidence {
            ledger: data_dir.join("crash-once.jsonl"),
        },
    )))
}

struct Evidence {
    ledger: PathBuf,
}

impl Evidence {
    fn run(&self, call: ToolCall<'_>) -> Result<ToolOutcome> {
        if call.name() == "crash_fixture.array" {
            let part = call.args["part"]
                .as_f64()
                .filter(|part| (0.0..=3.0).contains(part) && part.fract() == 0.0)
                .ok_or_else(|| anyhow::anyhow!("part must be 0..3"))?;
            return Ok(ToolOutcome::ok(json!(vec![7.0 + part; 32768])));
        }
        if call.name() == "crash_fixture.increment" {
            // Each entry is a body invocation, even for the same call id. A
            // fixture-side dedup would conceal the very failure this row judges.
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.ledger)?;
            writeln!(file, "{}", json!({"call": call.context.call_id()}))?;
            file.sync_all()?;
            let count = std::fs::read_to_string(&self.ledger)?.lines().count();
            return Ok(ToolOutcome::ok(json!(count)));
        }
        let text = call.args["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("text missing"))?;
        let array = call.args["array"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("array missing"))?;
        let bytes = serde_json::to_vec(array)?;
        Ok(ToolOutcome::ok(json!({
            "text_bytes": text.len(), "array_items": array.len(),
            "text_sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
            "array_sha256": format!("{:x}", Sha256::digest(&bytes)),
        })))
    }
}

#[async_trait]
impl StaticToolExecute for Evidence {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match self.run(call) {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome::err(error.to_string()),
        }
        .into()
    }
}
