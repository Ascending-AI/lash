//! Mocked multi-account inbox world for the workbench demo.
//!
//! The host owns a small in-memory set of named inboxes. Each is projected into
//! the RLM Lashlang host environment as a typed module authority of type `Inbox` at
//! module path `inbox.<slug>`, exposing three operations:
//!
//! - `send({ title, text })` — add a message to that inbox
//! - `list({})` — list the messages in that inbox
//! - `delete({ id })` — remove a message by id
//!
//! Because every account shares the `Inbox` authority type, a single
//! account-parametric process such as `process triage(box: Inbox) { ... }` can
//! be started against any account (`start triage(box: inbox.work)`), which is
//! the point of the multi-account showcase.
//!
//! Accounts are added at runtime from the UI. The provider reads the live
//! account set in [`MockMailProvider::definitions`], and route handlers enqueue
//! a durable tool-catalog refresh so the next opened turn sees the updated
//! `inbox.<slug>` authority set.

use lash::sync::{MutexExt, RwLockExt};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
};

use async_trait::async_trait;
use lash::tools::{
    ExecutionPolicy, ToolAttemptOutcome, ToolBinding, ToolCall, ToolContract, ToolDefinition,
    ToolDefinitionBindingExt, ToolManifest, ToolOutcome, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Operations every inbox authority exposes. Order is the surface order.
const MAIL_OPERATIONS: [&str; 3] = ["send", "list", "delete"];

/// One stored message: just a title and body text.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MailMessage {
    pub id: String,
    pub title: String,
    pub text: String,
}

impl MailMessage {
    fn value(&self) -> Value {
        json!({ "id": self.id, "title": self.title, "text": self.text })
    }
}

/// One delivered mock message: the event a `mail` trigger delivers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MailDelivery {
    pub account: String,
    pub title: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeliveredMail {
    pub message: MailMessage,
    pub delivery: MailDelivery,
}

struct Account {
    slug: String,
    display_name: String,
    messages: Vec<MailMessage>,
    next_id: u64,
}

impl Account {
    fn append(&mut self, title: &str, text: &str) -> MailMessage {
        let id = format!("{}-{}", self.slug, self.next_id);
        self.next_id += 1;
        let message = MailMessage {
            id,
            title: non_empty(title).unwrap_or("(no title)").to_string(),
            text: text.trim().to_string(),
        };
        self.messages.push(message.clone());
        message
    }

    fn summary(&self) -> AccountSummary {
        AccountSummary {
            slug: self.slug.clone(),
            display_name: self.display_name.clone(),
            authority: format!("inbox.{}", self.slug),
            total: self.messages.len(),
        }
    }
}

/// UI-facing account row.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AccountSummary {
    pub slug: String,
    pub display_name: String,
    /// Lashlang authority path the agent calls, e.g. `inbox.work`.
    pub authority: String,
    pub total: usize,
}

/// The shared, mutable mock inbox world. Cloneable handle around the store.
#[derive(Clone, Default)]
pub(crate) struct MailWorld {
    inner: Arc<RwLock<Vec<Account>>>,
    sent_by_call_id: Arc<Mutex<BTreeMap<String, DeliveredMail>>>,
}

impl MailWorld {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn add_account(&self, name: &str) -> Result<AccountSummary, String> {
        let display_name = name.trim().to_string();
        if display_name.is_empty() {
            return Err("account name is required".to_string());
        }
        let slug = slugify(&display_name)
            .ok_or_else(|| "account name must contain a letter or digit".to_string())?;
        let mut accounts = self.inner.write_recover();
        if accounts.iter().any(|account| account.slug == slug) {
            return Err(format!("account `{slug}` already exists"));
        }
        let account = Account {
            slug,
            display_name,
            messages: Vec::new(),
            next_id: 1,
        };
        let summary = account.summary();
        accounts.push(account);
        Ok(summary)
    }

    pub(crate) fn account_summaries(&self) -> Vec<AccountSummary> {
        self.inner
            .read_recover()
            .iter()
            .map(Account::summary)
            .collect()
    }

    pub(crate) fn clear(&self) {
        self.inner.write_recover().clear();
        self.sent_by_call_id.lock_recover().clear();
    }

    pub(crate) fn remove_account(&self, slug: &str) -> Result<(), String> {
        let mut accounts = self.inner.write_recover();
        let before = accounts.len();
        accounts.retain(|account| account.slug != slug);
        if accounts.len() == before {
            return Err(format!("unknown account `{slug}`"));
        }
        Ok(())
    }

    /// Deliver a message into an account. UI injects and agent `send` tools both
    /// use this path so storage, ids, and `mail.received` payloads match.
    pub(crate) fn deliver(
        &self,
        slug: &str,
        title: &str,
        text: &str,
    ) -> Result<DeliveredMail, String> {
        let mut accounts = self.inner.write_recover();
        let account = find_mut(&mut accounts, slug)?;
        let message = account.append(title, text);
        Ok(DeliveredMail {
            delivery: MailDelivery {
                account: slug.to_string(),
                title: message.title.clone(),
                text: message.text.clone(),
            },
            message,
        })
    }

