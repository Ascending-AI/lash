use super::*;
use lash::SessionId;

pub(crate) struct WorkbenchPluginFactory {
    pub(crate) mail_world: mail::MailWorld,
    pub(crate) config_changes: WorkbenchConfigChanges,
    pub(crate) context_budget: WorkbenchContextBudget,
    pub(crate) deferred_tools: deferred_tools::WorkbenchDeferredTools,
    pub(crate) approvals: approvals::WorkbenchApprovals,
}

impl WorkbenchPluginFactory {
    #[expect(
        clippy::expect_used,
        reason = "SQLite memory stores open a fresh `Connection::open_in_memory` with no on-disk \
                  path to collide, so their open cannot fail here"
    )]
    pub(crate) fn new() -> Self {
        Self {
            mail_world: mail::MailWorld::new(),
            config_changes: WorkbenchConfigChanges::default(),
            context_budget: WorkbenchContextBudget::default(),
            deferred_tools: deferred_tools::WorkbenchDeferredTools::in_memory()
                .expect("open in-memory deferred-tool grants"),
            approvals: approvals::WorkbenchApprovals::in_memory()
                .expect("open in-memory approval ledger"),
        }
    }

    pub(crate) fn with_mail_world(mut self, mail_world: mail::MailWorld) -> Self {
        self.mail_world = mail_world;
        self
    }

    pub(crate) fn with_deferred_tools(
        mut self,
        deferred_tools: deferred_tools::WorkbenchDeferredTools,
    ) -> Self {
        self.deferred_tools = deferred_tools;
        self
    }

    pub(crate) fn with_approvals(mut self, approvals: approvals::WorkbenchApprovals) -> Self {
        self.approvals = approvals;
        self
    }
}

impl Default for WorkbenchPluginFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginFactory for WorkbenchPluginFactory {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

    #[expect(
        clippy::expect_used,
        reason = "the contribution wraps statically defined abilities and resources that \
                  serialize by construction"
    )]
    fn extension_contributions(&self) -> Vec<lash::plugins::PluginExtensionContribution> {
        vec![
            lash::plugins::PluginExtensionContribution::new(
                lash::rlm::LASHLANG_SURFACE_EXTENSION_ID,
                lash::rlm::LashlangSurfaceContribution::new(
                    workbench_lashlang_abilities(),
                    lash::rlm::LashlangLanguageFeatures::default(),
                    workbench_lashlang_resources(),
                ),
            )
            .expect("workbench lashlang surface serializes"),
        ]
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(WorkbenchSessionPlugin {
            mail_world: self.mail_world.clone(),
            config_changes: self.config_changes.clone(),
            context_budget: self.context_budget.clone(),
            deferred_tools: self.deferred_tools.clone(),
            approvals: self.approvals.clone(),
        }))
    }
}

impl lash::plugins::PluginDefinition for WorkbenchPluginFactory {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial("agent_workbench")
    }
}

pub(crate) struct WorkbenchSessionPlugin {
    pub(crate) mail_world: mail::MailWorld,
    pub(crate) config_changes: WorkbenchConfigChanges,
    pub(crate) context_budget: WorkbenchContextBudget,
    pub(crate) deferred_tools: deferred_tools::WorkbenchDeferredTools,
    pub(crate) approvals: approvals::WorkbenchApprovals,
}

