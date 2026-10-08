use super::*;
use lash::SessionId;

pub(crate) struct WorkbenchPluginFactory {
    pub(crate) mail_world: mail::MailWorld,
    pub(crate) config_changes: WorkbenchConfigChanges,
    pub(crate) deferred_tools: deferred_tools::WorkbenchDeferredTools,
    pub(crate) approvals: approvals::WorkbenchApprovals,
    pub(crate) host_triggers: host_triggers::HostTriggers,
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
            deferred_tools: deferred_tools::WorkbenchDeferredTools::in_memory()
                .expect("open in-memory deferred-tool grants"),
            approvals: approvals::WorkbenchApprovals::in_memory()
                .expect("open in-memory approval ledger"),
            host_triggers: host_triggers::HostTriggers::in_memory()
                .expect("open in-memory trigger tables"),
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

    pub(crate) fn with_host_triggers(mut self, host_triggers: host_triggers::HostTriggers) -> Self {
        self.host_triggers = host_triggers;
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
                    lash::rlm::lang::LashlangHostCatalog::new(),
                ),
            )
            .expect("workbench lashlang surface serializes"),
        ]
    }

    /// The workbench's own config namespace: the host prompt text its
    /// sections render.
    fn register_config(
        &self,
        registrar: &mut lash::plugins::ConfigRegistrar,
    ) -> Result<(), lash::plugins::ConfigRegistrationError> {
        registrar.owner(WorkbenchConfigOwner)?;
        registrar.command::<SetWorkbenchPromptContext>(|recorded, command| {
            Ok(lash::plugins::OwnerChange {
                recorded: WorkbenchPrompt {
                    context: command.context,
                    ..recorded.clone()
                },
                output: (),
            })
        })
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(WorkbenchSessionPlugin {
            mail_world: self.mail_world.clone(),
            config_changes: self.config_changes.clone(),
            deferred_tools: self.deferred_tools.clone(),
            approvals: self.approvals.clone(),
            host_triggers: self.host_triggers.clone(),
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
    pub(crate) deferred_tools: deferred_tools::WorkbenchDeferredTools,
    pub(crate) approvals: approvals::WorkbenchApprovals,
    pub(crate) host_triggers: host_triggers::HostTriggers,
}

impl SessionPlugin for WorkbenchSessionPlugin {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        reg.tools()
            .provider(self.deferred_tools.search_provider())?;
        reg.tools()
            .provider(self.deferred_tools.execution_provider())?;
        reg.tools().provider(self.approvals.provider())?;
        reg.tools().provider(self.host_triggers.provider())?;
        reg.tools().provider(Arc::new(mail::MockMailProvider::new(
            self.mail_world.clone(),
            self.host_triggers.clone(),
        )))?;
        register_workbench_sections(reg)?;
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

/// The workbench's host prompt text, recorded as its own session config
/// namespace: what its `instructions` and `accounts` prompt sections render.
/// A run renders what its admitted config records, so an account change
/// reaches the next run.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkbenchPrompt {
    /// The workbench's standing instructions, one paragraph each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) instructions: Vec<String>,
    /// What the host states about the world right now: the connected
    /// accounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) context: Vec<String>,
}

/// Replace the workbench's prompt context.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct SetWorkbenchPromptContext {
    pub(crate) context: Vec<String>,
}

impl lash::plugins::ConfigCommand for SetWorkbenchPromptContext {
    type Owner = WorkbenchConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt_context";
}

pub(crate) struct WorkbenchConfigOwner;

impl lash::plugins::ConfigOwner for WorkbenchConfigOwner {
    type Create = WorkbenchPrompt;
    type Recorded = WorkbenchPrompt;
    type Refusal = String;
    type RunOptions = lash::plugins::NoRunOptions;

    /// A session records the prompt its creator states, and none
    /// otherwise: a delegated child starts from what the delegation tool
    /// passes, never from its parent's prompt (ADR 0134).
    fn create(&self, input: Option<WorkbenchPrompt>) -> Result<Option<WorkbenchPrompt>, String> {
        Ok(input)
    }

    fn validate(
        &self,
        _value: &WorkbenchPrompt,
        _base: Option<&WorkbenchPrompt>,
        _facts: &lash::plugins::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &WorkbenchPrompt,
        _options: lash::plugins::NoRunOptions,
    ) -> Result<WorkbenchPrompt, String> {
        Ok(recorded.clone())
    }
}

/// The workbench's identity statement, which replaces the standard
/// protocol's intro.
pub(crate) const WORKBENCH_INTRO: &str = "You are the Agent Workbench assistant.";
pub(crate) const WORKBENCH_INSTRUCTIONS_SECTION: &str = "instructions";
pub(crate) const WORKBENCH_ACCOUNTS_SECTION: &str = "accounts";
pub(crate) const WORKBENCH_CONTEXT_BUDGET_SECTION: &str = "context_budget";

fn section_key(key: &str) -> Result<lash::prompt::PromptSectionKey, PluginError> {
    lash::prompt::PromptSectionKey::new(key)
        .map_err(|error| PluginError::Registration(error.to_string()))
}

/// The paragraphs joined, or an omission when there are none.
fn paragraphs(paragraphs: &[String]) -> lash::plugins::SectionText {
    let text = paragraphs
        .iter()
        .map(|paragraph| paragraph.trim())
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        lash::plugins::SectionText::Omit
    } else {
        lash::plugins::SectionText::Text(text)
    }
}

