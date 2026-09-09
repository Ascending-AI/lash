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
    pub(crate) catalog: Catalog,
    pub(crate) retail: Retail,
    pub(crate) ops: Operations,
    pub(crate) weather: BTreeMap<String, WeatherReport>,
    pub(crate) kv: BTreeMap<String, String>,
    pub(crate) rendered_notes: BTreeMap<String, String>,
    pub(crate) mail: Vec<MailMessage>,
    pub(crate) contacts: BTreeMap<String, Contact>,
}

impl World {
    pub(crate) fn seeded() -> Self {
        Self {
            catalog: Catalog::Easy,
            retail: Retail::seeded(),
            ops: Operations::seeded(),
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
pub(crate) struct SharedWorld(Arc<Mutex<World>>, Arc<Mutex<Vec<Value>>>);

impl SharedWorld {
    pub(crate) fn new(world: World) -> Self {
        Self(Arc::new(Mutex::new(world)), Arc::default())
    }

    pub(crate) fn snapshot(&self) -> World {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn submissions(&self) -> Vec<Value> {
        self.1
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn standard_provider(&self) -> Arc<dyn ToolProvider> {
        let mut tools = definitions(self.snapshot().catalog);
        tools.push(ToolDefinition::raw("tool:toolbench_submit", "submit", "Submit exactly the value the task asks for and end the task.", json!({"type":"object", "properties":{"value":{"type":["number","string","boolean","null","array","object"]}}, "required":["value"], "additionalProperties":false}), json!({})));
        Arc::new(StaticToolProvider::new(tools, self.clone()))
    }

    fn submit(&self, args: &Value) -> ToolOutcome {
        let Some(value) = args.get("value") else {
            return ToolOutcome::err_fmt("value is required");
        };
        self.1
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(value.clone());
        ToolOutcome::ok(value.clone()).with_control(lash::tools::ToolControl::Finish {
            value: lash::tools::ToolValue::untrusted_json(value.clone()),
        })
    }

    pub(crate) fn provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(StaticToolProvider::new(
            definitions(self.snapshot().catalog),
            self.clone(),
        ))
    }
}

#[async_trait]
impl StaticToolExecute for SharedWorld {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        if call.name == "submit" {
            return self.submit(call.args);
        }
        match execute_call(
            &mut self.0.lock().unwrap_or_else(|poison| poison.into_inner()),
            call,
        ) {
            Ok(value) => ToolOutcome::ok(value),
            Err(message) => ToolOutcome::err_fmt(message),
        }
    }
}

pub(crate) fn execute_call(world: &mut World, call: ToolCall<'_>) -> Result<Value, String> {
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
        _ => hard_call(world, call.name, call.args),
    }
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{field}` must be a string"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Catalog {
    Easy,
    Retail,
    Ops,
}

fn definitions(catalog: Catalog) -> Vec<ToolDefinition> {
    match catalog {
        Catalog::Easy => easy_definitions(),
        Catalog::Retail | Catalog::Ops => {
            let prefix = if catalog == Catalog::Retail {
                "retail_"
            } else {
                "ops_"
            };
            hard_definitions()
                .into_iter()
                .filter(|d| d.manifest.name.starts_with(prefix))
                .collect()
        }
    }
}

fn easy_definitions() -> Vec<ToolDefinition> {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn submit_preserves_first_value_and_records_duplicates() {
        let world = SharedWorld::new(World::seeded());
        world.submit(&json!({"value": null}));
        world.submit(&json!({"value": 2}));
        assert_eq!(world.submissions(), vec![Value::Null, json!(2)]);
        assert_eq!(world.snapshot(), World::seeded());
    }
}

// Original, closed fixtures: retail has 16 records; operations has 18.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Customer {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) pending_cents: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Order {
    pub(crate) id: String,
    pub(crate) customer_id: String,
    pub(crate) sku: String,
    pub(crate) quantity: i64,
    pub(crate) paid_cents: i64,
    pub(crate) status: String,
    pub(crate) age_days: i64,
    pub(crate) delivery_day: i64,
    pub(crate) refunded_cents: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Product {
    pub(crate) sku: String,
    pub(crate) category: String,
    pub(crate) price_cents: i64,
    pub(crate) stock: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Retail {
    pub(crate) customers: Vec<Customer>,
    pub(crate) orders: Vec<Order>,
    pub(crate) products: Vec<Product>,
}
impl Retail {
    fn seeded() -> Self {
        Self {
            customers: [("C1", "Mira", 0), ("C2", "Owen", 500), ("C3", "Sana", 0)]
                .into_iter()
                .map(|(id, name, pending_cents)| Customer {
                    id: id.into(),
                    name: name.into(),
                    pending_cents,
                })
                .collect(),
            orders: [
                ("R1", "C1", "P1", 2, 2200, "delivered", 12, 10),
                ("R2", "C1", "P2", 1, 1800, "delivered", 45, 11),
                ("R3", "C1", "P3", 3, 1800, "delivered", 5, 12),
                ("R4", "C2", "P1", 1, 1200, "pending", 0, 18),
                ("R5", "C3", "P2", 2, 3600, "pending", 0, 20),
                ("R6", "C3", "P3", 1, 700, "shipped", 2, 19),
                ("R7", "C3", "P1", 1, 1200, "pending", 0, 17),
            ]
            .into_iter()
            .map(
                |(id, customer_id, sku, quantity, paid_cents, status, age_days, delivery_day)| {
                    Order {
                        id: id.into(),
                        customer_id: customer_id.into(),
                        sku: sku.into(),
                        quantity,
                        paid_cents,
                        status: status.into(),
                        age_days,
                        delivery_day,
                        refunded_cents: 0,
                    }
                },
            )
            .collect(),
            products: [
                ("P1", "lamp", 1200, 4),
                ("P2", "lamp", 1800, 0),
                ("P3", "cable", 700, 8),
                ("P4", "lamp", 1500, 5),
                ("P5", "lamp", 900, 1),
                ("P6", "cable", 500, 9),
            ]
            .into_iter()
            .map(|(sku, category, price_cents, stock)| Product {
                sku: sku.into(),
                category: category.into(),
                price_cents,
                stock,
            })
            .collect(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Service {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) team_id: String,
    pub(crate) deployed_release: String,
    pub(crate) capacity: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ticket {
    pub(crate) id: String,
    pub(crate) service_id: String,
    pub(crate) severity: i64,
    pub(crate) affected_users: i64,
    pub(crate) status: String,
    pub(crate) fix_release: String,
    pub(crate) blocked_by: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Release {
    pub(crate) id: String,
    pub(crate) service_id: String,
    pub(crate) version: i64,
    pub(crate) required_capacity: i64,
    pub(crate) checks: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Team {
    pub(crate) id: String,
    pub(crate) primary: String,
    pub(crate) backup: String,
    pub(crate) primary_available: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Operations {
    pub(crate) services: Vec<Service>,
    pub(crate) tickets: Vec<Ticket>,
    pub(crate) releases: Vec<Release>,
    pub(crate) teams: Vec<Team>,
}
impl Operations {
    fn seeded() -> Self {
        Self {
            services: [
                ("S1", "Beacon", "T1", "V1", 4),
                ("S2", "Harbor", "T2", "V4", 8),
                ("S3", "Relay", "T3", "V6", 2),
            ]
            .into_iter()
            .map(|(id, name, team_id, deployed_release, capacity)| Service {
                id: id.into(),
                name: name.into(),
                team_id: team_id.into(),
                deployed_release: deployed_release.into(),
                capacity,
            })
            .collect(),
            tickets: [
                ("I1", "S1", 1, 120, "open", "V2", vec![]),
                ("I2", "S1", 2, 80, "open", "V2", vec!["I1"]),
                ("I3", "S1", 1, 900, "resolved", "V1", vec![]),
                ("I4", "S2", 1, 250, "open", "V5", vec![]),
                ("I5", "S2", 3, 40, "open", "V5", vec![]),
                ("I6", "S3", 2, 60, "open", "V6", vec![]),
            ]
            .into_iter()
            .map(
                |(id, service_id, severity, affected_users, status, fix_release, blocked_by)| {
                    Ticket {
                        id: id.into(),
                        service_id: service_id.into(),
                        severity,
                        affected_users,
                        status: status.into(),
                        fix_release: fix_release.into(),
                        blocked_by: blocked_by.into_iter().map(str::to_owned).collect(),
                    }
                },
            )
            .collect(),
            releases: [
                ("V1", "S1", 1, 1, "passed"),
                ("V2", "S1", 2, 3, "passed"),
                ("V3", "S1", 3, 6, "passed"),
                ("V4", "S2", 1, 2, "passed"),
                ("V5", "S2", 2, 5, "failed"),
                ("V6", "S3", 1, 1, "passed"),
            ]
            .into_iter()
            .map(
                |(id, service_id, version, required_capacity, checks)| Release {
                    id: id.into(),
                    service_id: service_id.into(),
                    version,
                    required_capacity,
                    checks: checks.into(),
                },
            )
            .collect(),
            teams: [
                ("T1", "ida@example.test", "leo@example.test", false),
                ("T2", "nia@example.test", "max@example.test", true),
                ("T3", "ari@example.test", "kim@example.test", true),
            ]
            .into_iter()
            .map(|(id, primary, backup, primary_available)| Team {
                id: id.into(),
                primary: primary.into(),
                backup: backup.into(),
                primary_available,
            })
            .collect(),
        }
    }
}

// Domain refusals are structured JSON data on BOTH channels, so programs can
// inspect error.code without depending on dialect-specific exception support.
fn refusal(code: &str) -> Value {
    json!({"error":{"code":code}})
}
// JSON Schema integers include numbers such as 22.0. Keep conversion bounded
// to the signed representation used by the world, without truncation/saturation.
fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let n = value.as_f64()?;
        (n.fract() == 0.0 && n >= i64::MIN as f64 && n < -(i64::MIN as f64)).then_some(n as i64)
    })
}
pub(crate) fn hard_call(world: &mut World, name: &str, args: &Value) -> Result<Value, String> {
    // Enforce the same strict inputs in the direct oracle path as in the host.
    let definitions = hard_definitions();
    let definition = definitions
        .iter()
        .find(|d| d.manifest.name == name)
        .ok_or_else(|| format!("unknown tool `{name}`"))?;
    let schema = definition.contract.input_schema.canonical();
    let object = args.as_object().ok_or("arguments must be an object")?;
    let properties = schema["properties"].as_object().unwrap();
    if object.len() != properties.len()
        || properties
            .iter()
            .any(|(key, field)| match field["type"].as_str() {
                Some("string") => !object.get(key).is_some_and(Value::is_string),
                Some("integer") => !object.get(key).is_some_and(|v| integer(v).is_some()),
                _ => true,
            })
    {
        return Err("arguments must match the strict tool schema".into());
    }
    match name {
        "retail_customer" => {
            let name = required_string(args, "name")?;
            world
                .retail
                .customers
                .iter()
                .find(|c| c.name == name)
                .map(|c| json!(c))
                .ok_or_else(|| "unknown customer".into())
        }
        "retail_orders" => {
            let id = required_string(args, "customer_id")?;
            Ok(json!(
                world
                    .retail
                    .orders
                    .iter()
                    .filter(|o| o.customer_id == id)
                    .map(|o| &o.id)
                    .collect::<Vec<_>>()
            ))
        }
        "retail_order" => {
            let id = required_string(args, "id")?;
            world
                .retail
                .orders
                .iter()
                .find(|o| o.id == id)
                .map(|o| json!(o))
                .ok_or_else(|| "unknown order".into())
        }
        "retail_products" => {
            let category = required_string(args, "category")?;
            Ok(json!(
                world
                    .retail
                    .products
                    .iter()
                    .filter(|p| p.category == category)
                    .collect::<Vec<_>>()
            ))
        }
        "retail_product" => {
            let sku = required_string(args, "sku")?;
            world
                .retail
                .products
                .iter()
                .find(|p| p.sku == sku)
                .map(|p| json!(p))
                .ok_or_else(|| "unknown product".into())
        }
        "retail_exchange" => {
            let id = required_string(args, "order_id")?;
            let sku = required_string(args, "sku")?;
            let order = world
                .retail
                .orders
                .iter()
                .position(|o| o.id == id)
                .ok_or("unknown order")?;
            let product = world
                .retail
                .products
                .iter()
                .position(|p| p.sku == sku)
                .ok_or("unknown product")?;
            let o = &world.retail.orders[order];
            if o.status != "pending" {
                return Ok(refusal("not_pending"));
            }
            if world
                .retail
                .customers
                .iter()
                .find(|c| c.id == o.customer_id)
                .unwrap()
                .pending_cents
                != 0
            {
                return Ok(refusal("pending_payment"));
            }
            if o.sku == sku {
                return Ok(json!({"order_id":id}));
            }
            if world.retail.products[product].stock < o.quantity {
                return Ok(refusal("out_of_stock"));
            }
            let old = world
                .retail
                .products
                .iter()
                .find(|p| p.sku == o.sku)
                .unwrap();
            if old.category != world.retail.products[product].category {
                return Ok(refusal("category_mismatch"));
            }
            // Quantities represent an unallocated pending order; only the new
            // SKU is reserved. Repeating an already-applied exchange is a no-op.
            world.retail.products[product].stock -= o.quantity;
            world.retail.orders[order].sku = sku.into();
            Ok(json!({"order_id":id}))
        }
        "retail_refund" => {
            let id = required_string(args, "order_id")?;
            let order = world
                .retail
                .orders
                .iter_mut()
                .find(|o| o.id == id)
                .ok_or("unknown order")?;
            if order.status != "delivered" || order.age_days > 30 {
                return Ok(refusal("outside_return_policy"));
            }
            order.refunded_cents = order.paid_cents;
            Ok(json!({"order_id":id}))
        }
        "retail_reschedule" => {
            let id = required_string(args, "order_id")?;
            let day = args
                .get("day")
                .and_then(integer)
                .filter(|d| *d > 0)
                .ok_or("day must be a positive integer")?;
            let order = world
                .retail
                .orders
                .iter_mut()
                .find(|o| o.id == id)
                .ok_or("unknown order")?;
            if order.status != "pending" {
                return Ok(refusal("not_pending"));
            }
            if world
                .retail
                .customers
                .iter()
                .find(|c| c.id == order.customer_id)
                .unwrap()
                .pending_cents
                != 0
            {
                return Ok(refusal("pending_payment"));
            }
            order.delivery_day = day;
            Ok(json!({"order_id":id}))
        }
        "ops_service" => {
            let name = required_string(args, "name")?;
            world
                .ops
                .services
                .iter()
                .find(|s| s.name == name)
                .map(|s| json!(s))
                .ok_or_else(|| "unknown service".into())
        }
        "ops_tickets" => {
            let id = required_string(args, "service_id")?;
            Ok(json!(
                world
                    .ops
                    .tickets
                    .iter()
                    .filter(|t| t.service_id == id)
                    .map(|t| &t.id)
                    .collect::<Vec<_>>()
            ))
        }
        "ops_ticket" => {
            let id = required_string(args, "id")?;
            world
                .ops
                .tickets
                .iter()
                .find(|t| t.id == id)
                .map(|t| json!(t))
                .ok_or_else(|| "unknown ticket".into())
        }
        "ops_releases" => {
            let id = required_string(args, "service_id")?;
            Ok(json!(
                world
                    .ops
                    .releases
                    .iter()
                    .filter(|r| r.service_id == id)
                    .map(|r| &r.id)
                    .collect::<Vec<_>>()
            ))
        }
        "ops_release" => {
            let id = required_string(args, "id")?;
            world
                .ops
                .releases
                .iter()
                .find(|r| r.id == id)
                .map(|r| json!(r))
                .ok_or_else(|| "unknown release".into())
        }
        "ops_team" => {
            let id = required_string(args, "id")?;
            world
                .ops
                .teams
                .iter()
                .find(|t| t.id == id)
                .map(|t| json!(t))
                .ok_or_else(|| "unknown team".into())
        }
        "ops_deploy" => {
            let service_id = required_string(args, "service_id")?;
            let release_id = required_string(args, "release_id")?;
            let service = world
                .ops
                .services
                .iter_mut()
                .find(|s| s.id == service_id)
                .ok_or("unknown service")?;
            let release = world
                .ops
                .releases
                .iter()
                .find(|r| r.id == release_id)
                .ok_or("unknown release")?;
            if release.service_id != service_id {
                return Ok(refusal("wrong_service"));
            }
            if release.checks != "passed" {
                return Ok(refusal("checks_failed"));
            }
            if release.required_capacity > service.capacity {
                return Ok(refusal("capacity_exceeded"));
            }
            service.deployed_release = release_id.into();
            Ok(json!({"service_id":service_id}))
        }
        "ops_resolve" => {
            let id = required_string(args, "ticket_id")?;
            let index = world
                .ops
                .tickets
                .iter()
                .position(|t| t.id == id)
                .ok_or("unknown ticket")?;
            let ticket = &world.ops.tickets[index];
            if ticket.blocked_by.iter().any(|id| {
                world
                    .ops
                    .tickets
                    .iter()
                    .any(|t| &t.id == id && t.status != "resolved")
            }) {
                return Ok(refusal("dependency_open"));
            }
            let service = world
                .ops
                .services
                .iter()
                .find(|s| s.id == ticket.service_id)
                .unwrap();
            if service.deployed_release != ticket.fix_release {
                return Ok(refusal("fix_not_deployed"));
            }
            world.ops.tickets[index].status = "resolved".into();
            Ok(json!({"ticket_id":id}))
        }
        _ => Err(format!("unknown tool `{name}`")),
    }
}
fn array_schema(items: Value) -> Value {
    json!({"type":"array","items":items})
}
fn hard_definitions() -> Vec<ToolDefinition> {
    let customer = object_schema(&[
        ("id", "string"),
        ("name", "string"),
        ("pending_cents", "integer"),
    ]);
    let order = object_schema(&[
        ("id", "string"),
        ("customer_id", "string"),
        ("sku", "string"),
        ("quantity", "integer"),
        ("paid_cents", "integer"),
        ("status", "string"),
        ("age_days", "integer"),
        ("delivery_day", "integer"),
        ("refunded_cents", "integer"),
    ]);
    let product = object_schema(&[
        ("sku", "string"),
        ("category", "string"),
        ("price_cents", "integer"),
        ("stock", "integer"),
    ]);
    let service = object_schema(&[
        ("id", "string"),
        ("name", "string"),
        ("team_id", "string"),
        ("deployed_release", "string"),
        ("capacity", "integer"),
    ]);
    let mut ticket = object_schema(&[
        ("id", "string"),
        ("service_id", "string"),
        ("severity", "integer"),
        ("affected_users", "integer"),
        ("status", "string"),
        ("fix_release", "string"),
        ("blocked_by", "array"),
    ]);
    ticket["properties"]["blocked_by"] = array_schema(json!({"type":"string"}));
    let release = object_schema(&[
        ("id", "string"),
        ("service_id", "string"),
        ("version", "integer"),
        ("required_capacity", "integer"),
        ("checks", "string"),
    ]);
    let team = object_schema(&[
        ("id", "string"),
        ("primary", "string"),
        ("backup", "string"),
        ("primary_available", "boolean"),
    ]);
    let ids = array_schema(json!({"type":"string"}));
    let receipt = |field| json!({"oneOf":[object_schema(&[(field,"string")]),{"type":"object","properties":{"error":object_schema(&[("code","string")])},"required":["error"],"additionalProperties":false}]});
    vec![
        definition(
            "retail_customer",
            ["retail"],
            "customer",
            "Find a customer by exact name.",
            object_schema(&[("name", "string")]),
            customer,
        ),
        definition(
            "retail_orders",
            ["retail"],
            "orders",
            "List order IDs belonging to a customer; details require order.",
            object_schema(&[("customer_id", "string")]),
            ids.clone(),
        ),
        definition(
            "retail_order",
            ["retail"],
            "order",
            "Read current order including quantity, paid/refunded cents, status, age_days since delivery, and delivery_day (integer day of month).",
            object_schema(&[("id", "string")]),
            order,
        ),
        definition(
            "retail_products",
            ["retail"],
            "products",
            "List products in a category, with price_cents and available stock units.",
            object_schema(&[("category", "string")]),
            array_schema(product.clone()),
        ),
        definition(
            "retail_product",
            ["retail"],
            "product",
            "Read a product's category, price_cents and stock units.",
            object_schema(&[("sku", "string")]),
            product,
        ),
        definition(
            "retail_exchange",
            ["retail"],
            "exchange",
            "Replace a pending order's SKU within the same category and reserve its quantity from stock. Pending payment blocks exchange. Returns order_id or structured error.code (out_of_stock, pending_payment, not_pending, category_mismatch); refusals do not mutate. Original pending orders are unallocated. Read order to verify.",
            object_schema(&[("order_id", "string"), ("sku", "string")]),
            receipt("order_id"),
        ),
        definition(
            "retail_refund",
            ["retail"],
            "refund",
            "Refund full paid_cents for delivered orders aged at most 30 days; idempotent. Returns order_id or error.code outside_return_policy, with no mutation on refusal. Read order to verify refunded_cents.",
            object_schema(&[("order_id", "string")]),
            receipt("order_id"),
        ),
        definition(
            "retail_reschedule",
            ["retail"],
            "reschedule",
            "Set a pending order's delivery_day to a positive integer, only when customer pending_cents is zero. Returns order_id or error.code pending_payment/not_pending; refusals do not mutate. Read order to verify.",
            object_schema(&[("order_id", "string"), ("day", "integer")]),
            receipt("order_id"),
        ),
        definition(
            "ops_service",
            ["ops"],
            "service",
            "Find current service details by exact name, including deployed_release, team_id and capacity.",
            object_schema(&[("name", "string")]),
            service,
        ),
        definition(
            "ops_tickets",
            ["ops"],
            "tickets",
            "List ticket IDs for a service; use ticket for details.",
            object_schema(&[("service_id", "string")]),
            ids.clone(),
        ),
        definition(
            "ops_ticket",
            ["ops"],
            "ticket",
            "Read ticket: severity 1 is highest, affected_users, open/resolved status, fix_release, and blocked_by ticket IDs.",
            object_schema(&[("id", "string")]),
            ticket,
        ),
        definition(
            "ops_releases",
            ["ops"],
            "releases",
            "List release IDs for a service; use release for details.",
            object_schema(&[("service_id", "string")]),
            ids,
        ),
        definition(
            "ops_release",
            ["ops"],
            "release",
            "Read release version (larger is newer), required_capacity and checks (passed/failed).",
            object_schema(&[("id", "string")]),
            release,
        ),
        definition(
            "ops_team",
            ["ops"],
            "team",
            "Read team primary/backup email addresses and primary_available.",
            object_schema(&[("id", "string")]),
            team,
        ),
        definition(
            "ops_deploy",
            ["ops"],
            "deploy",
            "Deploy a release with passed checks within service capacity. Returns service_id or structured error.code wrong_service/checks_failed/capacity_exceeded without mutation. Read service to verify.",
            object_schema(&[("service_id", "string"), ("release_id", "string")]),
            receipt("service_id"),
        ),
        definition(
            "ops_resolve",
            ["ops"],
            "resolve",
            "Resolve ticket only with its exact fix_release deployed and all blocked_by tickets resolved. Returns ticket_id or structured error.code dependency_open/fix_not_deployed without mutation. Read ticket to verify.",
            object_schema(&[("ticket_id", "string")]),
            receipt("ticket_id"),
        ),
    ]
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    #[test]
    fn catalogs_are_identical_across_channels_apart_from_submit() {
        for (catalog, count) in [(Catalog::Easy, 7), (Catalog::Retail, 8), (Catalog::Ops, 8)] {
            let mut seed = World::seeded();
            seed.catalog = catalog;
            let world = SharedWorld::new(seed);
            let rlm = world.provider().tool_manifests();
            let mut standard = world.standard_provider().tool_manifests();
            assert_eq!(rlm.len(), count);
            assert_eq!(standard.pop().unwrap().name, "submit");
            assert_eq!(rlm, standard);
            for d in definitions(catalog) {
                assert_eq!(
                    d.contract.input_schema.canonical()["additionalProperties"],
                    false
                );
                assert!(d.contract.output_schema.canonical().is_object());
            }
        }
    }
}
