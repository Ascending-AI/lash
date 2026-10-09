use lash::vm::{ExecutionHostError, Record, Value};
use serde_json::{Value as JsonValue, json};

#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleOperation {
    kind: SampleOperationKind,
    pub module: &'static str,
    pub resource_type: &'static str,
    pub operation: &'static str,
    pub host_operation: &'static str,
    pub label: &'static str,
    pub fields: &'static [SampleField],
}

#[derive(Clone, Copy, Debug)]
enum SampleOperationKind {
    GmailListRecent,
    LlmQuery,
    WebSearch,
    AgentsSpawn,
    SlackRecent,
    GithubRecent,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SampleField {
    pub name: &'static str,
    pub field_type: &'static str,
    pub default: SampleDefault,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SampleDefault {
    String(&'static str),
    Number(f64),
    Expression(&'static str),
}

impl SampleDefault {
    pub(crate) fn editable(self) -> crate::EditableValue {
        match self {
            Self::String(value) => crate::EditableValue::String(value.to_string()),
            Self::Number(value) => crate::EditableValue::Number(value),
            Self::Expression(value) => crate::EditableValue::Expr(value.to_string()),
        }
    }
}

pub(crate) const OPERATIONS: &[SampleOperation] = &[
    SampleOperation {
        kind: SampleOperationKind::GmailListRecent,
        module: "gmail",
        resource_type: "MockGmail",
        operation: "list_recent",
        host_operation: "gmail.list_recent",
        label: "List recent emails",
        fields: &[SampleField {
            name: "count",
            field_type: "number",
            default: SampleDefault::Number(5.0),
        }],
    },
    SampleOperation {
        kind: SampleOperationKind::LlmQuery,
        module: "llm",
        resource_type: "MockLlm",
        operation: "query",
        host_operation: "llm_query",
        label: "Query LLM",
        fields: &[
            SampleField {
                name: "task",
                field_type: "string",
                default: SampleDefault::String("Summarize the supplied input"),
            },
            SampleField {
                name: "inputs",
                field_type: "expression",
                default: SampleDefault::Expression("{}"),
            },
            SampleField {
                name: "output",
                field_type: "expression",
                // A catalog default is authored source the fragment validator
                // has to accept: a type expression (`Type { .. }`) is not an
                // expression, so a palette insertion carrying one could never
                // be saved. The empty record is the unconstrained output shape
                // the workflows that call this operation already imply by
                // leaving the argument off (FIG-3179).
                default: SampleDefault::Expression("{}"),
            },
        ],
    },
    SampleOperation {
        kind: SampleOperationKind::WebSearch,
        module: "web",
        resource_type: "MockWeb",
        operation: "search",
        host_operation: "search_web",
        label: "Search the web",
        fields: &[
            SampleField {
                name: "query",
                field_type: "string",
                default: SampleDefault::String("NVIDIA stock outlook"),
            },
            SampleField {
                name: "limit",
                field_type: "number",
                default: SampleDefault::Number(5.0),
            },
        ],
    },
    SampleOperation {
        kind: SampleOperationKind::AgentsSpawn,
        module: "agents",
        resource_type: "MockAgents",
        operation: "spawn",
        host_operation: "spawn_agent",
        label: "Spawn subagent",
        fields: &[
            SampleField {
                name: "capability",
                field_type: "string",
                default: SampleDefault::String("explore"),
            },
            SampleField {
                name: "task",
                field_type: "string",
                default: SampleDefault::String("Research the supplied material"),
            },
            SampleField {
                name: "seed",
                field_type: "expression",
                default: SampleDefault::Expression("{}"),
            },
            SampleField {
                name: "output",
                field_type: "expression",
                // A catalog default is authored source the fragment validator
                // has to accept: a type expression (`Type { .. }`) is not an
                // expression, so a palette insertion carrying one could never
                // be saved. The empty record is the unconstrained output shape
                // the workflows that call this operation already imply by
                // leaving the argument off (FIG-3179).
                default: SampleDefault::Expression("{}"),
            },
        ],
    },
    SampleOperation {
        kind: SampleOperationKind::SlackRecent,
        module: "slack",
        resource_type: "MockSlack",
        operation: "recent",
        host_operation: "slack.recent",
        label: "Get recent Slack messages",
        fields: &[
            SampleField {
                name: "channel",
                field_type: "string",
                default: SampleDefault::String("team-platform"),
            },
            SampleField {
                name: "since",
                field_type: "string",
                default: SampleDefault::String("yesterday"),
            },
        ],
    },
    SampleOperation {
        kind: SampleOperationKind::GithubRecent,
        module: "github",
        resource_type: "MockGithub",
        operation: "recent",
        host_operation: "github.recent",
        label: "Get recent GitHub activity",
        fields: &[
            SampleField {
                name: "repo",
                field_type: "string",
                default: SampleDefault::String("acme/widgets"),
            },
            SampleField {
                name: "since",
                field_type: "string",
                default: SampleDefault::String("yesterday"),
            },
        ],
    },
];

impl SampleOperation {
    pub(crate) fn input_schema(self) -> JsonValue {
        match self.kind {
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
            SampleOperationKind::GmailListRecent => array_schema(
                json!({
                    "from": { "type": "string" },
                    "subject": { "type": "string" },
                    "snippet": { "type": "string" },
                    "unread": { "type": "boolean" }
                }),
                &["from", "subject", "snippet", "unread"],
            ),
            SampleOperationKind::LlmQuery | SampleOperationKind::AgentsSpawn => json!({}),
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

pub(crate) fn apply_tool(name: &str, args: &[Value]) -> Result<JsonValue, ExecutionHostError> {
    let operation = OPERATIONS
        .iter()
        .find(|operation| operation.host_operation.replace('.', "_") == name)
        .ok_or_else(|| ExecutionHostError::new(format!("unknown sample tool `{name}`")))?;
    let args = args
        .first()
        .and_then(Value::as_record)
        .ok_or_else(|| ExecutionHostError::new("a sample tool expects one record"))?;
    match operation.kind {
        SampleOperationKind::GmailListRecent => list_recent_emails(args),
        SampleOperationKind::LlmQuery => llm_query(args),
        SampleOperationKind::WebSearch => web_search(args),
        SampleOperationKind::AgentsSpawn => spawn_agent(args),
        SampleOperationKind::SlackRecent => recent_slack(args),
        SampleOperationKind::GithubRecent => recent_github(args),
    }
}

fn list_recent_emails(args: &Record) -> Result<JsonValue, ExecutionHostError> {
    let count = number_arg(args, "count")?;
    if !count.is_finite() || count < 0.0 {
        return Err(ExecutionHostError::new(
            "`count` must be a non-negative number",
        ));
    }
    let mut emails = vec![
        json!({
            "from": "Priya Shah <priya@example.com>",
            "subject": "Q3 planning decisions",
            "snippet": "We agreed to move the launch to September 18 and need final owners by Friday.",
            "unread": true
        }),
        json!({
            "from": "Alex Moreno <alex@example.com>",
            "subject": "Design review follow-up",
            "snippet": "The new checkout flow is approved pending the accessibility notes in the prototype.",
            "unread": true
        }),
        json!({
            "from": "Finance Team <finance@example.com>",
            "subject": "July expense deadline",
            "snippet": "Please submit July receipts by Thursday at 5 PM so payroll can close on time.",
            "unread": false
        }),
        json!({
            "from": "Mina Park <mina@example.com>",
            "subject": "Customer interview themes",
            "snippet": "Customers like saved views but want clearer sharing controls and faster exports.",
            "unread": true
        }),
        json!({
            "from": "Build Bot <ci@example.com>",
            "subject": "Nightly build recovered",
            "snippet": "The flaky integration suite passed after the retry fix; all main checks are green.",
            "unread": false
        }),
        json!({
            "from": "Jordan Lee <jordan@example.com>",
            "subject": "Friday team lunch",
            "snippet": "Lunch is booked for 12:30 at Little Lemon; reply with dietary needs by tomorrow.",
            "unread": true
        }),
    ];
    emails.truncate((count.floor() as usize).min(emails.len()));
    Ok(JsonValue::Array(emails))
}

fn llm_query(args: &Record) -> Result<JsonValue, ExecutionHostError> {
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

fn web_search(args: &Record) -> Result<JsonValue, ExecutionHostError> {
    let query = string_arg(args, "query")?;
    let limit = optional_number_arg(args, "limit")?.unwrap_or(5.0);
    if !(1.0..=20.0).contains(&limit) || limit.fract() != 0.0 {
        return Err(ExecutionHostError::new(
            "`limit` must be an integer between 1 and 20",
        ));
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

fn spawn_agent(args: &Record) -> Result<JsonValue, ExecutionHostError> {
    let capability = string_arg(args, "capability")?;
    if capability != "explore" && capability != "peer" {
        return Err(ExecutionHostError::new(format!(
            "unknown capability `{capability}`"
        )));
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

fn recent_slack(args: &Record) -> Result<JsonValue, ExecutionHostError> {
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

fn recent_github(args: &Record) -> Result<JsonValue, ExecutionHostError> {
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

fn string_arg(args: &Record, key: &str) -> Result<String, ExecutionHostError> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value.to_string()),
        _ => Err(ExecutionHostError::new(format!(
            "missing string argument `{key}`"
        ))),
    }
}

fn number_arg(args: &Record, key: &str) -> Result<f64, ExecutionHostError> {
    match args.get(key) {
        Some(Value::Number(value)) => Ok(*value),
        _ => Err(ExecutionHostError::new(format!(
            "missing number argument `{key}`"
        ))),
    }
}

fn optional_number_arg(args: &Record, key: &str) -> Result<Option<f64>, ExecutionHostError> {
    match args.get(key) {
        Some(Value::Number(value)) => Ok(Some(*value)),
        Some(_) => Err(ExecutionHostError::new(format!(
            "argument `{key}` must be a number"
        ))),
        None => Ok(None),
    }
}

fn record_arg<'a>(args: &'a Record, key: &str) -> Result<&'a Record, ExecutionHostError> {
    args.get(key)
        .and_then(Value::as_record)
        .ok_or_else(|| ExecutionHostError::new(format!("missing record argument `{key}`")))
}
