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
            "S01"
                | "S02"
                | "S05"
                | "S08"
                | "S09"
                | "S10"
                | "S11"
                | "S12"
                | "S19"
                | "S20"
                | "S23"
                | "S31"
                | "S32"
        ),
        "unknown H2 scenario"
    );
    let config = ProviderConfig {
        scenario: scenario.to_owned(),
    };
    let labels: Vec<_> = labels.iter().map(|label| (*label).to_owned()).collect();
    let gates = ledger_path.to_owned();
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
            let gates = gates.clone();
            async move {
                let fail = |error: anyhow::Error| {
                    lash::provider::LlmTransportError::new(format!(
                        "H2 provider fixture: {error:#}"
                    ))
                };
                let (response, gate) =
                    response(&config, &labels, protocol, &request, &ledger).map_err(fail)?;
                if let Some(input) = gate {
                    await_release(&gates, &input).await.map_err(fail)?;
                }
                Ok(response)
            }
        })
        .build()
        .into_handle())
}

/// The release file a case writes to let an isolated leg's first answer
/// return: the leg's input text, beside the provider ledger.
pub fn release_path(ledger: &Path, input: &str) -> std::path::PathBuf {
    ledger.with_file_name(format!("provider-release-{}", input.replace(' ', "-")))
}

/// Hold an isolated leg's first answer until its case released it: the
/// ledger record is the reached proof, so the case binds the Run's invocation
/// and arms its transport cuts before the cell calls the tool.
async fn await_release(ledger: &Path, input: &str) -> Result<()> {
    let release = release_path(ledger, input);
    while !release.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}

/// The answer, and the input whose release it waits for, if any.
fn response(
    config: &ProviderConfig,
    labels: &[String],
    protocol: FixtureProtocol,
    request: &LlmRequest,
    ledger: &Mutex<std::fs::File>,
) -> Result<(LlmResponse, Option<String>)> {
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
    if matches!(config.scenario.as_str(), "S19" | "S20") {
        return isolated_response(&text, &previous, request, &mut file);
    }
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
                _ => "intent",
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
                "S10" => {
                    "const timer = sleep(0); const a = tools.rank_one({}); const b = tools.rank_two({}); const c = tools.rank_three({}); await timer; print('unrelated-progress'); await Promise.all([a,b,c]); finish('drained');"
                }
                "S11" => {
                    "const winner = tools.winner({}); const loser = tools.loser({}); const value = await Promise.race([winner,loser]); await tools.after({}); finish(value);"
                }
                "S12" => {
                    "const gate = await tools.gate({}); const value = await tools.source({}); finish(gate + '|' + value);"
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
    Ok((
        LlmResponse {
            parts,
            ..Default::default()
        },
        None,
    ))
}

/// S19/S20: each leg is its own session and input, and its first request
/// calls the leg's one isolated tool. A later request of the same input (the
/// model asked again after a refused call) finishes without a tool.
fn isolated_response(
    text: &str,
    previous: &str,
    request: &LlmRequest,
    file: &mut std::fs::File,
) -> Result<(LlmResponse, Option<String>)> {
    // A retried model call must return the same script. Only a new logical
    // request advances the leg, even if transport delivered this one twice.
    let scope = serde_json::to_value(&request.scope)?;
    let mut previous_requests = BTreeSet::new();
    for line in previous.lines() {
        let value: serde_json::Value = serde_json::from_str(line)?;
        if value["input"] == text && value["scope"] != scope {
            previous_requests.insert(value["scope"].to_string());
        }
    }
    let asked = previous_requests.len();
    let tool = if text.contains("unbound") {
        "unbound"
    } else {
        "isolated"
    };
    let code = if asked == 0 {
        format!("const started = await tools.{tool}({{}});\nfinish(started);")
    } else {
        "finish('asked again');".to_owned()
    };
    let mut record = serde_json::to_vec(
        &serde_json::json!({"stage":asked,"input":text,"scope":request.scope,"request":request}),
    )?;
    record.push(b'\n');
    file.write_all(&record)?;
    file.sync_all()?;
    Ok((
        LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: format!("<typescript>\n{code}\n</typescript>"),
                response_meta: None,
            }],
            ..Default::default()
        },
        (asked == 0).then(|| text.to_owned()),
    ))
}
