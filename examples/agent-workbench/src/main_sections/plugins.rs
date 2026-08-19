struct WorkbenchPluginFactory {
    tavily_api_key: String,
    mail_world: mail::MailWorld,
    derived_notes: WorkbenchDerivedNotes,
    config_changes: WorkbenchConfigChanges,
    context_budget: WorkbenchContextBudget,
    deferred_tools: deferred_tools::WorkbenchDeferredTools,
    approvals: approvals::WorkbenchApprovals,
}

impl WorkbenchPluginFactory {
    fn new(tavily_api_key: impl Into<String>) -> Self {
        Self {
            tavily_api_key: tavily_api_key.into(),
            mail_world: mail::MailWorld::new(),
            derived_notes: WorkbenchDerivedNotes::default(),
            config_changes: WorkbenchConfigChanges::default(),
            context_budget: WorkbenchContextBudget::default(),
            deferred_tools: deferred_tools::WorkbenchDeferredTools::in_memory()
                .expect("open in-memory deferred-tool grants"),
            approvals: approvals::WorkbenchApprovals::in_memory()
                .expect("open in-memory approval ledger"),
        }
    }

    fn with_mail_world(mut self, mail_world: mail::MailWorld) -> Self {
        self.mail_world = mail_world;
        self
    }

    fn with_deferred_tools(
        mut self,
        deferred_tools: deferred_tools::WorkbenchDeferredTools,
    ) -> Self {
        self.deferred_tools = deferred_tools;
        self
    }

    fn with_approvals(mut self, approvals: approvals::WorkbenchApprovals) -> Self {
        self.approvals = approvals;
        self
    }

    /// Handle on the annotator's decision log, so a harness can read what the
    /// append fence did with each derived note.
    #[cfg(test)]
    fn derived_notes(&self) -> WorkbenchDerivedNotes {
        self.derived_notes.clone()
    }

    #[cfg(test)]
    fn config_changes(&self) -> WorkbenchConfigChanges {
        self.config_changes.clone()
    }

    /// Handle on what the per-turn context transform actually saw, so a harness
    /// can check the prepared context the runtime handed it.
    #[cfg(test)]
    fn context_budget(&self) -> WorkbenchContextBudget {
        self.context_budget.clone()
    }
}

impl PluginFactory for WorkbenchPluginFactory {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

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
            tavily_api_key: self.tavily_api_key.clone(),
            mail_world: self.mail_world.clone(),
            derived_notes: self.derived_notes.clone(),
            config_changes: self.config_changes.clone(),
            context_budget: self.context_budget.clone(),
            deferred_tools: self.deferred_tools.clone(),
            approvals: self.approvals.clone(),
        }))
    }
}

struct WorkbenchSessionPlugin {
    tavily_api_key: String,
    mail_world: mail::MailWorld,
    derived_notes: WorkbenchDerivedNotes,
    config_changes: WorkbenchConfigChanges,
    context_budget: WorkbenchContextBudget,
    deferred_tools: deferred_tools::WorkbenchDeferredTools,
    approvals: approvals::WorkbenchApprovals,
}

