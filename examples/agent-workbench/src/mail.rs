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

use crate::MAIL_RECEIVED_SOURCE_TYPE;
use async_trait::async_trait;
use lash::tools::{
    EmitTriggerIntent, LashlangToolBinding, ToolAttemptOutcome, ToolCall, ToolContract,
    ToolDefinition, ToolDefinitionLashlangExt, ToolIntent, ToolIntents, ToolManifest, ToolOutcome,
    ToolOutcomeDone, ToolProvider, ToolRetryPolicy,
};
use lash::triggers::{TriggerOccurrenceRequest, empty_trigger_source_key};
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

/// One delivered mock message, used to build the `mail.received` trigger occurrence.
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
    sent_by_replay_key: Arc<Mutex<BTreeMap<String, DeliveredMail>>>,
}

impl MailWorld {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add an account from a human-entered name. Returns the created summary or
    /// a human-readable error (empty/duplicate/invalid name).
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

    /// Remove an account and its messages. Returns an error if unknown.
    /// Drop every account and inbox: the workbench reset wipes the mail
    /// world along with the chat session.
    pub(crate) fn clear(&self) {
        self.inner.write_recover().clear();
        self.sent_by_replay_key.lock_recover().clear();
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

    /// Remove a single message by id.
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
        replay_key: &str,
        slug: &str,
        args: &Value,
    ) -> Result<DeliveredMail, String> {
        let mut sent = self.sent_by_replay_key.lock_recover();
        if let Some(delivered) = sent.get(replay_key) {
            return Ok(delivered.clone());
        }
        let delivered = self.op_send(slug, args)?;
        sent.insert(replay_key.to_string(), delivered.clone());
        Ok(delivered)
    }

    /// Commit one send and declare the `mail.received` emission it owes.
    ///
    /// The receipt and the declaration are produced together: there is no
    /// point between them at which the row is durable and the emission is
    /// merely hoped for. The caller returns both in one attempt outcome and
    /// the intent executor emits after that outcome commits.
    pub(crate) fn send_with_trigger(
        &self,
        replay_key: &str,
        session_id: &str,
        slug: &str,
        args: &Value,
    ) -> Result<(Value, ToolIntent), String> {
        let delivered = self.op_send_once(replay_key, slug, args)?;
        let payload = serde_json::to_value(&delivered.delivery).map_err(|err| err.to_string())?;
        let source_key =
            empty_trigger_source_key(MAIL_RECEIVED_SOURCE_TYPE).map_err(|err| err.to_string())?;
        // Both halves of this key are stable under redrive: the tool-call
        // replay key and the memoized message id. The trigger store therefore
        // ingests one occurrence however often the declaration is re-executed.
        let idempotency_key = format!("{replay_key}:mail.received:{}", delivered.message.id);
        let intent = ToolIntent::EmitTrigger(EmitTriggerIntent {
            session_id: session_id.to_string(),
            request: TriggerOccurrenceRequest::new(
                MAIL_RECEIVED_SOURCE_TYPE,
                source_key,
                payload,
                idempotency_key,
            )
            .with_source(json!({})),
        });
        Ok((
            json!({ "account": slug, "id": delivered.message.id }),
            intent,
        ))
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

/// Turn a human account name into a Lashlang module-path segment
/// (`[a-z][a-z0-9_]*`). Returns `None` if nothing usable remains.
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

fn definition_for(slug: &str, display_name: &str, operation: &str) -> ToolDefinition {
    let name = tool_name(slug, operation);
    let (input_schema, summary) = operation_schemas(operation);
    let description = format!("{summary} Account `{display_name}` (inbox.{slug}).");
    let retry_policy = match operation {
        "list" => ToolRetryPolicy::safe(3, 25, 250),
        "delete" => ToolRetryPolicy::Idempotent {
            max_attempts: 3,
            base_delay_ms: 25,
            max_delay_ms: 250,
        },
        _ => ToolRetryPolicy::Never,
    };
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        input_schema,
        json!({ "type": "object" }),
    )
    .with_retry_policy(retry_policy)
    .with_lashlang_binding(
        LashlangToolBinding::new(["inbox", slug], operation).with_authority_type("Inbox"),
    )
}

/// Dynamic provider: one `Inbox` authority per account, three operations each.
pub(crate) struct MockMailProvider {
    world: MailWorld,
}

impl MockMailProvider {
    pub(crate) fn new(world: MailWorld) -> Self {
        Self { world }
    }

