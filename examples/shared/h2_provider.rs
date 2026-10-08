//! H2's scripted provider validates actual model-facing returns before answering.
use anyhow::{Result, anyhow, ensure};
use lash::direct::LlmOutputPart;
use lash::provider::{
    LlmContentBlock, LlmRequest, LlmResponse, ProviderHandle, ProviderOptions, ProviderReliability,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
pub enum FixtureProtocol {
    Standard,
    Rlm,
}

#[derive(Clone)]
struct ProviderConfig {
    scenario: String,
}

/// The ledger belongs to the case and must survive a cold host replacement.
pub fn scripted_provider(
    scenario: &str,
    labels: &[&str],
    protocol: FixtureProtocol,
    ledger_path: &Path,
) -> Result<ProviderHandle> {
    ensure!(
        matches!(
            scenario,
            "S01" | "S02" | "S05" | "S11" | "S12" | "S18" | "S23" | "S31" | "S32"
        ),
        "unknown H2 scenario"
    );
    let config = ProviderConfig {
        scenario: scenario.to_owned(),
    };
    let labels: Vec<_> = labels.iter().map(|label| (*label).to_owned()).collect();
    let ledger = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(ledger_path)?,
    ));
    Ok(lash::testing::TestProvider::builder()
        .kind("h2-fixture")
        .serialize_config(|| serde_json::json!({"fixture":"h2-fixture"}))
        .options(ProviderOptions {
            reliability: ProviderReliability::disabled(),
            ..Default::default()
        })
        .complete(move |request| {
            let config = config.clone();
            let labels = labels.clone();
            let ledger = ledger.clone();
            async move {
                let fail = |error: anyhow::Error| {
                    lash::provider::LlmTransportError::new(format!(
                        "H2 provider fixture: {error:#}"
                    ))
                };
                response(&config, &labels, protocol, &request, &ledger).map_err(fail)
            }
        })
        .build()
        .into_handle())
}