fn recorded_prompt(
    input: &lash::plugins::PromptInput<'_>,
) -> Result<WorkbenchPrompt, lash::plugins::PromptRenderError> {
    input
        .config::<WorkbenchPrompt>()
        .map(Option::unwrap_or_default)
        .map_err(|error| lash::plugins::PromptRenderError::new(error.to_string()))
}

/// The workbench's prompt sections. Its identity is a host section. Its host text, the standing instructions and the
/// connected accounts, renders from the run's admitted config. The
/// context budget states the shape of the call's projected history back to
/// the model, late and outside the history, as an annotation: durable
/// compaction is an explicit Agent Frame transition, not a rewrite of the
/// request.
pub(crate) fn register_workbench_sections(reg: &mut PluginRegistrar) -> Result<(), PluginError> {
    reg.prompt().section(
        lash::plugins::PromptSectionSpec::new(
            section_key("intro")?,
            lash::prompt::PromptPlacement::InitialInstructions,
        )
        .purposes([
            lash::prompt::PromptPurpose::Turn,
            lash::prompt::PromptPurpose::Compaction,
        ]),
        Arc::new(|_: &lash::plugins::PromptInput<'_>| {
            Ok(lash::plugins::SectionText::text(WORKBENCH_INTRO))
        }),
    )?;
    reg.prompt().section(
        lash::plugins::PromptSectionSpec::new(
            section_key(WORKBENCH_INSTRUCTIONS_SECTION)?,
            lash::prompt::PromptPlacement::InitialInstructions,
        ),
        Arc::new(|input: &lash::plugins::PromptInput<'_>| {
            Ok(paragraphs(&recorded_prompt(input)?.instructions))
        }),
    )?;
    reg.prompt().section(
        lash::plugins::PromptSectionSpec::new(
            section_key(WORKBENCH_ACCOUNTS_SECTION)?,
            lash::prompt::PromptPlacement::InitialInstructions,
        ),
        Arc::new(|input: &lash::plugins::PromptInput<'_>| {
            Ok(paragraphs(&recorded_prompt(input)?.context))
        }),
    )?;
    reg.prompt().section(
        lash::plugins::PromptSectionSpec::new(
            section_key(WORKBENCH_CONTEXT_BUDGET_SECTION)?,
            lash::prompt::PromptPlacement::CurrentContext,
        ),
        Arc::new(|input: &lash::plugins::PromptInput<'_>| {
            Ok(lash::plugins::SectionText::Text(context_budget_text(input)))
        }),
    )
}