    /// Build the live tool definitions from the current account set.
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

    /// Resolve a tool name back to (slug, operation) by parsing it. Only used
    /// to route execution; resolution (`tool_manifests`/`resolve_contract`)
    /// covers live accounts exclusively. A persisted session that references
    /// a removed account's tools restores anyway — lash-core orphans them
    /// as non-members and rebinds when the account is re-added.
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

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let Some((slug, operation)) = self.route(call.name) else {
            return ToolOutcome::err_fmt(format_args!("unknown inbox tool `{}`", call.name));
        };
        if operation == "send" {
            // Sending commits a durable row and owes a `mail.received`
            // emission. Only the leaf attempt signature can pair the two, so
            // this legacy route refuses rather than committing half of it.
            return ToolOutcome::err_fmt(format_args!(
                "inbox tool `{}` requires the leaf AttemptContext signature",
                call.name
            ));
        }
        let result = match operation {
            "list" => self.world.op_list(&slug, call.args),
            "delete" => self.world.op_delete(&slug, call.args),
            other => Err(format!("unsupported inbox operation `{other}`")),
        };
        match result {
            Ok(value) => ToolOutcome::ok(value),
            Err(message) => ToolOutcome::err_fmt(message),
        }
    }

    async fn execute_attempt(&self, call: lash::tools::ToolCall<'_>) -> ToolAttemptOutcome {
        let Some((slug, operation)) = self.route(call.name) else {
            return done(ToolOutcome::err_fmt(format_args!(
                "unknown inbox tool `{}`",
                call.name
            )));
        };
        if operation != "send" {
            // Reads own no declaration; they are pure attempt bodies and run
            // against the same sealed attempt context.
            return done(self.execute(call).await);
        }
        let Some(replay_key) = call.context.replay_key() else {
            return done(ToolOutcome::err_fmt("mail send requires a replay key"));
        };
        match self
            .world
            .send_with_trigger(replay_key, call.context.session_id(), &slug, call.args)
        {
            // One attempt outcome carries the committed row and the declared
            // emission. The row can no longer become durable without the
            // `mail.received` occurrence that follows it.
            Ok((receipt, intent)) => ToolAttemptOutcome::done(
                ToolOutcomeDone::ok(receipt),
                ToolIntents::v1(vec![intent]),
            ),
            Err(message) => done(ToolOutcome::err_fmt(message)),
        }
    }
}