    /// Messages for an account, newest first (for the UI).
    pub(crate) fn inbox(&self, slug: &str) -> Result<Vec<MailMessage>, String> {
        let accounts = self.inner.read_recover();
        let account = find(&accounts, slug)?;
        let mut messages = account.messages.clone();
        messages.reverse();
        Ok(messages)
    }

    pub(crate) fn remove_message(&self, slug: &str, id: &str) -> Result<(), String> {
        let mut accounts = self.inner.write_recover();
        let account = find_mut(&mut accounts, slug)?;
        let before = account.messages.len();
        account.messages.retain(|message| message.id != id);
        if account.messages.len() == before {
            return Err(format!("no message `{id}` in `{slug}`"));
        }
        Ok(())
    }

    // --- Tool operation backends (called from MockMailProvider::execute) ---

    fn op_send(&self, slug: &str, args: &Value) -> Result<DeliveredMail, String> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = args.get("text").and_then(Value::as_str).unwrap_or_default();
        self.deliver(slug, title, text)
    }

    /// Deliver at most once for one stable tool-call replay identity.
    ///
    /// This in-memory map is intentionally only a workbench mock. A production
    /// host must record the replay key and receipt atomically in the same
    /// durable store as the external side effect, and prune completed entries
    /// according to that store's retention policy.
    fn op_send_once(
        &self,
        call_id: &str,
        slug: &str,
        args: &Value,
    ) -> Result<DeliveredMail, String> {
        let mut sent = self.sent_by_call_id.lock_recover();
        if let Some(delivered) = sent.get(call_id) {
            return Ok(delivered.clone());
        }
        let delivered = self.op_send(slug, args)?;
        sent.insert(call_id.to_string(), delivered.clone());
        Ok(delivered)
    }

    /// Send under `call_id` and record its arrival as the `mail` source's
    /// occurrence `mail:{call_id}`. Both halves are keyed by the call, so a
    /// redriven call sends one message and records one occurrence.
    pub(crate) fn send_and_fire(
        &self,
        triggers: &crate::host_triggers::HostTriggers,
        call_id: &str,
        slug: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let delivered = self.op_send_once(call_id, slug, args)?;
        triggers
            .fire_mail(&format!("mail:{call_id}"), &delivered.delivery)
            .map_err(|error| error.to_string())?;
        Ok(json!({ "account": slug, "id": delivered.message.id }))
    }

    fn op_list(&self, slug: &str, _args: &Value) -> Result<Value, String> {
        let accounts = self.inner.read_recover();
        let account = find(&accounts, slug)?;
        let mut messages: Vec<&MailMessage> = account.messages.iter().collect();
        messages.reverse();
        Ok(json!({
            "account": slug,
            "messages": messages.iter().map(|m| m.value()).collect::<Vec<_>>(),
        }))
    }

    fn op_delete(&self, slug: &str, args: &Value) -> Result<Value, String> {
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "delete requires an `id`".to_string())?;
        self.remove_message(slug, id)?;
        Ok(json!({ "account": slug, "id": id, "deleted": true }))
    }
}

fn find<'a>(accounts: &'a [Account], slug: &str) -> Result<&'a Account, String> {
    accounts
        .iter()
        .find(|account| account.slug == slug)
        .ok_or_else(|| format!("unknown account `{slug}`"))
}

fn find_mut<'a>(accounts: &'a mut [Account], slug: &str) -> Result<&'a mut Account, String> {
    accounts
        .iter_mut()
        .find(|account| account.slug == slug)
        .ok_or_else(|| format!("unknown account `{slug}`"))
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Turn a human account name into a Lashlang module-path segment (`[a-z][a-z0-9_]*`).
pub(crate) fn slugify(name: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_underscore = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !slug.is_empty() && !last_underscore {
            slug.push('_');
            last_underscore = true;
        }
    }
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        return None;
    }
    if slug.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        slug.insert(0, 'a');
    }
    Some(slug)
}

/// Tool name carrying the account slug and operation, e.g.
/// `inbox__work__send`. A double underscore separates the fixed parts so the
/// slug (which may itself contain single underscores) stays unambiguous.
fn tool_name(slug: &str, operation: &str) -> String {
    format!("inbox__{slug}__{operation}")
}