impl SessionPlugin for WorkbenchSessionPlugin {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.triggers().declare(TriggerEvent::new(
            BUTTON_TRIGGER_RESOURCE,
            BUTTON_TRIGGER_ALIAS,
            BUTTON_TRIGGER_EVENT,
            button_trigger_payload_schema(),
        ))?;
        reg.triggers().declare(TriggerEvent::new(
            MAIL_EVENT_RESOURCE,
            MAIL_EVENT_ALIAS,
            MAIL_EVENT_EVENT,
            mail_received_payload_schema(),
        ))?;
        reg.tools()
            .provider(self.deferred_tools.search_provider())?;
        reg.tools()
            .provider(self.deferred_tools.execution_provider())?;
        reg.tools().provider(self.approvals.provider())?;
        reg.tools().provider(Arc::new(mail::MockMailProvider::new(
            self.mail_world.clone(),
        )))?;
        reg.context()
            .prepare_turn(0, Arc::new(self.context_budget.clone()))?;
        register_workbench_prompt_sections(reg, &self.mail_world)?;
        reg.turn().after(
            lash::hook_key!("write-back-notes"),
            Arc::new(|ctx| {
                Box::pin(async move {
                    let snapshot = ctx.sessions.snapshot_current().await?;
                    let state = lash::persistence::SessionReadView::from_snapshot(&snapshot);
                    Ok(lash::plugins::AfterTurnContributions {
                        session: lash::plugins::SessionContributions {
                            graph_appends: workbench_derived_note(&state).into_iter().collect(),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                })
            }),
        )?;
        let config_changes = self.config_changes.clone();
        reg.session().on_event(
            lash::hook_key!("observe"),
            Arc::new(move |event| {
                let config_changes = config_changes.clone();
                Box::pin(async move {
                    if let lash::plugins::PluginLifecycleEvent::SessionConfigChanged(ctx) = event {
                        config_changes.observe(&ctx).await?;
                    }
                    Ok(())
                })
            }),
        )?;
        Ok(())
    }
}

/// The workbench's per-turn context budgeter: a [`TurnContextTransform`] that
/// reads the prepared context the runtime assembled and states the shape of it
/// back to the model.
///
/// This is the seam a rolling-context strategy uses. It is deliberately
/// read-mostly here: durable compaction is an explicit Agent Frame transition,
/// not a rewrite of the prepared context, so the transform annotates rather
/// than truncates. The annotation is load-bearing evidence — it is rendered
/// into the prompt the provider actually receives, so a harness can prove the
/// transform ran against the real assembled context and not a fabricated one.
#[derive(Clone, Default)]
pub(crate) struct WorkbenchContextBudget {
    pub(crate) observed: Arc<Mutex<Option<WorkbenchContextObservation>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkbenchContextObservation {
    pub(crate) session_id: SessionId,
    pub(crate) message_count: usize,
    pub(crate) tool_provider_count: usize,
    pub(crate) committed_message_count: usize,
    pub(crate) max_context_tokens: Option<usize>,
    pub(crate) last_prompt_context_tokens: Option<usize>,
}

impl WorkbenchContextBudget {}

#[async_trait]
impl lash::plugins::TurnContextTransform for WorkbenchContextBudget {
    fn id(&self) -> &'static str {
        "agent_workbench.context_budget"
    }

    async fn transform(
        &self,
        ctx: &lash::plugins::TurnTransformContext<'_>,
        input: lash::plugins::PreparedContext,
    ) -> Result<lash::plugins::PreparedContext, lash::plugins::ContextError> {
        let observation = WorkbenchContextObservation {
            session_id: ctx.session_id.clone(),
            message_count: input.messages.len(),
            tool_provider_count: input.tool_providers.len(),
            committed_message_count: ctx.state.messages().len(),
            max_context_tokens: ctx.max_context_tokens,
            last_prompt_context_tokens: ctx
                .prompt_usage
                .as_ref()
                .map(|usage: &lash::usage::TokenUsage| usage.input_total().max(0) as usize),
        };
        *self.observed.lock_recover() = Some(observation.clone());

        // The transform shapes the messages the model call carries: it adds
        // its note after the prepared ones. The system prompt is the
        // session's recorded config and no transform's to change.
        let mut output = input;
        output.messages.make_mut().push(lash::messages::Message {
            id: "workbench-context-budget".to_string(),
            role: lash::messages::MessageRole::User,
            parts: vec![lash::messages::Part::text(
                "workbench-context-budget.p0".to_string(),
                format!(
                    "Context budget: prepared {} message(s) from {} committed",
                    observation.message_count, observation.committed_message_count,
                ),
                None,
            )]
            .into(),
            origin: None,
            reply_marker: None,
        });
        Ok(output)
    }
}

#[derive(Clone, Default)]
pub(crate) struct WorkbenchConfigChanges {
    pub(crate) latest: Arc<Mutex<Option<WorkbenchConfigChange>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkbenchConfigChange {
    pub(crate) session_id: SessionId,
    pub(crate) previous_profile_key: String,
    pub(crate) current_profile_key: String,
    pub(crate) service_profile_key: String,
}

/// The model key a policy records, or empty for one that selects none.
fn recorded_llm_profile_id(policy: &lash::runtime::SessionPolicy) -> String {
    policy
        .profile_key()
        .map(ToString::to_string)
        .unwrap_or_default()
}

impl WorkbenchConfigChanges {
    pub(crate) async fn observe(
        &self,
        ctx: &lash::plugins::SessionConfigChangedContext,
    ) -> Result<(), PluginError> {
        let snapshot = ctx.sessions.snapshot_current().await?;
        *self.latest.lock_recover() = Some(WorkbenchConfigChange {
            session_id: ctx.session_id.clone(),
            previous_profile_key: recorded_llm_profile_id(&ctx.previous),
            current_profile_key: recorded_llm_profile_id(&ctx.current),
            service_profile_key: recorded_llm_profile_id(&snapshot.policy),
        });
        Ok(())
    }
}

/// Summarize the previously committed turn. The runtime records the request
/// returned by the after-turn callback before appending it to the new leaf.
/// Its ancestor fence keeps the summary on the branch it describes.
pub(crate) fn workbench_derived_note(
    state: &lash::persistence::SessionReadView,
) -> Option<lash::plugins::AppendSessionNodesRequest> {
    if state.turn_index() == 0 {
        return None;
    }
    let base_node_id = state.session_graph().leaf_node_id.clone()?;
    Some(lash::plugins::AppendSessionNodesRequest {
        operation_id: format!("workbench-derived-note:{base_node_id}"),
        nodes: vec![lash::plugins::SessionAppendNode::plugin(
            WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE,
            json!({
                "derived_from_node_id": base_node_id,
                "summary": workbench_note_summary(state),
            }),
        )],
        requires_ancestor_node_id: Some(base_node_id),
    })
}

pub(crate) const WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE: &str = "workbench.turn_note";

/// Stand-in for the expensive derivation: in a deployment this is a model call
/// over the transcript, which is exactly why the write-back lands a commit late.
pub(crate) fn workbench_note_summary(state: &lash::persistence::SessionReadView) -> String {
    format!(
        "{} messages after turn {}",
        state.messages().len(),
        state.turn_index()
    )
}

#[expect(
    clippy::expect_used,
    reason = "the three workbench trigger source types register against distinct split type \
              names and event types built here from valid object shapes"
)]
pub(crate) fn workbench_lashlang_resources() -> lash::rlm::lang::LashlangHostCatalog {
    let mut resources = lash::rlm::lang::LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            CRON_SCHEDULE_SOURCE_TYPE.split('.'),
            cron_schedule_config_type(),
            cron_tick_event_type(),
        )
        .expect("valid cron trigger source");
    resources
        .add_trigger_source_constructor(
            BUTTON_TRIGGER_SOURCE_TYPE.split('.'),
            lash::rlm::lang::TypeExpr::Object(vec![]),
            button_pressed_event_type(),
        )
        .expect("valid button trigger source");
    resources
        .add_trigger_source_constructor(
            MAIL_RECEIVED_SOURCE_TYPE.split('.'),
            lash::rlm::lang::TypeExpr::Object(vec![]),
            mail_received_event_type(),
        )
        .expect("valid mail trigger source");
    resources
}

