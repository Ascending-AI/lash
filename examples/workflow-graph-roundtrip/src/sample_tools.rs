use serde_json::{Map, Value as JsonValue, json};

type Record = Map<String, JsonValue>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleOperation {
    kind: SampleOperationKind,
    pub module: &'static str,
    pub operation: &'static str,
    pub host_operation: &'static str,
    pub label: &'static str,
}

#[derive(Clone, Copy, Debug)]
enum SampleOperationKind {
    GmailListRecent,
    LlmQuery,
    WebSearch,
    AgentsSpawn,
    SlackRecent,
    GithubRecent,
    ResearchSearch,
    ResearchSynthesize,
    MetricsFetch,
    MetricsReport,
}

pub(crate) const OPERATIONS: &[SampleOperation] = &[
    SampleOperation {
        kind: SampleOperationKind::ResearchSearch,
        module: "research",
        operation: "search",
        host_operation: "research.search",
        label: "Find research sources",
    },
    SampleOperation {
        kind: SampleOperationKind::ResearchSynthesize,
        module: "research",
        operation: "synthesize",
        host_operation: "research.synthesize",
        label: "Write cited research brief",
    },
    SampleOperation {
        kind: SampleOperationKind::MetricsFetch,
        module: "metrics",
        operation: "fetch",
        host_operation: "metrics.fetch",
        label: "Fetch business metrics",
    },
    SampleOperation {
        kind: SampleOperationKind::MetricsReport,
        module: "metrics",
        operation: "report",
        host_operation: "metrics.report",
        label: "Compare reporting periods",
    },
    SampleOperation {
        kind: SampleOperationKind::GmailListRecent,
        module: "gmail",
        operation: "list_recent",
        host_operation: "gmail.list_recent",
        label: "List recent emails",
    },
    SampleOperation {
        kind: SampleOperationKind::LlmQuery,
        module: "llm",
        operation: "query",
        host_operation: "llm_query",
        label: "Query LLM",
    },
    SampleOperation {
        kind: SampleOperationKind::WebSearch,
        module: "web",
        operation: "search",
        host_operation: "search_web",
        label: "Search the web",
    },
    SampleOperation {
        kind: SampleOperationKind::AgentsSpawn,
        module: "agents",
        operation: "spawn",
        host_operation: "spawn_agent",
        label: "Spawn subagent",
    },
    SampleOperation {
        kind: SampleOperationKind::SlackRecent,
        module: "slack",
        operation: "recent",
        host_operation: "slack.recent",
        label: "Get recent Slack messages",
    },
    SampleOperation {
        kind: SampleOperationKind::GithubRecent,
        module: "github",
        operation: "recent",
        host_operation: "github.recent",
        label: "Get recent GitHub activity",
    },
];