fn operation_schemas(operation: &str) -> (Value, &'static str) {
    match operation {
        "send" => (
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
            "Add a message to this inbox.",
        ),
        "list" => (
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            "List the messages in this inbox.",
        ),
        "delete" => (
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
            "Delete a message from this inbox by id.",
        ),
        _ => (json!({ "type": "object" }), ""),
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn definition_for(slug: &str, display_name: &str, operation: &str) -> ToolDefinition {
    let name = tool_name(slug, operation);
    let (input_schema, summary) = operation_schemas(operation);
    let description = format!("{summary} Account `{display_name}` (inbox.{slug}).");
    // Deleting a message twice leaves the same mailbox, so a reported failure
    // may retry a delete as safely as a list.
    let execution_policy = match operation {
        "list" | "delete" => ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(3).expect("nonzero attempt bound"),
            25,
            250,
        ),
        _ => ExecutionPolicy::Once,
    };
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(execution_policy)
    .with_tool_binding(ToolBinding::new(["inbox", slug], operation).with_authority_type("Inbox"))
}

/// Dynamic provider: one `Inbox` authority per account, three operations each.
pub(crate) struct MockMailProvider {
    world: MailWorld,
    triggers: crate::host_triggers::HostTriggers,
}

impl MockMailProvider {
    pub(crate) fn new(world: MailWorld, triggers: crate::host_triggers::HostTriggers) -> Self {
        Self { world, triggers }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        let summaries = self.world.account_summaries();
        let mut defs = Vec::with_capacity(summaries.len() * MAIL_OPERATIONS.len());
        for summary in summaries {
            for operation in MAIL_OPERATIONS {
                defs.push(definition_for(
                    &summary.slug,
                    &summary.display_name,
                    operation,
                ));
            }
        }
        defs
    }

    /// Only used to route execution; resolution (`tool_manifests`/`resolve_contract`) covers
    /// live accounts exclusively.
    /// A persisted session that references a removed account's tools restores anyway —
    /// lash-core orphans them as non-members and rebinds when the account is re-added.
    fn route(&self, name: &str) -> Option<(String, &'static str)> {
        let rest = name.strip_prefix("inbox__")?;
        for operation in MAIL_OPERATIONS {
            if let Some(slug) = rest.strip_suffix(&format!("__{operation}"))
                && !slug.is_empty()
            {
                return Some((slug.to_string(), operation));
            }
        }
        None
    }
}

#[async_trait]
impl ToolProvider for MockMailProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        self.definitions()
            .iter()
            .map(ToolDefinition::manifest)
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        let (slug, operation) = self.route(name)?;
        let summary = self
            .world
            .account_summaries()
            .into_iter()
            .find(|summary| summary.slug == slug)?;
        Some(Arc::new(
            definition_for(&slug, &summary.display_name, operation).contract(),
        ))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let Some((slug, operation)) = self.route(call.name()) else {
            return ToolOutcome::err_fmt(format_args!("unknown inbox tool `{}`", call.name()))
                .into();
        };
        if operation != "send" {
            let result = match operation {
                "list" => self.world.op_list(&slug, call.args),
                "delete" => self.world.op_delete(&slug, call.args),
                other => Err(format!("unsupported inbox operation `{other}`")),
            };
            return match result {
                Ok(value) => ToolOutcome::ok(value).into(),
                Err(message) => ToolOutcome::err_fmt(message).into(),
            };
        }
        // The call's `ToolCallId` is its idempotency key: the same across a
        // crash replay and a reported-failure retry.
        match self.world.send_and_fire(
            &self.triggers,
            call.context.call_id().as_str(),
            &slug,
            call.args,
        ) {
            Ok(receipt) => ToolOutcome::ok(receipt).into(),
            Err(message) => ToolOutcome::err_fmt(message).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes_names() {
        assert_eq!(slugify("Work").as_deref(), Some("work"));
        assert_eq!(slugify("Personal Mail").as_deref(), Some("personal_mail"));
        assert_eq!(slugify("  spaced  out  ").as_deref(), Some("spaced_out"));
        assert_eq!(slugify("2024 inbox").as_deref(), Some("a2024_inbox"));
        assert_eq!(slugify("!!!"), None);
    }

    #[test]
    fn add_account_rejects_duplicates_and_blanks() {
        let world = MailWorld::new();
        assert!(world.add_account("Work").is_ok());
        assert!(world.add_account("work").is_err());
        assert!(world.add_account("   ").is_err());
        assert_eq!(world.account_summaries().len(), 1);
    }

    #[test]
    fn send_list_and_delete() {
        let world = MailWorld::new();
        world.add_account("Work").expect("add work");

        let sent = world
            .op_send(
                "work",
                &json!({ "title": "Contract", "text": "Please review." }),
            )
            .expect("send");
        let id = sent.message.id.clone();
        assert_eq!(world.account_summaries()[0].total, 1);
        assert_eq!(world.account_summaries()[0].authority, "inbox.work");

        let listed = world.op_list("work", &json!({})).expect("list");
        let messages = listed["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["title"], json!("Contract"));
        assert_eq!(messages[0]["text"], json!("Please review."));

        world
            .op_delete("work", &json!({ "id": id }))
            .expect("delete");
        assert_eq!(world.account_summaries()[0].total, 0);
    }
}