/// The configuration contract `cron.Schedule` declares, and therefore the
/// contract a registration captures and a delivery's start checks every
/// emitted occurrence source against. `tz` is optional, so an occurrence for a schedule
/// registered without one must omit the key rather than send `null`.
pub(crate) fn cron_schedule_config_type() -> lash::rlm::lang::TypeExpr {
    lash::rlm::lang::TypeExpr::Object(vec![
        lash::rlm::lang::TypeField {
            name: "expr".into(),
            ty: lash::rlm::lang::TypeExpr::Str,
            optional: false,
        },
        lash::rlm::lang::TypeField {
            name: "tz".into(),
            ty: lash::rlm::lang::TypeExpr::Str,
            optional: true,
        },
    ])
}

#[expect(
    clippy::expect_used,
    reason = "`ui.button.Pressed` and its all-string fields satisfy NamedDataType::object's validation"
)]
pub(crate) fn button_pressed_event_type() -> lash::rlm::lang::NamedDataType {
    lash::rlm::lang::NamedDataType::object(
        "ui.button.Pressed",
        vec![
            field("button", lash::rlm::lang::TypeExpr::Str),
            field("message", lash::rlm::lang::TypeExpr::Str),
            field("pressed_at", lash::rlm::lang::TypeExpr::Str),
        ],
    )
    .expect("valid button pressed event type")
}