fn done(result: ToolOutcome) -> ToolAttemptOutcome {
    match result {
        ToolOutcome::Done(output) => {
            ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::from_output(*output))
        }
        ToolOutcome::Pending(pending) => ToolAttemptOutcome::pending(pending),
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

    /// The route that made the partial effect possible is gone. `send` used to
    /// run on the legacy signature, which commits the row and then emits, so a
    /// failure in between left a durable delivery whose `mail.received`
    /// occurrence never happened and whose concierge never ran. Sending now
    /// exists only on the leaf attempt signature, which pairs the row with its
    /// declaration; the legacy route refuses instead of committing half.
    #[tokio::test]
    async fn send_exists_only_on_the_attempt_route_that_pairs_row_and_emission() {
        let world = MailWorld::new();
        world.add_account("Work").expect("add work");
        let provider = MockMailProvider::new(world.clone());
        let args = json!({ "title": "Contract", "text": "Please review." });

        let refused = provider
            .execute(ToolCall {
                name: "inbox__work__send",
                args: &args,
                context: &lash_core::testing::mock_attempt_context(),
            })
            .await;
        let ToolOutcome::Done(output) = refused else {
            panic!("the legacy route must settle")
        };
        let message = serde_json::to_string(&output).expect("serialize the refusal");
        assert!(
            message.contains("requires the leaf AttemptContext signature"),
            "the legacy route must refuse rather than commit the row: {message}"
        );
        assert_eq!(
            world.inbox("work").expect("work inbox").len(),
            0,
            "a refused legacy send commits nothing"
        );
    }

    #[test]
    fn send_commits_the_row_and_declares_its_emission_in_one_outcome() {
        let world = MailWorld::new();
        world.add_account("Work").expect("add work");
        let args = json!({ "title": "Contract", "text": "Please review." });

        let (receipt, intent) = world
            .send_with_trigger("turn-1:call-1", "session-1", "work", &args)
            .expect("send declares its emission");

        assert_eq!(receipt, json!({ "account": "work", "id": "work-1" }));
        assert_eq!(world.inbox("work").expect("work inbox").len(), 1);
        assert_eq!(intent.kind(), lash_core::ToolIntentKind::EmitTrigger);
        let ToolIntent::EmitTrigger(declared) = intent else {
            panic!("inbox send must declare a trigger emission")
        };
        assert_eq!(declared.session_id, "session-1");
        assert_eq!(declared.request.source_type, MAIL_RECEIVED_SOURCE_TYPE);
        assert_eq!(
            declared.request.idempotency_key,
            "turn-1:call-1:mail.received:work-1"
        );

        let (replayed_receipt, replayed_intent) = world
            .send_with_trigger("turn-1:call-1", "session-1", "work", &args)
            .expect("redriving the same call re-declares the same emission");
        let ToolIntent::EmitTrigger(replayed) = replayed_intent else {
            panic!("the redrive must declare a trigger emission")
        };
        assert_eq!(replayed_receipt, receipt);
        assert_eq!(
            replayed.request.idempotency_key,
            declared.request.idempotency_key
        );
        assert_eq!(
            world.inbox("work").expect("work inbox").len(),
            1,
            "the redrive neither redelivers nor forks the occurrence key"
        );
    }

    #[test]
    fn send_replay_returns_the_stable_receipt_without_redelivery() {
        let world = MailWorld::new();
        world.add_account("Work").expect("add work");
        let args = json!({ "title": "Contract", "text": "Please review." });

        let first = world
            .op_send_once("turn-1:call-1", "work", &args)
            .expect("first send");
        let replay = world
            .op_send_once("turn-1:call-1", "work", &args)
            .expect("replayed send");

        assert_eq!(first.message.id, "work-1");
        assert_eq!(replay.message.id, first.message.id);
        assert_eq!(world.inbox("work").expect("work inbox").len(), 1);
    }

    #[test]
    fn provider_exposes_authority_per_account() {
        let world = MailWorld::new();
        world.add_account("Work").expect("add work");
        world.add_account("Personal").expect("add personal");
        let provider = MockMailProvider::new(world);

        let manifests = provider.tool_manifests();
        let names: Vec<String> = manifests
            .iter()
            .map(|manifest| manifest.name.clone())
            .collect();
        assert!(names.contains(&"inbox__work__send".to_string()));
        assert!(names.contains(&"inbox__personal__delete".to_string()));
        assert_eq!(names.len(), 6);

        let send = manifests
            .iter()
            .find(|manifest| manifest.name == "inbox__work__send")
            .expect("resolve non-retryable send manifest");
        assert_eq!(send.retry_policy, ToolRetryPolicy::Never);
        let list = manifests
            .iter()
            .find(|manifest| manifest.name == "inbox__work__list")
            .expect("resolve safely retryable list manifest");
        assert_eq!(
            list.retry_policy,
            ToolRetryPolicy::Safe {
                max_attempts: 3,
                base_delay_ms: 25,
                max_delay_ms: 250,
            }
        );
        let delete = manifests
            .iter()
            .find(|manifest| manifest.name == "inbox__work__delete")
            .expect("resolve idempotent delete manifest");
        assert_eq!(
            delete.retry_policy,
            ToolRetryPolicy::Idempotent {
                max_attempts: 3,
                base_delay_ms: 25,
                max_delay_ms: 250,
            }
        );
        assert!(provider.resolve_contract("inbox__work__delete").is_some());

        let manifest = provider
            .tool_manifests()
            .into_iter()
            .find(|manifest| manifest.name == "inbox__work__send")
            .expect("work send manifest");
        let surface = lash_lashlang_runtime::required_tool_lashlang_binding(&manifest)
            .expect("work send binding")
            .executable_for(&manifest.name)
            .expect("work send surface");
        assert_eq!(surface.call_path(), "inbox.work.send");
        assert_eq!(surface.authority_type, "Inbox");

        assert_eq!(
            provider.route("inbox__personal__delete"),
            Some(("personal".to_string(), "delete"))
        );
    }
}