impl SampleOperation {
    pub(crate) fn input_schema(self) -> JsonValue {
        match self.kind {
            SampleOperationKind::ResearchSearch => object_schema(
                json!({"topic": {"type": "string"}, "max_sources": {"type": "integer", "minimum": 1, "maximum": 3}}),
                &["topic", "max_sources"],
            ),
            SampleOperationKind::ResearchSynthesize => object_schema(
                json!({"topic": {"type": "string"}, "sources": research_sources_schema()}),
                &["topic", "sources"],
            ),
            SampleOperationKind::MetricsFetch => object_schema(
                json!({"workspace": {"type": "string"}, "period": {"type": "string", "enum": ["current", "previous"]}}),
                &["workspace", "period"],
            ),
            SampleOperationKind::MetricsReport => object_schema(
                json!({"workspace": {"type": "string"}, "current": metrics_snapshot_schema(), "previous": metrics_snapshot_schema()}),
                &["workspace", "current", "previous"],
            ),
            SampleOperationKind::GmailListRecent => object_schema(
                json!({
                    "count": { "type": "number" }
                }),
                &["count"],
            ),
            SampleOperationKind::LlmQuery => object_schema(
                json!({
                    "task": { "type": "string" },
                    "inputs": {},
                    "output": { "type": "object", "additionalProperties": true }
                }),
                &["task"],
            ),
            SampleOperationKind::WebSearch => object_schema(
                json!({
                    "query": { "type": "string" },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 5
                    }
                }),
                &["query"],
            ),
            SampleOperationKind::AgentsSpawn => object_schema(
                json!({
                    "task": { "type": "string" },
                    "capability": { "type": "string", "enum": ["explore", "peer"] },
                    "output": {
                        "type": "object",
                        "additionalProperties": true
                    },
                    "seed": {
                        "type": "object",
                        "additionalProperties": true
                    }
                }),
                &["task", "capability"],
            ),
            SampleOperationKind::SlackRecent => object_schema(
                json!({
                    "channel": { "type": "string" },
                    "since": { "type": "string" }
                }),
                &["channel", "since"],
            ),
            SampleOperationKind::GithubRecent => object_schema(
                json!({
                    "repo": { "type": "string" },
                    "since": { "type": "string" }
                }),
                &["repo", "since"],
            ),
        }
    }

    pub(crate) fn output_schema(self) -> JsonValue {
        match self.kind {
            SampleOperationKind::ResearchSearch => object_schema(
                json!({"topic": {"type": "string"}, "sources": research_sources_schema()}),
                &["topic", "sources"],
            ),
            SampleOperationKind::ResearchSynthesize => object_schema(
                json!({
                    "title": {"type": "string"},
                    "summary": {"type": "string"},
                    "source_count": {"type": "integer"},
                    "citations": array_schema(json!({"source_id": {"type": "string"}, "url": {"type": "string"}}), &["source_id", "url"])
                }),
                &["title", "summary", "source_count", "citations"],
            ),
            SampleOperationKind::MetricsFetch => metrics_snapshot_schema(),
            SampleOperationKind::MetricsReport => object_schema(
                json!({
                    "title": {"type": "string"},
                    "summary": {"type": "string"},
                    "period": {"type": "string"},
                    "metrics": array_schema(json!({
                        "name": {"type": "string"},
                        "current": {"type": "number"},
                        "previous": {"type": "number"},
                        "change_pct": {"type": ["number", "null"]}
                    }), &["name", "current", "previous", "change_pct"])
                }),
                &["title", "summary", "period", "metrics"],
            ),
            SampleOperationKind::GmailListRecent => array_schema(
                json!({
                    "sender": { "type": "string" },
                    "subject": { "type": "string" },
                    "snippet": { "type": "string" },
                    "unread": { "type": "boolean" }
                }),
                &["sender", "subject", "snippet", "unread"],
            ),
            SampleOperationKind::LlmQuery => json!({"type":"string"}),
            SampleOperationKind::AgentsSpawn => json!({"anyOf":[
                object_schema(json!({"summary":{"type":"string"},"risks":{"type":"string"}}), &["summary", "risks"]),
                object_schema(json!({"digest":{"type":"string"},"blockers":{"type":"array","items":{"type":"string"}}}), &["digest", "blockers"])
            ]}),
            SampleOperationKind::WebSearch => object_schema(
                json!({
                    "results": {
                        "type": "array",
                        "items": object_schema(
                            json!({
                                "title": { "type": "string" },
                                "url": { "type": "string" },
                                "content": { "type": "string" }
                            }),
                            &["title", "url", "content"],
                        )
                    }
                }),
                &["results"],
            ),
            SampleOperationKind::SlackRecent => array_schema(
                json!({
                    "user": { "type": "string" },
                    "text": { "type": "string" },
                    "ts": { "type": "string" }
                }),
                &["user", "text", "ts"],
            ),
            SampleOperationKind::GithubRecent => array_schema(
                json!({
                    "author": { "type": "string" },
                    "kind": { "type": "string" },
                    "title": { "type": "string" }
                }),
                &["author", "kind", "title"],
            ),
        }
    }

    pub(crate) fn output_from_input(self) -> Option<(&'static str, Option<JsonValue>)> {
        match self.kind {
            SampleOperationKind::LlmQuery => Some(("output", Some(json!({ "type": "string" })))),
            SampleOperationKind::AgentsSpawn => Some(("output", None)),
            _ => None,
        }
    }
}