/// The answer to one request.
fn response(
    config: &ProviderConfig,
    labels: &[String],
    protocol: FixtureProtocol,
    request: &LlmRequest,
    ledger: &Mutex<std::fs::File>,
) -> Result<LlmResponse> {
    use std::io::{Read, Seek};
    let start = request
        .messages
        .iter()
        .rposition(|message| message.starts_user_segment)
        .ok_or_else(|| anyhow!("fixture request has no genuine user input"))?;
    let text: String = request.messages[start]
        .blocks
        .iter()
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    ensure!(
        text.contains(&config.scenario),
        "provider received another scenario's input"
    );
    let returned: BTreeSet<_> = request.messages[start..]
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    let stage = if returned.is_empty() {
        "initial"
    } else {
        "terminal"
    };
    let mut file = ledger
        .lock()
        .map_err(|_| anyhow!("provider ledger writer panicked"))?;
    file.rewind()?;
    let mut previous = String::new();
    file.read_to_string(&mut previous)?;
    // S23 submits identical input again after cancelling the first Run; each
    // Run makes exactly one request of its own.
    let runs = if config.scenario == "S23" { 2 } else { 1 };
    let mut repeated = 0;
    for line in previous.lines() {
        let value: serde_json::Value = serde_json::from_str(line)?;
        repeated += usize::from(value["stage"] == stage);
    }
    ensure!(
        repeated < runs,
        "unexpected repeated provider request at {stage}"
    );
    let parts = match protocol {
        FixtureProtocol::Standard if stage == "initial" => {
            if config.scenario == "S05" {
                vec![LlmOutputPart::ToolCall {
                    call_id: "h2-batch".to_owned(), tool_name: "batch".to_owned(),
                    input_json: serde_json::json!({"tool_calls":labels.iter().map(|label| serde_json::json!({"tool":label,"parameters":{}})).collect::<Vec<_>>()} ).to_string(),
                    replay: None,
                }]
            } else {
                labels
                    .iter()
                    .map(|label| LlmOutputPart::ToolCall {
                        call_id: format!("h2-{label}"),
                        tool_name: label.clone(),
                        input_json: "{}".to_owned(),
                        replay: None,
                    })
                    .collect()
            }
        }
        FixtureProtocol::Standard => {
            let expected: BTreeSet<_> = if config.scenario == "S05" {
                BTreeSet::from(["h2-batch".to_owned()])
            } else {
                labels.iter().map(|label| format!("h2-{label}")).collect()
            };
            ensure!(
                returned
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>()
                    == expected,
                "provider saw missing or extra tool results"
            );
            let mut results = BTreeMap::new();
            for block in request.messages[start..]
                .iter()
                .flat_map(|message| message.blocks.iter())
            {
                if let LlmContentBlock::ToolResult {
                    call_id, content, ..
                } = block
                {
                    let mut text = String::new();
                    for part in content {
                        match part {
                            lash::messages::ModelToolReturnPart::Text { text: part } => {
                                text.push_str(part)
                            }
                            _ => {
                                return Err(anyhow!(
                                    "fixture result unexpectedly retained or attached"
                                ));
                            }
                        }
                    }
                    let value = serde_json::from_str::<serde_json::Value>(&text)
                        .unwrap_or(serde_json::Value::String(text));
                    ensure!(
                        results.insert(call_id.as_str(), value).is_none(),
                        "duplicate model-facing tool result"
                    );
                }
            }
            if config.scenario == "S05" {
                ensure!(
                    results["h2-batch"]
                        == serde_json::json!({"results":[
                            {"index":0,"tool":"a","success":true,"result":"A"},
                            {"index":1,"tool":"b","success":true,"result":"B"},
                            {"index":2,"tool":"c","success":true,"result":"C"}
                        ]}),
                    "batch presentation differs from source-order A/B/C results"
                );
            } else {
                for label in labels {
                    let value = match label.as_str() {
                        "a" => "A",
                        "b" => "B",
                        value => value,
                    };
                    ensure!(
                        results.get(format!("h2-{label}").as_str())
                            == Some(&serde_json::json!(value)),
                        "model-facing result differs from fixture body value"
                    );
                }
            }
            let answer = match config.scenario.as_str() {
                "S01" => "echo",
                "S02" => "A|B",
                "S05" => "A|B|C",
                _ => return Err(anyhow!("scenario needs an RLM channel")),
            };
            vec![LlmOutputPart::Text {
                text: answer.to_owned(),
                response_meta: None,
            }]
        }
        FixtureProtocol::Rlm => {
            ensure!(
                stage == "initial",
                "RLM fixture should finish in its single cell"
            );
            let code = match config.scenario.as_str() {
                "S11" => {
                    "const winner = tools.winner({}); const loser = tools.loser({}); const value = await Promise.race([winner,loser]); await tools.after({}); finish(value);"
                }
                "S12" => {
                    "const gate = await tools.gate({}); const value = await tools.source({}); finish(gate + '|' + value);"
                }
                "S18" => {
                    "const h = await tools.handle({}); const r = await processes.await({ handle: h }); finish('awaited');"
                }
                "S31" => {
                    "const winner = tools.winner({}); const loser = tools.loser({}); const value = await Promise.race([winner,loser]); const gate = await tools.gate({}); finish(value + '|' + gate);"
                }
                "S23" => {
                    "const winner = tools.winner({}); const source = tools.source({}); const value = await Promise.race([winner,source]); const gate = await tools.gate({}); const later = await tools.later({}); finish(value + '|' + gate + '|' + later);"
                }
                "S32" => {
                    "const winner = tools.winner({}); const source = tools.source({}); const value = await Promise.race([winner,source]); const gate = await tools.gate({}); finish(value + '|' + gate);"
                }
                _ => return Err(anyhow!("scenario needs a Standard channel")),
            };
            vec![LlmOutputPart::Text {
                text: format!("<typescript>\n{code}\n</typescript>"),
                response_meta: None,
            }]
        }
    };
    let mut record = serde_json::to_vec(
        &serde_json::json!({"stage":stage,"scope":request.scope,"request":request}),
    )?;
    record.push(b'\n');
    file.write_all(&record)?;
    file.sync_all()?;
    Ok(LlmResponse {
        parts,
        ..Default::default()
    })
}