/// The context budget's text: the call's projected history against the
/// committed history it was projected from.
pub(crate) fn context_budget_text(input: &lash::plugins::PromptInput<'_>) -> String {
    let committed = input
        .session()
        .map_or(0, |session| session.messages().len());
    format!(
        "Context budget: prepared {} message(s) from {committed} committed",
        input.history().messages,
    )
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
        let protocol = crate::session_protocol::selected().map_err(serde::de::Error::custom)?;
        let spec = spec.plugin(
            "agent_workbench",
            workbench_session_prompt(protocol, &self.mail_world),
        )?;
        let spec = match protocol {
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
        Ok(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            spec,
        ))
    }
}

/// The workbench's host prompt, stated in each session's creation spec. On
/// the RLM protocol it carries the standing instructions (ADR 0063: worked
/// examples in the session's own language, and TypeScript is the sole RLM
/// language, ADR 0096) with the deferred catalogue's advertisement; on the
/// standard protocol the workbench's identity replaces the protocol's intro
/// instead. Every session states the connected accounts as context.
pub(crate) fn workbench_session_prompt(
    protocol: crate::session_protocol::SessionProtocol,
    mail_world: &mail::MailWorld,
) -> WorkbenchPrompt {
    let instructions = match protocol {
        crate::session_protocol::SessionProtocol::Standard => Vec::new(),
        crate::session_protocol::SessionProtocol::Rlm => vec![
            workbench_prompt().to_string(),
            deferred_tools::prompt_preview(),
        ],
    };
    WorkbenchPrompt {
        instructions,
        context: workbench_prompt_context(mail_world),
    }
}

/// The prompt context a run should run under now: the connected accounts.
pub(crate) fn workbench_prompt_context(mail_world: &mail::MailWorld) -> Vec<String> {
    vec![connected_accounts_prompt(mail_world)]
}

/// Record an account change for every live session, including child sessions.
pub(crate) async fn record_accounts_context(state: &AppState) -> Result<(), AppError> {
    for view in state
        .core
        .sessions_filtered(lash::SessionListFilter {
            deleted: Some(false),
            ..Default::default()
        })
        .await
        .map_err(AppError::internal)?
    {
        let session = state
            .core
            .session(view.session_id.clone())
            .open()
            .await
            .map_err(|error| {
                state.session_admission_error(&view.session_id, "accounts.context", error)
            })?;
        record_accounts_context_for_session(state, &session).await?;
    }
    Ok(())
}

pub(crate) async fn record_accounts_context_for_session(
    state: &AppState,
    session: &lash::LashSession,
) -> Result<(), AppError> {
    let config = session.admin().config();
    loop {
        let transaction = lash::config::ConfigTransaction::of(SetWorkbenchPromptContext {
            context: workbench_prompt_context(&state.mail_world),
        });
        let revision = config.revision().await.map_err(|error| {
            state.session_admission_error(&session.session_id(), "accounts.context", error)
        })?;
        let outcome = config
            .apply(
                lash::config::ConfigWrite::new(
                    format!("accounts-context:{}", uuid::Uuid::new_v4()),
                    revision,
                ),
                transaction,
            )
            .await
            .map_err(AppError::internal)?
            .await_outcome(&config)
            .await
            .map_err(|error| {
                state.session_admission_error(&session.session_id(), "accounts.context", error)
            })?;
        match outcome {
            lash::config::ConfigTransactionOutcome::Applied { .. } => return Ok(()),
            lash::config::ConfigTransactionOutcome::Stale { .. } => continue,
            outcome => {
                return Err(AppError::internal(format!(
                    "the prompt context did not apply: {outcome:?}"
                )));
            }
        }
    }
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
