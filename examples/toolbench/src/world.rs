use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolCall, ToolDefinition,
    ToolDefinitionBindingExt, ToolOutcome, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WeatherReport {
    pub(crate) city: String,
    pub(crate) temperature_c: i64,
    pub(crate) condition: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MailMessage {
    pub(crate) id: String,
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) subject: String,
    pub(crate) body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Contact {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct World {
    pub(crate) weather: BTreeMap<String, WeatherReport>,
    pub(crate) kv: BTreeMap<String, String>,
    pub(crate) rendered_notes: BTreeMap<String, String>,
    pub(crate) mail: Vec<MailMessage>,
    pub(crate) contacts: BTreeMap<String, Contact>,
}

impl World {
    pub(crate) fn seeded() -> Self {
        Self {
            weather: BTreeMap::from([
                (
                    "Berlin".to_string(),
                    WeatherReport {
                        city: "Berlin".to_string(),
                        temperature_c: 12,
                        condition: "rain".to_string(),
                    },
                ),
                (
                    "Lisbon".to_string(),
                    WeatherReport {
                        city: "Lisbon".to_string(),
                        temperature_c: 24,
                        condition: "sunny".to_string(),
                    },
                ),
            ]),
            kv: BTreeMap::from([
                ("project".to_string(), "aurora".to_string()),
                ("theme".to_string(), "amber".to_string()),
                ("launch_code".to_string(), "L7".to_string()),
            ]),
            rendered_notes: BTreeMap::from([
                (
                    "N-7".to_string(),
                    "Record(id=N-7, title=Launch, owner=Imani, token=ALPHA-17)".to_string(),
                ),
                (
                    "N-9".to_string(),
                    "Lookup instruction: key=launch_code".to_string(),
                ),
            ]),
            mail: vec![
                MailMessage {
                    id: "m1".to_string(),
                    sender: "Ada".to_string(),
                    recipient: "me@example.test".to_string(),
                    subject: "Build".to_string(),
                    body: "Build 104 is green".to_string(),
                },
                MailMessage {
                    id: "m2".to_string(),
                    sender: "Lin".to_string(),
                    recipient: "me@example.test".to_string(),
                    subject: "Lunch".to_string(),
                    body: "Meet at noon".to_string(),
                },
            ],
            contacts: BTreeMap::from([(
                "C-17".to_string(),
                Contact {
                    id: "C-17".to_string(),
                    name: "Noor".to_string(),
                    email: "noor@example.test".to_string(),
                },
            )]),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SharedWorld(Arc<Mutex<World>>);

impl SharedWorld {
    pub(crate) fn new(world: World) -> Self {
        Self(Arc::new(Mutex::new(world)))
    }

    pub(crate) fn snapshot(&self) -> World {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(StaticToolProvider::new(definitions(), self.clone()))
    }
}

#[async_trait]
impl StaticToolExecute for SharedWorld {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        match execute_call(
            &mut self.0.lock().unwrap_or_else(|poison| poison.into_inner()),
            call,
        ) {
            Ok(value) => ToolOutcome::ok(value),
            Err(message) => ToolOutcome::err_fmt(message),
        }
    }
}

fn execute_call(world: &mut World, call: ToolCall<'_>) -> Result<Value, String> {
    match call.name {
        "weather_lookup" => {
            let city = required_string(call.args, "city")?;
            world
                .weather
                .get(city)
                .map(|report| json!(report))
                .ok_or_else(|| format!("unknown city `{city}`"))
        }
        "kv_get" => {
            let key = required_string(call.args, "key")?;
            world
                .kv
                .get(key)
                .map(|value| json!({ "key": key, "value": value }))
                .ok_or_else(|| format!("unknown key `{key}`"))
        }
        "kv_put" => {
            let key = required_string(call.args, "key")?.to_string();
            let value = required_string(call.args, "value")?.to_string();
            world.kv.insert(key.clone(), value.clone());
            Ok(json!({ "key": key, "value": value }))
        }
        "notes_render" => {
            let id = required_string(call.args, "id")?;
            world
                .rendered_notes
                .get(id)
                .map(|note| json!(note))
                .ok_or_else(|| format!("unknown note `{id}`"))
        }
        "mail_list" => Ok(json!({ "messages": world.mail })),
        "mail_send" => {
            let recipient = required_string(call.args, "recipient")?.to_string();
            let subject = required_string(call.args, "subject")?.to_string();
            let body = required_string(call.args, "body")?.to_string();
            let id = format!("m{}", world.mail.len() + 1);
            let message = MailMessage {
                id,
                sender: "me@example.test".to_string(),
                recipient,
                subject,
                body,
            };
            world.mail.push(message.clone());
            Ok(json!(message))
        }
        "contacts_get" => {
            let id = required_string(call.args, "id")?;
            world
                .contacts
                .get(id)
                .map(|contact| json!(contact))
                .ok_or_else(|| format!("unknown contact `{id}`"))
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{field}` must be a string"))
}

fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "weather_lookup",
            ["weather"],
            "lookup",
            "Look up one seeded city's weather. Returns a structured record.",
            object_schema(&[("city", "string")]),
            json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string" },
                    "temperature_c": { "type": "integer" },
                    "condition": { "type": "string" }
                },
                "required": ["city", "temperature_c", "condition"],
                "additionalProperties": false
            }),
        ),
        definition(
            "kv_get",
            ["kv"],
            "get",
            "Read a seeded key. Returns a structured key/value record and errors when absent.",
            object_schema(&[("key", "string")]),
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" }, "value": { "type": "string" } },
                "required": ["key", "value"],
                "additionalProperties": false
            }),
        ),
        definition(
            "kv_put",
            ["kv"],
            "put",
            "Write one key/value pair and return the stored structured record.",
            object_schema(&[("key", "string"), ("value", "string")]),
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" }, "value": { "type": "string" } },
                "required": ["key", "value"],
                "additionalProperties": false
            }),
        ),
        definition(
            "notes_render",
            ["notes"],
            "render",
            "Render a seeded note. Important: the result is a STRING containing record-looking text, not a structured record.",
            object_schema(&[("id", "string")]),
            json!({ "type": "string" }),
        ),
        definition(
            "mail_list",
            ["mail"],
            "list",
            "List every seeded mail message as structured records.",
            object_schema(&[]),
            json!({
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" }, "sender": { "type": "string" },
                                "recipient": { "type": "string" }, "subject": { "type": "string" },
                                "body": { "type": "string" }
                            },
                            "required": ["id", "sender", "recipient", "subject", "body"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["messages"],
                "additionalProperties": false
            }),
        ),
        definition(
            "mail_send",
            ["mail"],
            "send",
            "Append one deterministic mail record and return it. IDs are assigned m1, m2, and so on.",
            object_schema(&[
                ("recipient", "string"),
                ("subject", "string"),
                ("body", "string"),
            ]),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" }, "sender": { "type": "string" },
                    "recipient": { "type": "string" }, "subject": { "type": "string" },
                    "body": { "type": "string" }
                },
                "required": ["id", "sender", "recipient", "subject", "body"],
                "additionalProperties": false
            }),
        ),
        definition(
            "contacts_get",
            ["contacts"],
            "get",
            "Get a structured contact record. Only id, name, and email exist; there is no phone field.",
            object_schema(&[("id", "string")]),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" }, "name": { "type": "string" },
                    "email": { "type": "string" }
                },
                "required": ["id", "name", "email"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn definition<const N: usize>(
    name: &'static str,
    module: [&'static str; N],
    operation: &'static str,
    description: &'static str,
    input_schema: Value,
    output_schema: Value,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:toolbench_{name}"),
        name,
        description,
        input_schema,
        output_schema,
    )
    .with_tool_binding(ToolBinding::new(module, operation))
}

fn object_schema(fields: &[(&str, &str)]) -> Value {
    let properties = fields
        .iter()
        .map(|(name, kind)| ((*name).to_string(), json!({ "type": kind })))
        .collect::<serde_json::Map<_, _>>();
    let required = fields.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}