fn object_schema(properties: JsonValue, required: &[&str]) -> JsonValue {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn array_schema(properties: JsonValue, required: &[&str]) -> JsonValue {
    json!({
        "type": "array",
        "items": object_schema(properties, required)
    })
}

pub(crate) fn apply_tool(name: &str, args: &JsonValue) -> Result<JsonValue, String> {
    let operation = OPERATIONS
        .iter()
        .find(|operation| format!("{}.{}", operation.module, operation.operation) == name)
        .ok_or_else(|| format!("unknown sample tool `{name}`"))?;
    let args = args
        .as_object()
        .ok_or_else(|| "a sample tool expects one record".to_owned())?;
    match operation.kind {
        SampleOperationKind::GmailListRecent => list_recent_emails(args),
        SampleOperationKind::LlmQuery => llm_query(args),
        SampleOperationKind::WebSearch => web_search(args),
        SampleOperationKind::AgentsSpawn => spawn_agent(args),
        SampleOperationKind::SlackRecent => recent_slack(args),
        SampleOperationKind::GithubRecent => recent_github(args),
        SampleOperationKind::ResearchSearch => research_search(args),
        SampleOperationKind::ResearchSynthesize => research_synthesize(args),
        SampleOperationKind::MetricsFetch => metrics_fetch(args),
        SampleOperationKind::MetricsReport => metrics_report(args),
    }
}

fn list_recent_emails(args: &Record) -> Result<JsonValue, String> {
    let count = number_arg(args, "count")?;
    if !count.is_finite() || count < 0.0 {
        return Err("`count` must be a non-negative number".to_owned());
    }
    let mut emails = vec![
        json!({
            "sender": "Priya Shah <priya@example.com>",
            "subject": "Q3 planning decisions",
            "snippet": "We agreed to move the launch to September 18 and need final owners by Friday.",
            "unread": true
        }),
        json!({
            "sender": "Alex Moreno <alex@example.com>",
            "subject": "Design review follow-up",
            "snippet": "The new checkout flow is approved pending the accessibility notes in the prototype.",
            "unread": true
        }),
        json!({
            "sender": "Finance Team <finance@example.com>",
            "subject": "July expense deadline",
            "snippet": "Please submit July receipts by Thursday at 5 PM so payroll can close on time.",
            "unread": false
        }),
        json!({
            "sender": "Mina Park <mina@example.com>",
            "subject": "Customer interview themes",
            "snippet": "Customers like saved views but want clearer sharing controls and faster exports.",
            "unread": true
        }),
        json!({
            "sender": "Build Bot <ci@example.com>",
            "subject": "Nightly build recovered",
            "snippet": "The flaky integration suite passed after the retry fix; all main checks are green.",
            "unread": false
        }),
        json!({
            "sender": "Jordan Lee <jordan@example.com>",
            "subject": "Friday team lunch",
            "snippet": "Lunch is booked for 12:30 at Little Lemon; reply with dietary needs by tomorrow.",
            "unread": true
        }),
    ];
    emails.truncate((count.floor() as usize).min(emails.len()));
    Ok(JsonValue::Array(emails))
}

fn llm_query(args: &Record) -> Result<JsonValue, String> {
    let task = string_arg(args, "task")?;
    if task.to_ascii_lowercase().contains("format") {
        return Ok(json!(
            "Top 5 emails\n\n1. Q3 launch moves to September 18; final owners are due Friday.\n2. Checkout design is approved once accessibility feedback is addressed.\n3. Submit July receipts by Thursday at 5 PM.\n4. Interviews praise saved views and flag sharing controls and export speed.\n5. The nightly build is green after the integration retry fix."
        ));
    }
    let inputs = record_arg(args, "inputs")?;
    let text = string_arg(inputs, "snippet")?;
    let summary = if text.contains("September 18") {
        "Q3 launch moves to September 18; final owners are due Friday."
    } else if text.contains("accessibility") {
        "Checkout design is approved once the accessibility feedback is addressed."
    } else if text.contains("receipts") {
        "Submit July receipts by Thursday at 5 PM for payroll close."
    } else if text.contains("saved views") {
        "Interviews praise saved views and flag sharing controls and export speed."
    } else if text.contains("integration suite") {
        "The nightly build is green after fixing the flaky integration retry."
    } else {
        "Team lunch is Friday at 12:30; dietary needs are due tomorrow."
    };
    Ok(json!(summary))
}

fn web_search(args: &Record) -> Result<JsonValue, String> {
    let query = string_arg(args, "query")?;
    let limit = optional_number_arg(args, "limit")?.unwrap_or(5.0);
    if !(1.0..=20.0).contains(&limit) || limit.fract() != 0.0 {
        return Err("`limit` must be an integer between 1 and 20".to_owned());
    }
    let mut results = vec![
        json!({
            "title": "NVIDIA outlines the next generation of accelerated computing",
            "url": "https://example.com/nvidia-platform-outlook",
            "content": format!("Results for {query}: data-center demand remains strong as inference adoption broadens.")
        }),
        json!({
            "title": "Semiconductor outlook: demand, supply, and valuation",
            "url": "https://example.com/semiconductor-outlook",
            "content": "Analysts highlight execution, supply, customer concentration, export controls, and valuation as key risks."
        }),
        json!({
            "title": "Cloud providers expand AI infrastructure budgets",
            "url": "https://example.com/cloud-ai-spending",
            "content": "Large cloud providers continue investing in training, inference, networking, and custom silicon."
        }),
    ];
    results.truncate((limit as usize).min(results.len()));
    Ok(json!({ "results": results }))
}

fn spawn_agent(args: &Record) -> Result<JsonValue, String> {
    let capability = string_arg(args, "capability")?;
    if capability != "explore" && capability != "peer" {
        return Err(format!("unknown capability `{capability}`"));
    }
    let task = string_arg(args, "task")?;
    let _seed = record_arg(args, "seed")?;
    if task.to_ascii_lowercase().contains("nvidia") {
        Ok(json!({
            "summary": "NVIDIA remains strongly positioned in accelerated computing as AI training and inference demand expands. Its software and networking ecosystem strengthens the moat beyond individual chips.",
            "risks": "High expectations in the valuation, supply constraints, customer concentration, export restrictions, and competition from custom silicon."
        }))
    } else {
        Ok(json!({
            "digest": "Team standup digest\n\nSlack: the API rollout is unblocked, checkout accessibility notes remain, and staging metrics are stable.\n\nGitHub: two pull requests moved forward and the retry fix restored the nightly build. Today: land the accessibility follow-up and watch rollout health.",
            "blockers": ["Checkout accessibility notes still need to land."]
        }))
    }
}

fn recent_slack(args: &Record) -> Result<JsonValue, String> {
    let channel = string_arg(args, "channel")?;
    let _since = string_arg(args, "since")?;
    Ok(json!([
        {
            "user": "Priya",
            "text": format!("#{channel}: API rollout is unblocked after the config fix."),
            "ts": "2026-07-18T08:42:00Z"
        },
        {
            "user": "Alex",
            "text": "Checkout is ready once the accessibility notes land.",
            "ts": "2026-07-18T09:05:00Z"
        },
        {
            "user": "Mina",
            "text": "Staging latency and error-rate metrics stayed within target overnight.",
            "ts": "2026-07-18T09:27:00Z"
        }
    ]))
}

fn recent_github(args: &Record) -> Result<JsonValue, String> {
    let repo = string_arg(args, "repo")?;
    let _since = string_arg(args, "since")?;
    Ok(json!([
        {
            "author": "alexm",
            "kind": "pull_request",
            "title": format!("{repo}#482: Add accessible checkout focus states")
        },
        {
            "author": "priyashah",
            "kind": "commit",
            "title": "Retry transient integration setup failures"
        },
        {
            "author": "mina-park",
            "kind": "pull_request",
            "title": format!("{repo}#487: Expose rollout health metrics")
        }
    ]))
}

fn string_arg(args: &Record, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(JsonValue::String(value)) => Ok(value.clone()),
        _ => Err(format!("missing string argument `{key}`")),
    }
}