impl SessionPlugin for WorkbenchSessionPlugin {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let mail_world = self.mail_world.clone();
        let deferred_preview = self.deferred_tools.clone();
        reg.prompt().contribute(Arc::new(move |ctx| {
            let mail_world = mail_world.clone();
            let deferred_preview = deferred_preview.clone();
            // ADR 0063: the host's worked examples are written in the dialect
            // the executor is actually serving this turn, read from its own
            // execution section rather than from this process's configuration.
            let prompt = workbench_prompt(tutorial_dialect(&ctx.protocol_turn_options));
            Box::pin(async move {
                let mut contributions = vec![PromptContribution::environment(
                    "Agent Workbench",
                    format!("{prompt}\n\n{}", connected_accounts_prompt(&mail_world)),
                )];
                contributions.push(deferred_preview.preview_contribution());
                Ok(contributions)
            })
        }));
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
            .provider(Arc::new(lash_tools::web::web_search_provider(
                self.tavily_api_key.clone(),
            )))?;
        reg.tools()
            .provider(Arc::new(lash_tools::web::fetch_url_provider(
                self.tavily_api_key.clone(),
            )))?;
        reg.tools()
            .provider(self.deferred_tools.search_provider())?;
        reg.tools()
            .provider(self.deferred_tools.execution_provider())?;
        reg.tools().provider(self.approvals.provider())?;
        reg.tools().provider(Arc::new(mail::MockMailProvider::new(
            self.mail_world.clone(),
        )))?;
        reg.context()
            .prepare_turn(0, Arc::new(self.context_budget.clone()));
        let derived_notes = self.derived_notes.clone();
        let config_changes = self.config_changes.clone();
        reg.session().on_event(Arc::new(move |event| {
            let derived_notes = derived_notes.clone();
            let config_changes = config_changes.clone();
            Box::pin(async move {
                match event {
                    lash::plugins::PluginLifecycleEvent::TurnPersisted(ctx) => {
                        derived_notes.on_turn_persisted(&ctx).await;
                    }
                    lash::plugins::PluginLifecycleEvent::SessionConfigChanged(ctx) => {
                        config_changes.observe(&ctx).await?;
                    }
                    _ => {}
                }
                Ok(())
            })
        }));
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
struct WorkbenchContextBudget {
    observed: Arc<Mutex<Option<WorkbenchContextObservation>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkbenchContextObservation {
    session_id: String,
    message_count: usize,
    contribution_count: usize,
    tool_provider_count: usize,
    include_base_tools: bool,
    committed_message_count: usize,
    max_context_tokens: Option<usize>,
    last_prompt_context_tokens: Option<usize>,
}

impl WorkbenchContextBudget {
    #[cfg(test)]
    fn observation(&self) -> Option<WorkbenchContextObservation> {
        self.observed
            .lock_recover()
            .clone()
    }
}

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
            contribution_count: input.prompt_contributions.len(),
            tool_provider_count: input.tool_providers.len(),
            include_base_tools: input.include_base_tools,
            committed_message_count: ctx.state.messages().len(),
            max_context_tokens: ctx.max_context_tokens,
            last_prompt_context_tokens: ctx
                .prompt_usage
                .as_ref()
                .map(|usage: &lash::runtime::PromptUsage| usage.prompt_context_tokens),
        };
        *self
            .observed
            .lock_recover() = Some(observation.clone());

        let mut output = input;
        output.prompt_contributions.push(
            lash::prompt::PromptContribution::new(
                lash::prompt::PromptSlot::Environment,
                "Context budget",
                format!(
                    "prepared {} message(s) from {} committed; base tools {}",
                    observation.message_count,
                    observation.committed_message_count,
                    if observation.include_base_tools {
                        "on"
                    } else {
                        "off"
                    }
                ),
            )
            .with_priority(-100),
        );
        Ok(output)
    }
}

#[derive(Clone, Default)]
struct WorkbenchConfigChanges {
    latest: Arc<Mutex<Option<WorkbenchConfigChange>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkbenchConfigChange {
    session_id: String,
    previous_model_id: String,
    current_model_id: String,
    service_model_id: String,
}

impl WorkbenchConfigChanges {
    async fn observe(
        &self,
        ctx: &lash::plugins::SessionConfigChangedContext,
    ) -> Result<(), PluginError> {
        let snapshot = ctx.sessions.snapshot_current().await?;
        *self
            .latest
            .lock_recover() = Some(WorkbenchConfigChange {
            session_id: ctx.session_id.clone(),
            previous_model_id: ctx.previous.model_id().to_string(),
            current_model_id: ctx.current.model_id().to_string(),
            service_model_id: snapshot.policy.model_id().to_string(),
        });
        Ok(())
    }