#[expect(
    clippy::expect_used,
    reason = "`mail.Received` and its all-string fields satisfy NamedDataType::object's validation"
)]
pub(crate) fn mail_received_event_type() -> lash::rlm::lang::NamedDataType {
    lash::rlm::lang::NamedDataType::object(
        "mail.Received",
        vec![
            field("account", lash::rlm::lang::TypeExpr::Str),
            field("title", lash::rlm::lang::TypeExpr::Str),
            field("text", lash::rlm::lang::TypeExpr::Str),
        ],
    )
    .expect("valid mail received event type")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn mail_received_payload_schema() -> lash::schema::JsonSchema {
    lash::schema::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "properties": {
            "account": { "type": "string" },
            "title": { "type": "string" },
            "text": { "type": "string" }
        },
        "required": ["account", "title", "text"],
        "additionalProperties": false
    }))
    .expect("valid declared payload schema")
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub(crate) fn button_trigger_payload_schema() -> lash::schema::JsonSchema {
    lash::schema::JsonSchema::admit(serde_json::json!({
        "type": "object",
        "properties": {
            "button": { "type": "string", "enum": ["Red", "Blue"] },
            "message": { "type": "string" },
            "pressed_at": { "type": "string" }
        },
        "required": ["button", "message", "pressed_at"],
        "additionalProperties": false
    }))
    .expect("valid declared payload schema")
}

pub(crate) fn field(name: &str, ty: lash::rlm::lang::TypeExpr) -> lash::rlm::lang::TypeField {
    lash::rlm::lang::TypeField {
        name: name.into(),
        ty,
        optional: false,
    }
}

impl AppState {
    /// What the workbench creates a session with: its default spec, running
    /// the host's model selection and stating the connected accounts at the
    /// moment of creation.
    pub(crate) fn session_creation(&self) -> Result<lash::SessionCreation, serde_json::Error> {
        let selection = self.selected_llm_profile();
        let spec = self
            .session_defaults
            .clone()
            .model(selection.key())
            .reasoning(selection.reasoning());
        let spec = match crate::session_protocol::selected().map_err(serde::de::Error::custom)? {
            crate::session_protocol::SessionProtocol::Standard => spec,
            crate::session_protocol::SessionProtocol::Rlm => spec.plugin(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    termination: Some(lash::rlm::RlmTermination::Natural {
                        schema: Some(
                            lash::schema::JsonSchema::admit(json!({ "type": "string" }))
                                .map_err(serde::de::Error::custom)?,
                        ),
                    }),
                    ..Default::default()
                },
            )?,
        };
        Ok(lash::SessionCreation::root(spec))
    }
}