fn number_arg(args: &Record, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| format!("missing number argument `{key}`"))
}

fn optional_number_arg(args: &Record, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        Some(value) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("argument `{key}` must be a number")),
        None => Ok(None),
    }
}

fn record_arg<'a>(args: &'a Record, key: &str) -> Result<&'a Record, String> {
    args.get(key)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| format!("missing record argument `{key}`"))
}

fn research_sources_schema() -> JsonValue {
    array_schema(
        json!({
            "id": {"type": "string"},
            "title": {"type": "string"},
            "url": {"type": "string"},
            "excerpt": {"type": "string"}
        }),
        &["id", "title", "url", "excerpt"],
    )
}

fn metrics_snapshot_schema() -> JsonValue {
    object_schema(
        json!({
            "workspace": {"type": "string"},
            "period": {"type": "string"},
            "start": {"type": "string"},
            "end": {"type": "string"},
            "revenue": {"type": "number"},
            "active_accounts": {"type": "number"},
            "new_trials": {"type": "number"},
            "paid_conversions": {"type": "number"}
        }),
        &[
            "workspace",
            "period",
            "start",
            "end",
            "revenue",
            "active_accounts",
            "new_trials",
            "paid_conversions",
        ],
    )
}

fn research_search(args: &Record) -> Result<JsonValue, String> {
    let topic = string_arg(args, "topic")?;
    let count = number_arg(args, "max_sources")?;
    if !(1.0..=3.0).contains(&count) || count.fract() != 0.0 {
        return Err("`max_sources` must be an integer between 1 and 3".to_owned());
    }
    let mut sources = vec![
        json!({
            "id": "customer-interviews",
            "title": format!("Customer interviews: {topic}"),
            "url": "https://research.example.com/customer-interviews",
            "excerpt": "In 12 mocked interviews, 8 customers delayed setup because the first useful outcome was unclear."
        }),
        json!({
            "id": "activation-study",
            "title": format!("Activation cohort study: {topic}"),
            "url": "https://research.example.com/activation-cohorts",
            "excerpt": "The mocked cohort completing a guided checklist activated at 64%, compared with 41% without it. This is observational, not proof of causation."
        }),
        json!({
            "id": "support-review",
            "title": format!("Support review: {topic}"),
            "url": "https://research.example.com/support-review",
            "excerpt": "Of 45 mocked setup tickets, 19 involved connecting the first data source and 11 involved inviting teammates."
        }),
    ];
    sources.truncate(count as usize);
    Ok(json!({"topic": topic, "sources": sources}))
}