    #[cfg(test)]
    fn latest(&self) -> Option<WorkbenchConfigChange> {
        self.latest
            .lock_recover()
            .clone()
    }
}

/// The workbench's derive-then-append annotator: a background worker that
/// summarizes a committed turn and writes the summary back into the session's
/// own history, so it survives a restart and travels with the branch.
///
/// Deriving a summary is slow — in a real deployment it is a model call — so a
/// note is always written back *after* the commit it describes, into a session
/// whose head has already moved on. That is the whole reason each note carries
/// [`lash::plugins::AppendSessionNodesRequest::requires_ancestor_node_id`]: the
/// worker keeps no session bookkeeping at all (session ids change when an
/// operator rewinds a branch, and the queue would be wrong the moment they
/// did), and instead lets the append itself decide. A head that merely moved
/// on keeps the note; a base that is no longer on the session's active path
/// throws it away, because the conversation it summarizes is not the one this
/// session is having.
#[derive(Clone, Default)]
struct WorkbenchDerivedNotes {
    inner: Arc<WorkbenchDerivedNotesState>,
}

#[derive(Default)]
struct WorkbenchDerivedNotesState {
    pending: Mutex<Vec<WorkbenchPendingNote>>,
    settled: Mutex<Vec<WorkbenchSettledNote>>,
}

#[derive(Clone, Debug)]
struct WorkbenchPendingNote {
    /// The node the summary was read at. Not where the note lands.
    base_node_id: String,
    summary: String,
}

/// What the append fence decided about one derived note.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkbenchSettledNote {
    /// Kept: `node_id` is where it actually landed, which is the leaf as of
    /// the append and generally *not* `base_node_id`.
    Written {
        base_node_id: String,
        node_id: String,
        leaf_node_id: String,
    },
    /// Dropped: the branch `base_node_id` sat on is not the one this session
    /// executes any more, so the summary describes history that is gone.
    AbandonedBranch { base_node_id: String },
}

impl WorkbenchDerivedNotes {
    async fn on_turn_persisted(&self, ctx: &lash::plugins::SessionStateChangedContext<'_>) {
        for note in self.take_pending() {
            self.write_back(ctx, note).await;
        }
        // Start the next derivation from the head this turn just committed.
        if let Some(base_node_id) = ctx.state.session_graph().leaf_node_id.clone() {
            let summary = workbench_note_summary(&ctx.state);
            self.inner
                .pending
                .lock_recover()
                .push(WorkbenchPendingNote {
                    base_node_id,
                    summary,
                });
        }
    }

    async fn write_back(
        &self,
        ctx: &lash::plugins::SessionStateChangedContext<'_>,
        note: WorkbenchPendingNote,
    ) {
        let request = lash::plugins::AppendSessionNodesRequest {
            operation_id: format!("workbench-derived-note:{}", note.base_node_id),
            nodes: vec![lash::plugins::SessionAppendNode::plugin(
                WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE,
                json!({
                    // The base rides in the payload: the note's position in the
                    // graph says nothing about what it was derived from.
                    "derived_from_node_id": note.base_node_id,
                    "summary": note.summary,
                }),
            )],
            requires_ancestor_node_id: Some(note.base_node_id.clone()),
        };
        let settled = match ctx
            .session_graph
            .append_session_nodes(&ctx.session_id, request)
            .await
        {
            Ok(lash::plugins::AppendSessionNodesOutcome::Appended {
                node_ids,
                leaf_node_id,
            }) => WorkbenchSettledNote::Written {
                base_node_id: note.base_node_id,
                node_id: node_ids
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| leaf_node_id.clone()),
                leaf_node_id,
            },
            Ok(lash::plugins::AppendSessionNodesOutcome::StaleBranch { required_node_id }) => {
                WorkbenchSettledNote::AbandonedBranch {
                    base_node_id: required_node_id,
                }
            }
            Err(error) => {
                // A store or plugin failure is not a verdict about the branch;
                // keep the note and let the next persisted turn retry it.
                eprintln!("workbench derived note write-back failed: {error}");
                self.inner
                    .pending
                    .lock_recover()
                    .push(note);
                return;
            }
        };
        if let WorkbenchSettledNote::AbandonedBranch { base_node_id } = &settled {
            println!(
                "workbench derived note dropped: `{base_node_id}` is no longer on \
                 session `{}`'s active path",
                ctx.session_id
            );
        }
        let mut log = self
            .inner
            .settled
            .lock_recover();
        log.push(settled);
        // The decision log is a rolling operator aid, not a record: a long-lived
        // workbench must not accumulate one entry per turn forever.
        let overflow = log.len().saturating_sub(WORKBENCH_DERIVED_NOTE_LOG_LIMIT);
        log.drain(..overflow);
    }

    fn take_pending(&self) -> Vec<WorkbenchPendingNote> {
        std::mem::take(
            &mut *self
                .inner
                .pending
                .lock_recover(),
        )
    }

    #[cfg(test)]
    fn settled(&self) -> Vec<WorkbenchSettledNote> {
        self.inner
            .settled
            .lock_recover()
            .clone()
    }
}

const WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE: &str = "workbench.turn_note";
const WORKBENCH_DERIVED_NOTE_LOG_LIMIT: usize = 64;

/// Stand-in for the expensive derivation: in a deployment this is a model call
/// over the transcript, which is exactly why the write-back lands a commit late.
fn workbench_note_summary(state: &lash::persistence::SessionReadView) -> String {
    format!("{} messages after turn {}", state.messages().len(), state.turn_index())
}

fn workbench_lashlang_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_trigger_source_constructor(
            CRON_SCHEDULE_SOURCE_TYPE.split('.'),
            lashlang::TypeExpr::Object(vec![
                lashlang::TypeField {
                    name: "expr".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                },
                lashlang::TypeField {
                    name: "tz".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: true,
                },
            ]),
            cron_tick_event_type(),
        )
        .expect("valid cron trigger source");
    resources
        .add_trigger_source_constructor(
            BUTTON_TRIGGER_SOURCE_TYPE.split('.'),
            lashlang::TypeExpr::Object(vec![]),
            button_pressed_event_type(),
        )
        .expect("valid button trigger source");
    resources
        .add_trigger_source_constructor(
            MAIL_RECEIVED_SOURCE_TYPE.split('.'),
            lashlang::TypeExpr::Object(vec![]),
            mail_received_event_type(),
        )
        .expect("valid mail trigger source");
    resources
}

fn button_pressed_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "ui.button.Pressed",
        vec![
            field("button", lashlang::TypeExpr::Str),
            field("message", lashlang::TypeExpr::Str),
            field("pressed_at", lashlang::TypeExpr::Str),
        ],
    )
    .expect("valid button pressed event type")
}

fn mail_received_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "mail.Received",
        vec![
            field("account", lashlang::TypeExpr::Str),
            field("title", lashlang::TypeExpr::Str),
            field("text", lashlang::TypeExpr::Str),
        ],
    )
    .expect("valid mail received event type")
}

fn mail_received_payload_schema() -> lash::triggers::LashSchema {
    lash::triggers::LashSchema::new(serde_json::json!({
        "type": "object",
        "properties": {
            "account": { "type": "string" },
            "title": { "type": "string" },
            "text": { "type": "string" }
        },
        "required": ["account", "title", "text"],
        "additionalProperties": false
    }))
}

fn button_trigger_payload_schema() -> lash::triggers::LashSchema {
    lash::triggers::LashSchema::new(serde_json::json!({
        "type": "object",
        "properties": {
            "button": { "type": "string", "enum": ["Red", "Blue"] },
            "message": { "type": "string" },
            "pressed_at": { "type": "string" }
        },
        "required": ["button", "message", "pressed_at"],
        "additionalProperties": false
    }))
}

fn field(name: &str, ty: lashlang::TypeExpr) -> lashlang::TypeField {
    lashlang::TypeField {
        name: name.into(),
        ty,
        optional: false,
    }
}

/// Live, per-turn prompt line naming the inbox authorities that actually exist,
/// so the agent never assumes the illustrative `inbox.work`/`inbox.personal`
/// names from the static guidance are real.
fn connected_accounts_prompt(mail_world: &mail::MailWorld) -> String {
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

fn cron_tick_event_type() -> lashlang::NamedDataType {
    lashlang::NamedDataType::object(
        "cron.Tick",
        vec![lashlang::TypeField {
            name: "fired_at".into(),
            ty: lashlang::TypeExpr::Str,
            optional: false,
        }],
    )
    .expect("valid cron tick type")
}
