#[derive(Clone)]
struct AppState {
    core: LashCore,
    attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    process_observer: lash::process::ProcessWorkObserver,
    process_work_driver: lash::process::ProcessWorkDriver,
    session_ids: WorkbenchSessionIds,
    messages: Arc<Mutex<Vec<ChatMessage>>>,
    selected_model: Arc<Mutex<ModelSelection>>,
    web_configured: bool,
    trace_sink: Option<Arc<dyn TraceSink>>,
    lashlang_execution: Arc<TraceLashlangGraphStore>,
    event_tx: SessionEventRegistry,
    queued_work_driver: lash::runtime::QueuedWorkDriver,
    restate_ingress_url: String,
    #[cfg_attr(not(test), allow(dead_code))]
    restate_admin_url: String,
    restate_http: reqwest::Client,
    restate_cron_job_keys: Arc<Mutex<BTreeSet<String>>>,
    mail_world: mail::MailWorld,
    active_turns: ActiveTurns,
    authorization: WorkbenchAuthorization,
}

#[derive(Clone, Debug, Serialize)]
struct Settings {
    model: String,
    model_variant: Option<String>,
    web_configured: bool,
    model_variants: Vec<&'static str>,
    session_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelSelection {
    model: String,
    model_variant: Option<String>,
}

impl ModelSelection {
    fn from_spec(model: &lash::ModelSpec) -> Self {
        Self {
            model: model.id.clone(),
            model_variant: model.variant.effort().map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct StateSnapshot {
    settings: Settings,
    messages: Vec<ChatMessage>,
    observation: RemoteSessionObservation,
    product_events: ProductEventSnapshot,
    active_turns: Vec<lash::TurnAddress>,
    pending_turn_inputs: Vec<lash::PendingTurnInput>,
    queued_work: Vec<lash::persistence::QueuedWorkBatch>,
    turn_input_applications:
        Vec<lash::remote::observations::RemoteTurnInputApplication>,
    usage: lash::usage::SessionUsageReport,
}

#[derive(Debug, Serialize)]
struct StateReadSnapshot {
    #[serde(flatten)]
    state: StateSnapshot,
    transcript: Vec<TranscriptRow>,
}

impl std::ops::Deref for StateReadSnapshot {
    type Target = StateSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[derive(Clone, Debug)]
enum WorkbenchAuthorizationAction {
    Observe { session_id: String },
    EnqueueTurn { session_id: String },
    EnqueueTurnInput { session_id: String },
    CancelTurn { session_id: String },
    ManageQueuedWork { session_id: String },
    /// Destructive, deployment-wide maintenance. It is deliberately not
    /// session-scoped: no chat participant should ever be able to reach it.
    PruneTriggerMutationReceipts,
}

trait WorkbenchAuthorizer: Send + Sync {
    fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError>;
}

#[derive(Clone)]
struct WorkbenchAuthorization {
    authorizer: Arc<dyn WorkbenchAuthorizer>,
}

impl WorkbenchAuthorization {
    fn allow_all() -> Self {
        Self::with_authorizer(Arc::new(AllowAllWorkbenchAuthorizer))
    }

    fn with_authorizer(authorizer: Arc<dyn WorkbenchAuthorizer>) -> Self {
        Self { authorizer }
    }

    fn authorize(&self, action: WorkbenchAuthorizationAction) -> Result<(), AppError> {
        self.authorizer.authorize(&action)
    }
}

struct AllowAllWorkbenchAuthorizer;

impl WorkbenchAuthorizer for AllowAllWorkbenchAuthorizer {
    fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
        match action {
            WorkbenchAuthorizationAction::Observe { session_id }
            | WorkbenchAuthorizationAction::EnqueueTurn { session_id }
            | WorkbenchAuthorizationAction::EnqueueTurnInput { session_id }
            | WorkbenchAuthorizationAction::CancelTurn { session_id }
            | WorkbenchAuthorizationAction::ManageQueuedWork { session_id } => {
                let _ = session_id;
            }
            WorkbenchAuthorizationAction::PruneTriggerMutationReceipts => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatMessage {
    id: String,
    role: String,
    text: String,
    at: String,
}

/// The id of the optimistic user row this workbench publishes when a send is
/// accepted. It lives in the workbench's own id namespace — symmetric with
/// `workbench-assistant:{turn_id}` — because the UI owns the rows it renders.
/// The runtime's committed copy of the same text keeps its runtime-minted id
/// and is correlated by `MessageOrigin::TurnInput`, never by id shape
/// (FIG-972).
fn workbench_turn_user_message_id(turn_id: &str) -> String {
    format!("workbench-user:{turn_id}")
}

fn workbench_turn_id_from_user_message_id(message_id: &str) -> Option<&str> {
    message_id.strip_prefix("workbench-user:")
}

/// The id of the live agent row this workbench publishes when a turn produces a
/// reply, in the same workbench-owned namespace as the user row above.
///
/// The durable copy of that reply is usually the runtime's own terminal
/// assistant message, minted by the runtime under an id the workbench never
/// predicts, so this row retires from the product-event log when its turn stops
/// running rather than when a committed message happens to share its id
/// (FIG-984).
fn workbench_turn_assistant_message_id(turn_id: &str) -> String {
    format!("workbench-assistant:{turn_id}")
}

fn workbench_turn_id_from_assistant_message_id(message_id: &str) -> Option<&str> {
    message_id.strip_prefix("workbench-assistant:")
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TranscriptRow {
    Message {
        message: ChatMessage,
    },
    Reasoning {
        id: String,
        text: String,
    },
    CodeBlock {
        id: String,
        language: String,
        code: String,
        output: String,
        error: Option<String>,
        success: bool,
    },
}

#[derive(Debug, Deserialize)]
struct TurnRequest {
    text: String,
    model: Option<String>,
    model_variant: Option<String>,
    #[serde(default)]
    attachment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AttachmentUploadRequest {
    name: String,
    mime: String,
    data_base64: String,
}

#[derive(Clone, Debug, Serialize)]
struct AttachmentUploadResponse {
    attachment: lash::attachments::AttachmentRef,
    retrieve_url: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TurnInputIngressRequest {
    ActiveTurn,
    NextTurn,
}

#[derive(Debug, Deserialize)]
struct TurnInputRequest {
    text: String,
    ingress: TurnInputIngressRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TurnInputReceipt {
    accepted: bool,
    input_id: String,
    ingress: lash::persistence::TurnInputIngress,
    state: lash::persistence::TurnInputState,
    text: String,
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    cursor: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProductEventsQuery {
    cursor: Option<u64>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct SessionQuery {
    #[serde(default)]
    session_id: Option<String>,
}

impl SessionQuery {
    fn resolve(&self, state: &AppState) -> Result<String, AppError> {
        let Some(session_id) = self.session_id.as_deref() else {
            return Ok(state.current_session_id());
        };
        let session_id = session_id.trim();
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(AppError::bad_request(
                "session_id must be 1-128 ASCII letters, digits, '.', '_' or '-'",
            ));
        }
        Ok(session_id.to_string())
    }

    fn is_explicit(&self) -> bool {
        self.session_id.is_some()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) enum ButtonChoice {
    Red,
    Blue,
}

impl ButtonChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Red => "Red",
            Self::Blue => "Blue",
        }
    }

    fn lower(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Blue => "blue",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ButtonEventRequest {
    button: ButtonChoice,
    model: Option<String>,
    model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddAccountRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
struct InjectMessageRequest {
    title: Option<String>,
    text: Option<String>,
    model: Option<String>,
    model_variant: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamItem {
    Message {
        message: ChatMessage,
    },
    TurnInput {
        receipt: TurnInputReceipt,
    },
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

const PUBLIC_TURN_FAILURE_MESSAGE: &str = "turn could not be completed";

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProductEvent {
    event_id: String,
    sequence: u64,
    #[serde(flatten)]
    item: StreamItem,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ProductEventSnapshot {
    cursor: u64,
    events: Vec<ProductEvent>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ProductEventHistory {
    cursor: u64,
    events: Vec<ProductEvent>,
    #[serde(default)]
    event_ids: BTreeSet<String>,
}

impl ProductEventHistory {
    fn normalized(mut self) -> Self {
        self.cursor = self
            .events
            .last()
            .map_or(self.cursor, |event| self.cursor.max(event.sequence));
        self.event_ids
            .extend(self.events.iter().map(|event| event.event_id.clone()));
        self
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PersistedProductEventHistories {
    Current(HashMap<String, ProductEventHistory>),
    Legacy(HashMap<String, Vec<ProductEvent>>),
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ProductStreamItem {
    Event {
        event: ProductEvent,
    },
    Resync {
        snapshot: ProductEventSnapshot,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ObservationStreamItem {
    Cursor {
        cursor: String,
    },
    Observation {
        event: Box<RemoteSessionObservationEvent>,
    },
    ReplayGap {
        observation: Box<RemoteSessionObservation>,
        gap: Box<RemoteLiveReplayGap>,
    },
    TerminalReplacement {
        event: Box<RemoteSessionObservationEvent>,
        cursor: String,
    },
}

#[derive(Clone)]
struct SessionEventRegistry {
    histories: Arc<Mutex<HashMap<String, ProductEventHistory>>>,
    senders: Arc<Mutex<HashMap<String, broadcast::Sender<ProductEvent>>>>,
    channel_capacity: usize,
    path: Option<Arc<PathBuf>>,
}

impl SessionEventRegistry {
    #[cfg(test)]
    fn new(channel_capacity: usize) -> Self {
        Self {
            histories: Arc::new(Mutex::new(HashMap::new())),
            senders: Arc::new(Mutex::new(HashMap::new())),
            channel_capacity: channel_capacity.max(1),
            path: None,
        }
    }

    fn persistent(path: PathBuf, channel_capacity: usize) -> AnyhowResult<Self> {
        let histories = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes)
                .with_context(|| format!("decode product event log `{}`", path.display()))?
            {
                PersistedProductEventHistories::Current(histories) => histories
                    .into_iter()
                    .map(|(session_id, history)| (session_id, history.normalized()))
                    .collect(),
                PersistedProductEventHistories::Legacy(histories) => histories
                    .into_iter()
                    .map(|(session_id, events)| {
                        let cursor = events.last().map_or(0, |event| event.sequence);
                        (
                            session_id,
                            ProductEventHistory {
                                cursor,
                                events,
                                event_ids: BTreeSet::new(),
                            }
                            .normalized(),
                        )
                    })
                    .collect(),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read product event log `{}`", path.display()));
            }
        };
        Ok(Self {
            histories: Arc::new(Mutex::new(histories)),
            senders: Arc::new(Mutex::new(HashMap::new())),
            channel_capacity: channel_capacity.max(1),
            path: Some(Arc::new(path)),
        })
    }

    fn sender(&self, session_id: &str) -> broadcast::Sender<ProductEvent> {
        let mut senders = self.senders.lock().expect("session event registry lock");
        senders
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(self.channel_capacity).0)
            .clone()
    }

    #[cfg(test)]
    fn subscribe(&self, session_id: &str) -> broadcast::Receiver<ProductEvent> {
        self.sender(session_id).subscribe()
    }

    fn subscribe_after(
        &self,
        session_id: &str,
        cursor: u64,
    ) -> (Vec<ProductEvent>, broadcast::Receiver<ProductEvent>) {
        let receiver = self.sender(session_id).subscribe();
        let replay = self
            .histories
            .lock()
            .expect("product event history lock")
            .get(session_id)
            .into_iter()
            .flat_map(|history| history.events.iter())
            .filter(|event| event.sequence > cursor)
            .cloned()
            .collect();
        (replay, receiver)
    }

    #[cfg(test)]
    fn publish(&self, session_id: &str, item: StreamItem) {
        self.publish_identified(
            session_id,
            format!("workbench-product-event:{}", uuid::Uuid::new_v4()),
            item,
        );
    }

    fn publish_identified(
        &self,
        session_id: &str,
        event_id: impl Into<String>,
        item: StreamItem,
    ) -> bool {
        let event_id = event_id.into();
        let event = {
            let mut histories = self
                .histories
                .lock()
                .expect("product event history lock");
            let history = histories.entry(session_id.to_string()).or_default();
            if !history.event_ids.insert(event_id.clone()) {
                return false;
            }
            history.cursor = history.cursor.saturating_add(1);
            let event = ProductEvent {
                event_id,
                sequence: history.cursor,
                item,
            };
            history.events.push(event.clone());
            self.persist_snapshot(&histories);
            event
        };
        let _ = self.sender(session_id).send(event);
        true
    }

    fn snapshot(&self, session_id: &str) -> ProductEventSnapshot {
        let history = self
            .histories
            .lock()
            .expect("product event history lock")
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        ProductEventSnapshot {
            cursor: history.cursor,
            events: history.events,
        }
    }

    fn reconcile_settled(
        &self,
        session_id: &str,
        committed_message_ids: &BTreeSet<String>,
        active_turn_ids: &BTreeSet<String>,
    ) {
        let mut histories = self
            .histories
            .lock()
            .expect("product event history lock");
        let Some(history) = histories.get_mut(session_id) else {
            return;
        };
        let before = history.events.len();
        history.events.retain(|event| match &event.item {
            StreamItem::Message { message } => {
                match workbench_turn_id_from_user_message_id(&message.id)
                    .or_else(|| workbench_turn_id_from_assistant_message_id(&message.id))
                {
                    // A row the workbench owns belongs to a turn, so it retires
                    // when that turn stops running. Identity is not enough for
                    // the agent row: the durable copy of the reply is the
                    // runtime's terminal message on the paths where the runtime
                    // commits it, and that id is runtime-minted (FIG-984).
                    Some(turn_id) => active_turn_ids.contains(turn_id),
                    // Everything else in this lane is a mirror of a committed
                    // message and retires once that commit is readable.
                    None => !committed_message_ids.contains(&message.id),
                }
            }
            StreamItem::Done {
                turn_id: Some(turn_id),
            } => active_turn_ids.contains(turn_id),
            StreamItem::TurnInput { .. } | StreamItem::Done { turn_id: None } => true,
        });
        if history.events.len() != before {
            self.persist_snapshot(&histories);
        }
    }

    fn remove(&self, session_id: &str) {
        let mut histories = self
            .histories
            .lock()
            .expect("product event history lock");
        histories.remove(session_id);
        self.persist_snapshot(&histories);
        drop(histories);
        self.senders
            .lock()
            .expect("session event registry lock")
            .remove(session_id);
    }

    fn persist_snapshot(&self, histories: &HashMap<String, ProductEventHistory>) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let bytes = serde_json::to_vec(histories).expect("serialize product event log");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes).unwrap_or_else(|err| {
            panic!(
                "write product event log `{}`: {err}",
                temporary.display()
            )
        });
        std::fs::rename(&temporary, path).unwrap_or_else(|err| {
            panic!(
                "replace product event log `{}` from `{}`: {err}",
                path.display(),
                temporary.display()
            )
        });
    }

    #[cfg(test)]
    fn contains(&self, session_id: &str) -> bool {
        self.senders
            .lock()
            .expect("session event registry lock")
            .contains_key(session_id)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct TriggerEnabledRequest {
    enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
struct WorkbenchTriggerRegistration {
    // Keep these sibling names absent from the flattened core DTO: serde would
    // otherwise emit duplicate JSON keys with order-dependent browser values.
    #[serde(flatten)]
    registration: lash::triggers::TriggerRegistration,
    subscription_id: String,
    registrant_scope: String,
}

impl From<&lash::triggers::TriggerSubscriptionRecord> for WorkbenchTriggerRegistration {
    fn from(record: &lash::triggers::TriggerSubscriptionRecord) -> Self {
        Self {
            registration: lash::triggers::TriggerRegistration::from(record),
            subscription_id: record.subscription_id.clone(),
            registrant_scope: record.registrant_scope_id(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct TriggerMutationResponse {
    changed: bool,
    registration: Option<lash::triggers::TriggerRegistration>,
}

#[derive(Clone, Default)]
struct ActiveTurns {
    inner: Arc<Mutex<BTreeSet<(String, String)>>>,
    prompts: Arc<Mutex<BTreeMap<(String, String), String>>>,
    path: Option<Arc<PathBuf>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PersistedActiveTurns {
    Current {
        turns: BTreeSet<(String, String)>,
        #[serde(default)]
        prompts: Vec<PersistedActiveTurnPrompt>,
    },
    Legacy(BTreeSet<(String, String)>),
}

#[derive(Serialize)]
struct PersistedActiveTurnsRef<'a> {
    turns: &'a BTreeSet<(String, String)>,
    prompts: Vec<PersistedActiveTurnPromptRef<'a>>,
}

#[derive(Deserialize)]
struct PersistedActiveTurnPrompt {
    session_id: String,
    turn_id: String,
    prompt: String,
}

#[derive(Serialize)]
struct PersistedActiveTurnPromptRef<'a> {
    session_id: &'a str,
    turn_id: &'a str,
    prompt: &'a str,
}

impl ActiveTurns {
    fn persistent(path: PathBuf) -> AnyhowResult<Self> {
        let (turns, prompts) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes)
                .with_context(|| format!("decode active turns `{}`", path.display()))?
            {
                PersistedActiveTurns::Current { turns, prompts } => (
                    turns,
                    prompts
                        .into_iter()
                        .map(|prompt| ((prompt.session_id, prompt.turn_id), prompt.prompt))
                        .collect(),
                ),
                PersistedActiveTurns::Legacy(turns) => (turns, BTreeMap::new()),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                (BTreeSet::new(), BTreeMap::new())
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("read active turns `{}`", path.display()));
            }
        };
        let active = Self {
            inner: Arc::new(Mutex::new(turns)),
            prompts: Arc::new(Mutex::new(prompts)),
            path: Some(Arc::new(path)),
        };
        active.persist();
        Ok(active)
    }

    fn insert(&self, session_id: impl Into<String>, turn_id: impl Into<String>) {
        self.insert_with_prompt(session_id, turn_id, None);
    }

    fn insert_with_prompt(
        &self,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        prompt: Option<String>,
    ) {
        let key = (session_id.into(), turn_id.into());
        let mut active = self.inner.lock().expect("active turn lock");
        let mut prompts = self.prompts.lock().expect("active turn prompt lock");
        active.insert(key.clone());
        if let Some(prompt) = prompt {
            prompts.insert(key, prompt);
        }
        self.persist_snapshot(&active, &prompts);
    }

    fn remove(&self, session_id: &str, turn_id: &str) {
        let key = (session_id.to_string(), turn_id.to_string());
        let mut active = self.inner.lock().expect("active turn lock");
        let mut prompts = self.prompts.lock().expect("active turn prompt lock");
        active.remove(&key);
        prompts.remove(&key);
        self.persist_snapshot(&active, &prompts);
    }

    fn contains(&self, session_id: &str, turn_id: &str) -> bool {
        self.inner
            .lock()
            .expect("active turn lock")
            .contains(&(session_id.to_string(), turn_id.to_string()))
    }

    fn for_session(&self, session_id: &str) -> Vec<lash::TurnAddress> {
        self.inner
            .lock()
            .expect("active turn lock")
            .iter()
            .filter(|(active_session_id, _)| active_session_id == session_id)
            .map(|(session_id, turn_id)| lash::TurnAddress::new(session_id, turn_id))
            .collect()
    }

    fn prompt_for(&self, session_id: &str, turn_id: &str) -> Option<String> {
        self.prompts
            .lock()
            .expect("active turn prompt lock")
            .get(&(session_id.to_string(), turn_id.to_string()))
            .cloned()
    }

    fn persist(&self) {
        let active = self.inner.lock().expect("active turn lock");
        let prompts = self.prompts.lock().expect("active turn prompt lock");
        self.persist_snapshot(&active, &prompts);
    }

    fn persist_snapshot(
        &self,
        active: &BTreeSet<(String, String)>,
        prompts: &BTreeMap<(String, String), String>,
    ) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let prompts = prompts
            .iter()
            .map(
                |((session_id, turn_id), prompt)| PersistedActiveTurnPromptRef {
                    session_id,
                    turn_id,
                    prompt,
                },
            )
            .collect();
        let bytes = serde_json::to_vec(&PersistedActiveTurnsRef {
            turns: active,
            prompts,
        })
        .expect("serialize active turns");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .unwrap_or_else(|err| panic!("write active turns `{}`: {err}", temporary.display()));
        std::fs::rename(&temporary, path).unwrap_or_else(|err| {
            panic!(
                "replace active turns `{}` from `{}`: {err}",
                path.display(),
                temporary.display()
            )
        });
    }
}

#[derive(Debug, Serialize)]
struct CommandAccepted {
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct ProcessCancelAccepted {
    accepted: bool,
    operation_id: String,
    process_id: String,
}

#[derive(Clone, Debug, Serialize)]
struct TurnCancelReceipt {
    address: lash::TurnAddress,
    outcome: lash::TurnCancelOutcome,
    terminal: Option<lash::TurnTerminal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal_error: Option<lash::runtime::RuntimeError>,
}

#[derive(Clone, Debug, Serialize)]
struct TurnCancelResponse {
    accepted: bool,
    cancellations: Vec<TurnCancelReceipt>,
}

/// Best-effort [`ProcessEventSink`](lash::process::ProcessEventSink) that hands
/// each appended process event to a channel (ADR 0017). `emit` runs inline on
/// the registry append path, so it must return fast: it does no I/O, only a
/// non-blocking `try_send`. Dropping on a full channel is intentional — the
/// durable event log (`events_after`) is the reconcile source, not this feed.
#[derive(Clone)]
struct ChannelProcessEventSink {
    tx: mpsc::Sender<lash::process::ProcessEvent>,
}

impl ChannelProcessEventSink {
    fn new(tx: mpsc::Sender<lash::process::ProcessEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl lash::process::ProcessEventSink for ChannelProcessEventSink {
    async fn emit(&self, event: &lash::process::ProcessEvent) {
        // Non-blocking: drop on a full channel rather than slow every append.
        let _ = self.tx.try_send(event.clone());
    }
}

#[derive(Clone)]
struct WorkbenchQueuedWorkSubmitter {
    session_ids: WorkbenchSessionIds,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    restate_ingress_url: String,
    restate_http: reqwest::Client,
    active_turns: ActiveTurns,
}

#[async_trait]
impl lash::runtime::QueuedWorkRunHandle for WorkbenchQueuedWorkSubmitter {
    async fn run_queued_work(
        &self,
        request: lash::runtime::QueuedWorkRunRequest,
    ) -> std::result::Result<(), lash::runtime::QueuedWorkRunError> {
        let session_id = request
            .session_id
            .unwrap_or_else(|| self.session_ids.current());
        // A trigger process may finish while a foreground turn still owns this
        // session's ingress. Its wake stays in the durable queued-work store;
        // terminalization calls `claim_and_run_pending` again after releasing
        // the lease, so submitting a competing queued turn here is both
        // unnecessary and unsafe.
        if !self.active_turns.for_session(&session_id).is_empty() {
            return Ok(());
        }
        if !self
            .has_queued_work(&session_id)
            .await
            .map_err(lash::runtime::QueuedWorkRunError::terminal)?
        {
            return Ok(());
        }
        let workflow_request = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: format!("workbench-queued-{}", uuid::Uuid::new_v4()),
            session_id: session_id.clone(),
            reason: request.reason,
            batch_ids: Vec::new(),
            drain_id: None,
        };
        self.active_turns
            .insert(&session_id, &workflow_request.turn_id);
        if let Err(err) = restate::submit_queued_turn_request(
            &self.restate_http,
            &self.restate_ingress_url,
            &workflow_request,
        )
        .await
        {
            self.active_turns
                .remove(&session_id, &workflow_request.turn_id);
            return Err(lash::runtime::QueuedWorkRunError::transient(
                PluginError::Session(err.to_string()),
            ));
        }
        Ok(())
    }
}

impl WorkbenchQueuedWorkSubmitter {
    async fn has_queued_work(&self, session_id: &str) -> std::result::Result<bool, PluginError> {
        let store = self
            .store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.to_string(),
                relation: lash::persistence::SessionRelation::default(),
                policy: lash::runtime::SessionPolicy::default(),
            })
            .await
            .map_err(lash::runtime::RuntimeEffectControllerError::from)?;
        let queued = store
            .list_queued_work(session_id)
            .await
            .map_err(lash::runtime::RuntimeEffectControllerError::from)?;
        let next_turn_inputs = store
            .list_pending_turn_inputs(session_id)
            .await
            .map_err(lash::runtime::RuntimeEffectControllerError::from)?
            .into_iter()
            .any(|input| {
                matches!(
                    input.ingress,
                    lash::persistence::TurnInputIngress::NextTurn
                )
            });
        Ok(!queued.is_empty() || next_turn_inputs)
    }
}

#[cfg(test)]
struct NoopQueuedWorkRunHandle;

#[cfg(test)]
#[async_trait]
impl lash::runtime::QueuedWorkRunHandle for NoopQueuedWorkRunHandle {
    async fn run_queued_work(
        &self,
        _request: lash::runtime::QueuedWorkRunRequest,
    ) -> std::result::Result<(), lash::runtime::QueuedWorkRunError> {
        Ok(())
    }
}

#[cfg(test)]
fn inert_queued_work_driver() -> lash::runtime::QueuedWorkDriver {
    lash::runtime::QueuedWorkDriver::new(Arc::new(NoopQueuedWorkRunHandle))
}

#[cfg(test)]
struct NoopProcessRunHandle;

#[cfg(test)]
#[async_trait]
impl lash::process::ProcessRunHandle for NoopProcessRunHandle {
    async fn claim_and_run_pending(&self) -> std::result::Result<(), PluginError> {
        Ok(())
    }
}

/// A driver that reads the registry directly (no external run handle) — enough
/// for tests that build state but do not drive process execution.
#[cfg(test)]
fn inert_process_work_driver(
    registry: Arc<dyn lash::process::ProcessRegistry>,
) -> lash::process::ProcessWorkDriver {
    lash::process::ProcessWorkDriver::new(registry, Arc::new(NoopProcessRunHandle))
}

#[derive(Debug, Serialize)]
struct WorkItem {
    process: WorkProcess,
    events: Vec<WorkEvent>,
    kind: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct WorkProcess {
    process_id: String,
    graph_key: String,
    lifecycle: lash::process::ProcessStatus,
    status_label: String,
    terminal: bool,
    error: Option<String>,
    created_at_ms: u64,
    updated_at_ms: u64,
    input: Value,
    external_ref: Option<Value>,
    child_session_id: Option<String>,
    label: String,
}

#[derive(Debug, Serialize)]
struct WorkEvent {
    sequence: u64,
    event_type: String,
    occurred_at_ms: u64,
    payload: Value,
}

#[derive(Debug, Serialize)]
struct WorkAwaitResult {
    process_id: String,
    outcome: lash::process::ProcessAwaitOutput,
    /// Reconciled from the durable log at terminal (ADR 0017): the authoritative,
    /// complete record, unlike the best-effort event sink.
    events: Vec<WorkAwaitEvent>,
}

#[derive(Debug, Serialize)]
struct WorkAwaitEvent {
    sequence: u64,
    event_type: String,
}