fn research_synthesize(args: &Record) -> Result<JsonValue, String> {
    let topic = string_arg(args, "topic")?;
    let sources = args
        .get("sources")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "`sources` must be an array".to_owned())?;
    let mut citations = Vec::with_capacity(sources.len());
    let mut paragraphs = vec![format!("Mock research brief: {topic}")];
    for source in sources {
        let source = source
            .as_object()
            .ok_or_else(|| "each source must be a record".to_owned())?;
        let id = string_arg(source, "id")?;
        let title = string_arg(source, "title")?;
        let excerpt = string_arg(source, "excerpt")?;
        let url = string_arg(source, "url")?;
        paragraphs.push(format!("{title}\n{excerpt} [{id}]"));
        citations.push(json!({"source_id": id, "url": url}));
    }
    paragraphs.push("Next step: validate these findings with customer evidence before changing the onboarding flow. All sources and findings in this demo are mocked.".to_owned());
    Ok(json!({
        "title": topic,
        "summary": paragraphs.join("\n\n"),
        "source_count": citations.len(),
        "citations": citations
    }))
}

fn metrics_fetch(args: &Record) -> Result<JsonValue, String> {
    let workspace = string_arg(args, "workspace")?;
    let period = string_arg(args, "period")?;
    let (start, end, revenue, active_accounts, new_trials, paid_conversions) = match period.as_str()
    {
        "current" => ("2026-10-05", "2026-10-11", 48_600, 1_284, 360, 72),
        "previous" => ("2026-09-28", "2026-10-04", 45_000, 1_200, 320, 56),
        _ => return Err("`period` must be `current` or `previous`".to_owned()),
    };
    Ok(json!({
        "workspace": workspace,
        "period": period,
        "start": start,
        "end": end,
        "revenue": revenue,
        "active_accounts": active_accounts,
        "new_trials": new_trials,
        "paid_conversions": paid_conversions
    }))
}