fn prompt_key(key: &str) -> Result<lash::prompt::PromptSectionKey, PluginError> {
    lash::prompt::PromptSectionKey::new(key)
        .map_err(|error| PluginError::Registration(error.to_string()))
}

/// The workbench's host text, as its own prompt sections (ADR 0133): its
/// intro in place of the standard protocol's, the standing RLM instructions
/// (ADR 0063: worked examples in the session's own language, and TypeScript
/// is the sole RLM language, ADR 0096) with the deferred catalogue's
/// advertisement, and the connected accounts, rendered for each call.
fn register_workbench_prompt_sections(
    reg: &mut PluginRegistrar,
    mail_world: &mail::MailWorld,
) -> Result<(), PluginError> {
    use lash::plugins::{
        PromptInput, PromptSectionSpec, PromptWrapSpec, PromptWrapTarget, SectionText,
    };
    use lash::prompt::{PromptPlacement, PromptSectionId, PromptWrapKey};
    reg.prompt().wrap(
        PromptWrapSpec::new(
            PromptWrapKey::new("intro")
                .map_err(|error| PluginError::Registration(error.to_string()))?,
            PromptSectionId::new(
                lash::standard::STANDARD_PROTOCOL_PLUGIN_ID,
                prompt_key(lash::standard::standard_section_keys::INTRO)?,
            ),
        ),
        Arc::new(
            |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, _: SectionText| {
                Ok(SectionText::text("You are the Agent Workbench assistant."))
            },
        ),
    )?;
    if matches!(
        crate::session_protocol::selected(),
        Ok(crate::session_protocol::SessionProtocol::Rlm)
    ) {
        reg.prompt().section(
            PromptSectionSpec::new(
                prompt_key("instructions")?,
                PromptPlacement::InitialInstructions,
            ),
            Arc::new(|_: &PromptInput<'_>| {
                Ok(SectionText::Text(format!(
                    "## Instructions\n\n{}\n\n{}",
                    workbench_prompt().trim(),
                    deferred_tools::prompt_preview().trim()
                )))
            }),
        )?;
    }
    let mail_world = mail_world.clone();
    reg.prompt().section(
        PromptSectionSpec::new(
            prompt_key("accounts")?,
            PromptPlacement::InitialInstructions,
        ),
        Arc::new(move |_: &PromptInput<'_>| {
            Ok(SectionText::Text(format!(
                "## Context\n\n{}",
                connected_accounts_prompt(&mail_world)
            )))
        }),
    )
}

pub(crate) fn connected_accounts_prompt(mail_world: &mail::MailWorld) -> String {
    let accounts = mail_world.account_summaries();
    if accounts.is_empty() {
        return "Connected inbox accounts: none yet. The `inbox` namespace is empty until the \
            user adds an account from the Accounts tab, so `inbox.<anything>` will not resolve. \
            If asked to use an inbox, tell the user to add one first instead of guessing a name."
            .to_string();
    }
    let list = accounts
        .iter()
        .map(|account| account.authority.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Connected inbox authorities right now: {list}. These are the ONLY inbox accounts that \
        exist — use these exact paths and never reference any other `inbox.<name>`. The \
        `inbox.work` / `inbox.personal` names used in the examples above are illustrative only; \
        substitute the real authorities listed here."
    )
}

#[expect(
    clippy::expect_used,
    reason = "`cron.Tick` and its single string field satisfy NamedDataType::object's validation"
)]
pub(crate) fn cron_tick_event_type() -> lash::rlm::lang::NamedDataType {
    lash::rlm::lang::NamedDataType::object(
        "cron.Tick",
        vec![lash::rlm::lang::TypeField {
            name: "fired_at".into(),
            ty: lash::rlm::lang::TypeExpr::Str,
            optional: false,
        }],
    )
    .expect("valid cron tick type")
}
