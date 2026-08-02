struct WorkbenchPluginFactory {
    tavily_api_key: String,
    mail_world: mail::MailWorld,
    derived_notes: WorkbenchDerivedNotes,
}

impl WorkbenchPluginFactory {
    fn new(tavily_api_key: impl Into<String>) -> Self {
        Self {
            tavily_api_key: tavily_api_key.into(),
            mail_world: mail::MailWorld::new(),
            derived_notes: WorkbenchDerivedNotes::default(),
        }
    }

    fn with_mail_world(mut self, mail_world: mail::MailWorld) -> Self {
        self.mail_world = mail_world;
        self
    }

    /// Handle on the annotator's decision log, so a harness can read what the
    /// append fence did with each derived note.
    #[cfg(test)]
    fn derived_notes(&self) -> WorkbenchDerivedNotes {
        self.derived_notes.clone()
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
        }))
    }
}

struct WorkbenchSessionPlugin {
    tavily_api_key: String,
    mail_world: mail::MailWorld,
    derived_notes: WorkbenchDerivedNotes,
}

impl SessionPlugin for WorkbenchSessionPlugin {
    fn id(&self) -> &'static str {
        "agent_workbench"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let mail_world = self.mail_world.clone();
        reg.prompt().contribute(Arc::new(move |_ctx| {
            let mail_world = mail_world.clone();
            Box::pin(async move {
                Ok(vec![PromptContribution::environment(
                    "Agent Workbench",
                    format!(
                        "{WORKBENCH_PROMPT}\n\n{}",
                        connected_accounts_prompt(&mail_world)
                    ),
                )])
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
        reg.tools().provider(Arc::new(mail::MockMailProvider::new(
            self.mail_world.clone(),
        )))?;
        let derived_notes = self.derived_notes.clone();
        reg.session().on_event(Arc::new(move |event| {
            let derived_notes = derived_notes.clone();
            Box::pin(async move {
                if let lash_core::PluginLifecycleEvent::TurnPersisted(ctx) = event {
                    derived_notes.on_turn_persisted(&ctx).await;
                }
                Ok(())
            })
        }));
        Ok(())
    }
}

/// The workbench's derive-then-append annotator: a background worker that
/// summarizes a committed turn and writes the summary back into the session's
/// own history, so it survives a restart and travels with the branch.
///
/// Deriving a summary is slow — in a real deployment it is a model call — so a
/// note is always written back *after* the commit it describes, into a session
/// whose head has already moved on. That is the whole reason each note carries
/// [`lash_core::AppendSessionNodesRequest::requires_ancestor_node_id`]: the
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
    async fn on_turn_persisted(&self, ctx: &lash_core::SessionStateChangedContext<'_>) {
        for note in self.take_pending() {
            self.write_back(ctx, note).await;
        }
        // Start the next derivation from the head this turn just committed.
        if let Some(base_node_id) = ctx.state.session_graph().leaf_node_id.clone() {
            let summary = workbench_note_summary(&ctx.state);
            self.inner
                .pending
                .lock()
                .expect("workbench derived notes lock")
                .push(WorkbenchPendingNote {
                    base_node_id,
                    summary,
                });
        }
    }

    async fn write_back(
        &self,
        ctx: &lash_core::SessionStateChangedContext<'_>,
        note: WorkbenchPendingNote,
    ) {
        let request = lash_core::AppendSessionNodesRequest {
            operation_id: format!("workbench-derived-note:{}", note.base_node_id),
            nodes: vec![lash_core::SessionAppendNode::plugin(
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
            Ok(lash_core::AppendSessionNodesResult::Appended {
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
            Ok(lash_core::AppendSessionNodesResult::StaleBranch { required_node_id }) => {
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
                    .lock()
                    .expect("workbench derived notes lock")
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
            .lock()
            .expect("workbench derived notes lock");
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
                .lock()
                .expect("workbench derived notes lock"),
        )
    }

    #[cfg(test)]
    fn settled(&self) -> Vec<WorkbenchSettledNote> {
        self.inner
            .settled
            .lock()
            .expect("workbench derived notes lock")
            .clone()
    }
}

const WORKBENCH_DERIVED_NOTE_PLUGIN_TYPE: &str = "workbench.turn_note";
const WORKBENCH_DERIVED_NOTE_LOG_LIMIT: usize = 64;

/// Stand-in for the expensive derivation: in a deployment this is a model call
/// over the transcript, which is exactly why the write-back lands a commit late.
fn workbench_note_summary(state: &lash_core::SessionReadView) -> String {
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