fn metrics_report(args: &Record) -> Result<JsonValue, String> {
    let workspace = string_arg(args, "workspace")?;
    let current = record_arg(args, "current")?;
    let previous = record_arg(args, "previous")?;
    let period = format!(
        "{} to {}",
        string_arg(current, "start")?,
        string_arg(current, "end")?
    );
    let title = format!("{workspace}: weekly KPIs");
    let mut lines = vec![format!("{title}\n{period} · mocked data")];
    let mut metrics = Vec::new();
    for (key, label) in [
        ("revenue", "Revenue (USD)"),
        ("active_accounts", "Active accounts"),
        ("new_trials", "New trials"),
        ("paid_conversions", "Paid conversions"),
    ] {
        let now = number_arg(current, key)?;
        let before = number_arg(previous, key)?;
        let change = if before == 0.0 {
            None
        } else {
            Some(((now - before) / before * 1_000.0).round() / 10.0)
        };
        let comparison = change.map_or_else(
            || "no prior baseline".to_owned(),
            |change| format!("{change:+.1}%"),
        );
        lines.push(format!("{label}: {now} vs {before} ({comparison})"));
        metrics.push(json!({
            "name": key,
            "current": now,
            "previous": before,
            "change_pct": change
        }));
    }
    Ok(json!({"title": title, "period": period, "summary": lines.join("\n"), "metrics": metrics}))
}

#[cfg(test)]
mod tests {
    use super::apply_tool;
    use serde_json::json;

    #[test]
    fn mock_reports_use_the_supplied_evidence_and_metrics() {
        let research = apply_tool(
            "research.synthesize",
            &json!({
                "topic": "Changed question",
                "sources": [{"id": "one", "title": "Supplied source", "url": "https://example.com/one", "excerpt": "Only this finding was supplied."}]
            }),
        ).expect("valid research input");
        assert_eq!(research["title"], "Changed question");
        assert_eq!(research["source_count"], 1);
        assert_eq!(
            research["citations"],
            json!([{"source_id": "one", "url": "https://example.com/one"}])
        );
        assert!(
            research["summary"]
                .as_str()
                .expect("summary")
                .contains("Only this finding was supplied.")
        );

        let mut current = apply_tool(
            "metrics.fetch",
            &json!({"workspace": "Test", "period": "current"}),
        )
        .expect("current snapshot");
        let previous = apply_tool(
            "metrics.fetch",
            &json!({"workspace": "Test", "period": "previous"}),
        )
        .expect("previous snapshot");
        current["revenue"] = json!(22_500);
        let report = apply_tool(
            "metrics.report",
            &json!({"workspace": "Test", "current": current, "previous": previous}),
        )
        .expect("report");
        assert_eq!(
            report["metrics"][0],
            json!({"name": "revenue", "current": 22500.0, "previous": 45000.0, "change_pct": -50.0})
        );
    }
}
