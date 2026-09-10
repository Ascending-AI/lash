use super::*;
use lash::TurnId;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) core: LashCore,
    /// The dialect new sessions are created with, from `LASH_RUNBOOK_DIALECT`.
    ///
    /// A plain field rather than a `cfg(test)` fork: forking it meant the
    /// production and test builds of `session_builder` differed by
    /// construction, so no test could ever reach the TypeScript branch of the
    /// code that ships.
    pub(crate) rlm_dialect: lash::rlm::RlmDialect,
    pub(crate) attachment_store: Arc<dyn lash::persistence::AttachmentStore>,
    /// The deployment's session-store factory, retained beside the core it was
    /// built with because it is also this host's attachment **root authority**
    /// (`lash::persistence::AttachmentRootSet`). Store-growth maintenance needs
    /// it under both hats — to open a session's store for `vacuum`, and to hand
    /// `reclaim_unreferenced_attachments` an explicit root set — so the route
    /// names it rather than reaching into the core for a store it did not
    /// choose.
    pub(crate) session_store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    pub(crate) trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    pub(crate) process_observer: lash::process::ProcessWorkObserver,
    pub(crate) sessions: WorkbenchSessions,
    pub(crate) messages: Arc<Mutex<Vec<ChatMessage>>>,
    pub(crate) selected_model: Arc<Mutex<ModelSelection>>,
    pub(crate) web_configured: bool,
    pub(crate) trace_sink: Option<Arc<dyn TraceSink>>,
    pub(crate) lashlang_execution: Arc<TraceLashlangGraphStore>,
    pub(crate) event_tx: SessionEventRegistry,
    pub(crate) queued_work_driver: lash::runtime::NativeQueuedWork,
    pub(crate) restate_ingress_url: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) restate_admin_url: String,
    pub(crate) restate_http: reqwest::Client,
    pub(crate) restate_cron_job_keys: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    pub(crate) mail_world: mail::MailWorld,
    pub(crate) active_turns: ActiveTurns,
    pub(crate) authorization: WorkbenchAuthorization,
    pub(crate) approvals: approvals::WorkbenchApprovals,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) model_variant: Option<String>,
    pub(crate) web_configured: bool,
    pub(crate) model_variants: Vec<&'static str>,
    pub(crate) session_id: String,
    /// The operator's name for this session, or its id when they gave none.
    pub(crate) session_name: String,
    /// The language id this session recorded, for the dialect badge.
    pub(crate) rlm_dialect: &'static str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ModelSelection {
    pub(crate) model: String,
    pub(crate) model_variant: Option<String>,
}

impl ModelSelection {
    pub(crate) fn from_spec(model: &lash::ModelSpec) -> Self {
        Self {
            model: model.id.clone(),
            model_variant: model.variant.effort().map(str::to_string),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StateSnapshot {
    pub(crate) settings: Settings,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) observation: RemoteSessionObservation,
    pub(crate) product_events: ProductEventSnapshot,
    pub(crate) active_turns: Vec<lash::TurnAddress>,
    pub(crate) pending_turn_inputs: Vec<lash::PendingTurnInput>,
    pub(crate) queued_work: Vec<lash::persistence::QueuedWorkBatch>,
    pub(crate) turn_input_applications: Vec<lash::remote::observations::RemoteTurnInputApplication>,
    pub(crate) turn_failure_settlements: Vec<lash::TurnFailureSettlement>,
    pub(crate) usage: lash::usage::SessionUsageReport,
    pub(crate) pending_approvals: Vec<approvals::PendingApproval>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StateReadSnapshot {
    #[serde(flatten)]
    pub(crate) state: StateSnapshot,
    pub(crate) transcript: Vec<TranscriptRow>,
}

impl std::ops::Deref for StateReadSnapshot {
    type Target = StateSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

#[derive(Clone, Debug)]
pub(crate) enum WorkbenchAuthorizationAction {
    Observe {
        session_id: String,
    },
    EnqueueTurn {
        session_id: String,
    },
    EnqueueTurnInput {
        session_id: String,
    },
    CancelTurn {
        session_id: String,
    },
    ManageQueuedWork {
        session_id: String,
    },
    /// Deployment-wide operator policy. Approval decisions are deliberately
    /// separate from chat/session participation.
    ManageApprovals,
    /// Destructive, deployment-wide maintenance. It is deliberately not
    /// session-scoped: no chat participant should ever be able to reach it.
    PruneTriggerMutationReceipts,
    /// Destructive, deployment-wide store-growth maintenance: trigger
    /// occurrence reclamation, session-store vacuum, and attachment
    /// reclamation. Operator-only for the same reason
    /// [`WorkbenchAuthorizationAction::PruneTriggerMutationReceipts`] is — it
    /// deletes durable rows and bytes across sessions, and the caller owns the
    /// safety argument.
    RunStoreMaintenance,
}

pub(crate) trait WorkbenchAuthorizer: Send + Sync {
    fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError>;
}

#[derive(Clone)]
pub(crate) struct WorkbenchAuthorization {
    pub(crate) authorizer: Arc<dyn WorkbenchAuthorizer>,
}

impl WorkbenchAuthorization {
    pub(crate) fn allow_all() -> Self {
        Self::with_authorizer(Arc::new(AllowAllWorkbenchAuthorizer))
    }

    pub(crate) fn with_authorizer(authorizer: Arc<dyn WorkbenchAuthorizer>) -> Self {
        Self { authorizer }
    }

    pub(crate) fn authorize(&self, action: WorkbenchAuthorizationAction) -> Result<(), AppError> {
        self.authorizer.authorize(&action)
    }
}

pub(crate) struct AllowAllWorkbenchAuthorizer;

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
            WorkbenchAuthorizationAction::ManageApprovals => {}
            WorkbenchAuthorizationAction::PruneTriggerMutationReceipts => {}
            WorkbenchAuthorizationAction::RunStoreMaintenance => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ChatMessageProvenance {
    TurnOutput { turn_id: TurnId },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChatMessage {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attachments: Vec<ChatAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provenance: Option<ChatMessageProvenance>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ChatAttachment {
    pub(crate) attachment_id: String,
    pub(crate) retrieve_url: String,
}

impl ChatAttachment {
    pub(crate) fn from_id(attachment_id: impl Into<String>) -> Self {
        let attachment_id = attachment_id.into();
        Self {
            retrieve_url: attachment_retrieve_url(&attachment_id),
            attachment_id,
        }
    }
}

pub(crate) fn attachment_retrieve_url(attachment_id: &str) -> String {
    let encoded =
        percent_encoding::utf8_percent_encode(attachment_id, percent_encoding::NON_ALPHANUMERIC);
    format!("/api/attachments/{encoded}")
}

/// The id of the optimistic user row this workbench publishes when a send is
/// accepted. It lives in the workbench's own id namespace — symmetric with
/// `workbench-assistant:{turn_id}` — because the UI owns the rows it renders.
/// The runtime's committed copy of the same text keeps its runtime-minted id
/// and is correlated by `MessageOrigin::TurnInput`, never by id shape
/// (FIG-972).
pub(crate) fn workbench_turn_user_message_id(turn_id: &TurnId) -> String {
    format!("workbench-user:{turn_id}")
}

pub(crate) fn workbench_turn_id_from_user_message_id(message_id: &str) -> Option<&str> {
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
pub(crate) fn workbench_turn_assistant_message_id(turn_id: &TurnId) -> String {
    format!("workbench-assistant:{turn_id}")
}

pub(crate) fn workbench_turn_id_from_assistant_message_id(message_id: &str) -> Option<&str> {
    message_id.strip_prefix("workbench-assistant:")
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TranscriptRow {
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
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tools: Vec<TranscriptTool>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranscriptTool {
    DurableSummary {
        operation: String,
        status: &'static str,
    },
    Omitted {
        count: usize,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnRequest {
    pub(crate) text: String,
    pub(crate) model: Option<String>,
    pub(crate) model_variant: Option<String>,
    #[serde(default)]
    pub(crate) attachment_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AttachmentUploadRequest {
    pub(crate) name: String,
    pub(crate) mime: String,
    pub(crate) data_base64: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AttachmentUploadResponse {
    pub(crate) attachment: lash::attachments::AttachmentRef,
    pub(crate) retrieve_url: String,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnInputIngressRequest {
    ActiveTurn,
    NextTurn,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TurnInputRequest {
    pub(crate) text: String,
    pub(crate) ingress: TurnInputIngressRequest,
}

/// What `/api/turn` did with a send.
///
/// A session runs one turn at a time, so "accepted" alone cannot describe the
/// outcome: a send that arrives while a turn is running is admitted as the next
/// turn's input rather than started now, and the caller has to be able to tell
/// the two apart (FIG-1000).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TurnAccepted {
    pub(crate) accepted: bool,
    pub(crate) queued: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) queued_input: Option<TurnInputReceipt>,
}

impl TurnAccepted {
    pub(crate) fn started() -> Self {
        Self {
            accepted: true,
            queued: false,
            queued_input: None,
        }
    }

    pub(crate) fn queued(receipt: TurnInputReceipt) -> Self {
        Self {
            accepted: true,
            queued: true,
            queued_input: Some(receipt),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TurnInputReceipt {
    pub(crate) accepted: bool,
    pub(crate) input_id: String,
    pub(crate) ingress: lash::persistence::TurnInputIngress,
    pub(crate) state: lash::persistence::TurnInputState,
    pub(crate) text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProductEventsQuery {
    pub(crate) cursor: Option<u64>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct SessionQuery {
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

impl SessionQuery {
    pub(crate) fn resolve(&self, state: &AppState) -> Result<String, AppError> {
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

    pub(crate) fn is_explicit(&self) -> bool {
        self.session_id.is_some()
    }
}

/// The create-a-session request: a name the operator can read, and the dialect
/// the session is pinned to for its durable lifetime.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct SessionCreateRequest {
    #[serde(default)]
    pub(crate) name: Option<String>,
    /// A registered RLM language id. Absent means the deployment's ambient
    /// `LASH_RUNBOOK_DIALECT`; an unregistered id is refused, never defaulted.
    #[serde(default)]
    pub(crate) dialect: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct SessionSelectRequest {
    pub(crate) session_id: String,
}

/// One session as the selector renders it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: String,
    pub(crate) name: String,
    /// The dialect this session recorded, read back from the session itself.
    pub(crate) dialect: &'static str,
    pub(crate) created_at_ms: i64,
    pub(crate) last_active_ms: i64,
    pub(crate) current: bool,
}

/// The session list, with the menu a create form has to offer.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SessionListResponse {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) current_session_id: String,
    /// Every registered RLM language id, from the substrate's own dialect
    /// enumeration rather than a list this host writes down.
    pub(crate) dialects: Vec<&'static str>,
    /// The dialect a session gets when the create form offers no choice.
    pub(crate) default_dialect: &'static str,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) enum ButtonChoice {
    Red,
    Blue,
}

impl ButtonChoice {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Red => "Red",
            Self::Blue => "Blue",
        }
    }

    pub(crate) fn lower(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Blue => "blue",
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ButtonEventRequest {
    pub(crate) button: ButtonChoice,
    pub(crate) model: Option<String>,
    pub(crate) model_variant: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddAccountRequest {
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InjectMessageRequest {
    pub(crate) title: String,
    pub(crate) text: String,
    pub(crate) model: Option<String>,
    pub(crate) model_variant: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum StreamItem {
    Message {
        message: ChatMessage,
    },
    TurnInput {
        receipt: TurnInputReceipt,
    },
    ModelCallRecorded {
        record: lash::remote::llm::RemoteLlmCallRecord,
    },
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        /// How the turn ended. A viewer that already rendered this turn's
        /// UI-owned rows needs this to know whether they still stand for
        /// anything (FIG-1000): a failed turn's rows have been retired from the
        /// lane and the viewer must re-derive from the authoritative snapshot.
        #[serde(default, skip_serializing_if = "TurnDoneOutcome::is_completed")]
        outcome: TurnDoneOutcome,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnDoneOutcome {
    /// The turn reached a terminal outcome of its own — finished, stopped, or
    /// cancelled. Whatever it committed is durable truth.
    #[default]
    Completed,
    /// The turn never reached its own outcome: it failed before or at commit,
    /// so nothing it optimistically claimed is durable.
    Failed,
}

impl TurnDoneOutcome {
    pub(crate) fn is_completed(&self) -> bool {
        *self == Self::Completed
    }
}

pub(crate) const PUBLIC_TURN_FAILURE_MESSAGE: &str = "turn could not be completed";
pub(crate) const REPLAY_DIVERGENCE_TURN_FAILURE_MESSAGE: &str =
    "durable replay diverged for this turn; retry after the deployment is stable";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ProductEvent {
    pub(crate) event_id: String,
    pub(crate) sequence: u64,
    #[serde(flatten)]
    pub(crate) item: StreamItem,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ProductEventSnapshot {
    pub(crate) cursor: u64,
    pub(crate) events: Vec<ProductEvent>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ProductEventHistory {
    pub(crate) cursor: u64,
    pub(crate) events: Vec<ProductEvent>,
    #[serde(default)]
    pub(crate) event_ids: BTreeSet<String>,
    /// UI-owned user rows that have been correlated with durable turn-input
    /// provenance. This survives frame-scoped read models, which may no longer
    /// expose the old frame's messages on a later `/api/state` read.
    #[serde(default)]
    pub(crate) committed_user_turn_ids: BTreeSet<TurnId>,
}

impl ProductEventHistory {
    pub(crate) fn normalized(mut self) -> Self {
        self.cursor = self
            .events
            .last()
            .map_or(self.cursor, |event| self.cursor.max(event.sequence));
        self.event_ids
            .extend(self.events.iter().map(|event| event.event_id.clone()));
        self
    }
}

pub(crate) const PRODUCT_EVENT_LOG_FORMAT_VERSION: u32 = 2;

#[derive(Serialize)]
pub(crate) struct PersistedProductEventLog<'a> {
    pub(crate) format_version: u32,
    pub(crate) histories: &'a HashMap<String, ProductEventHistory>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProductEventLogDecodeError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("field `format_version` must be an unsigned 32-bit integer")]
    InvalidFormatVersion,
    #[error("format version mismatch: expected {expected}, found {found}")]
    FormatVersionMismatch { expected: u32, found: u32 },
    #[error("field `histories` is required")]
    MissingHistories,
    #[error("field `{field}` could not be decoded: {source}")]
    Field {
        field: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "unversioned product event log is not supported; expected a root object with `format_version` and `histories`"
    )]
    UnversionedRoot,
    #[error("product event log root must be a JSON object")]
    InvalidRoot,
}

#[derive(Debug, thiserror::Error)]
#[error("decode product event log `{path}`: {source}")]
pub(crate) struct ProductEventLogLoadError {
    pub(crate) path: PathBuf,
    #[source]
    pub(crate) source: ProductEventLogDecodeError,
}

pub(crate) fn decode_product_event_histories(
    bytes: &[u8],
) -> Result<HashMap<String, ProductEventHistory>, ProductEventLogDecodeError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(ProductEventLogDecodeError::InvalidJson)?;
    let root = value
        .as_object()
        .ok_or(ProductEventLogDecodeError::InvalidRoot)?;

    // Released logs used arbitrary session ids as root keys. Treat
    // `format_version` as the wrapper discriminator only when its value cannot
    // itself be a released history object or legacy event array.
    let versioned = root
        .get("format_version")
        .is_some_and(|value| !value.is_object() && !value.is_array());
    if versioned {
        let found = root
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(ProductEventLogDecodeError::InvalidFormatVersion)?;
        if found != PRODUCT_EVENT_LOG_FORMAT_VERSION {
            return Err(ProductEventLogDecodeError::FormatVersionMismatch {
                expected: PRODUCT_EVENT_LOG_FORMAT_VERSION,
                found,
            });
        }
        let histories = root
            .get("histories")
            .cloned()
            .ok_or(ProductEventLogDecodeError::MissingHistories)?;
        return serde_json::from_value::<HashMap<String, ProductEventHistory>>(histories)
            .map(|histories| {
                histories
                    .into_iter()
                    .map(|(session_id, history)| (session_id, history.normalized()))
                    .collect()
            })
            .map_err(|source| ProductEventLogDecodeError::Field {
                field: "histories",
                source,
            });
    }

    Err(ProductEventLogDecodeError::UnversionedRoot)
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ProductStreamItem {
    Event { event: ProductEvent },
    Resync { snapshot: ProductEventSnapshot },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ObservationStreamItem {
    Cursor {
        cursor: String,
    },
    Observation {
        event: Box<Envelope<RemoteSessionObservationEvent>>,
    },
    ReplayGap {
        observation: Box<Envelope<RemoteSessionObservation>>,
        gap: Box<Envelope<RemoteLiveReplayGap>>,
    },
    TerminalReplacement {
        event: Box<Envelope<RemoteSessionObservationEvent>>,
        cursor: String,
    },
    ResidentReplacement {
        event: Box<Envelope<RemoteSessionObservationEvent>>,
        cursor: String,
    },
}

#[derive(Clone)]
pub(crate) struct SessionEventRegistry {
    pub(crate) histories: Arc<Mutex<HashMap<String, ProductEventHistory>>>,
    pub(crate) senders: Arc<Mutex<HashMap<String, broadcast::Sender<ProductEvent>>>>,
    pub(crate) channel_capacity: usize,
    pub(crate) path: Option<Arc<PathBuf>>,
}

impl SessionEventRegistry {
    #[cfg(test)]
    pub(crate) fn new(channel_capacity: usize) -> Self {
        Self {
            histories: Arc::new(Mutex::new(HashMap::new())),
            senders: Arc::new(Mutex::new(HashMap::new())),
            channel_capacity: channel_capacity.max(1),
            path: None,
        }
    }

    pub(crate) fn persistent(path: PathBuf, channel_capacity: usize) -> AnyhowResult<Self> {
        let histories = match std::fs::read(&path) {
            Ok(bytes) => decode_product_event_histories(&bytes).map_err(|source| {
                ProductEventLogLoadError {
                    path: path.clone(),
                    source,
                }
            })?,
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

    pub(crate) fn sender(&self, session_id: &str) -> broadcast::Sender<ProductEvent> {
        let mut senders = self.senders.lock_recover();
        senders
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(self.channel_capacity).0)
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn subscribe(&self, session_id: &str) -> broadcast::Receiver<ProductEvent> {
        self.sender(session_id).subscribe()
    }

    pub(crate) fn subscribe_after(
        &self,
        session_id: &str,
        cursor: u64,
    ) -> (Vec<ProductEvent>, broadcast::Receiver<ProductEvent>) {
        let receiver = self.sender(session_id).subscribe();
        let replay = self
            .histories
            .lock_recover()
            .get(session_id)
            .into_iter()
            .flat_map(|history| history.events.iter())
            .filter(|event| event.sequence > cursor)
            .cloned()
            .collect();
        (replay, receiver)
    }

    #[cfg(test)]
    pub(crate) fn publish(&self, session_id: &str, item: StreamItem) {
        self.publish_identified(
            session_id,
            format!("workbench-product-event:{}", uuid::Uuid::new_v4()),
            item,
        );
    }

    pub(crate) fn publish_identified(
        &self,
        session_id: &str,
        event_id: impl Into<String>,
        item: StreamItem,
    ) -> bool {
        let event_id = event_id.into();
        let event = {
            let mut histories = self.histories.lock_recover();
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

    pub(crate) fn snapshot(&self, session_id: &str) -> ProductEventSnapshot {
        let history = self
            .histories
            .lock_recover()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        ProductEventSnapshot {
            cursor: history.cursor,
            events: history.events,
        }
    }

    pub(crate) fn reconcile_settled(
        &self,
        session_id: &str,
        committed_message_ids: &BTreeSet<String>,
        committed_input_turn_ids: &BTreeSet<TurnId>,
        active_turn_ids: &BTreeSet<TurnId>,
    ) {
        let mut histories = self.histories.lock_recover();
        let Some(history) = histories.get_mut(session_id) else {
            return;
        };
        let committed_user_turn_ids = history
            .events
            .iter()
            .filter_map(|event| match &event.item {
                StreamItem::Message { message } => {
                    workbench_turn_id_from_user_message_id(&message.id)
                }
                StreamItem::TurnInput { .. }
                | StreamItem::ModelCallRecorded { .. }
                | StreamItem::Done { .. } => None,
            })
            .filter(|turn_id| committed_input_turn_ids.contains(*turn_id))
            .map(TurnId::from)
            .collect::<BTreeSet<_>>();
        let committed_status_before = history.committed_user_turn_ids.len();
        history
            .committed_user_turn_ids
            .extend(committed_user_turn_ids);
        let committed_status_changed =
            history.committed_user_turn_ids.len() != committed_status_before;
        let before = history.events.len();
        history.events.retain(|event| match &event.item {
            StreamItem::Message { message } => {
                if let Some(turn_id) = workbench_turn_id_from_user_message_id(&message.id) {
                    // Submitted user rows become session-scoped host state
                    // once the turn commits anywhere in the session graph.
                    // Until then they remain optimistic and retire with a turn
                    // that is no longer active (FIG-1000, FIG-1062).
                    active_turn_ids.contains(turn_id)
                        || history.committed_user_turn_ids.contains(turn_id)
                } else if let Some(turn_id) =
                    workbench_turn_id_from_assistant_message_id(&message.id)
                {
                    // The live assistant row is turn-scoped. The committed
                    // reply replaces it at settlement; old-frame replies then
                    // collapse naturally when the active frame changes. Its
                    // durable id is termination-dependent (FIG-984).
                    active_turn_ids.contains(turn_id)
                } else {
                    // Everything else in this lane is a mirror of a committed
                    // message and retires once that commit is readable.
                    !committed_message_ids.contains(&message.id)
                }
            }
            StreamItem::Done {
                turn_id: Some(turn_id),
                ..
            } => active_turn_ids.contains(turn_id),
            StreamItem::TurnInput { .. }
            | StreamItem::ModelCallRecorded { .. }
            | StreamItem::Done { turn_id: None, .. } => true,
        });
        if history.events.len() != before || committed_status_changed {
            self.persist_snapshot(&histories);
        }
    }

    /// Retire the transient product rows this workbench published on behalf of `turn_id`,
    /// reporting the message ids it removed.
    ///
    /// A turn that failed has no outcome for its optimistic rows to stand for —
    /// the losing side of a commit race commits nothing at all — so leaving them
    /// in the lane would broadcast, and replay to every later viewer, a
    /// conversation row durable truth does not have (FIG-1000). Provenance comes
    /// from the ids the workbench itself minted for the turn, never from parsing
    /// a runtime-minted id (FIG-972).
    ///
    /// Retirement drops the events and keeps their identities, exactly as
    /// settlement compaction does: a Restate replay that re-publishes the same
    /// row must be a no-op, not a resurrection of the row this just retired.
    pub(crate) fn retire_turn_rows(&self, session_id: &str, turn_id: &TurnId) -> BTreeSet<String> {
        let mut retired = BTreeSet::new();
        let mut histories = self.histories.lock_recover();
        let Some(history) = histories.get_mut(session_id) else {
            return retired;
        };
        history.events.retain(|event| {
            let StreamItem::Message { message } = &event.item else {
                return true;
            };
            let owned_by_turn = workbench_turn_id_from_user_message_id(&message.id)
                .or_else(|| workbench_turn_id_from_assistant_message_id(&message.id))
                .is_some_and(|owner| owner == turn_id);
            if owned_by_turn {
                retired.insert(message.id.clone());
            }
            !owned_by_turn
        });
        if !retired.is_empty() {
            self.persist_snapshot(&histories);
        }
        retired
    }

    pub(crate) fn remove(&self, session_id: &str) {
        let mut histories = self.histories.lock_recover();
        histories.remove(session_id);
        self.persist_snapshot(&histories);
        drop(histories);
        self.senders.lock_recover().remove(session_id);
    }

    pub(crate) fn persist_snapshot(&self, histories: &HashMap<String, ProductEventHistory>) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let bytes = serde_json::to_vec(&PersistedProductEventLog {
            format_version: PRODUCT_EVENT_LOG_FORMAT_VERSION,
            histories,
        })
        .expect("serialize product event log");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes).unwrap_or_else(|err| {
            panic!("write product event log `{}`: {err}", temporary.display())
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
    pub(crate) fn contains(&self, session_id: &str) -> bool {
        self.senders.lock_recover().contains_key(session_id)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct TriggerEnabledRequest {
    pub(crate) enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WorkbenchTriggerRegistration {
    // Keep these sibling names absent from the flattened core DTO: serde would
    // otherwise emit duplicate JSON keys with order-dependent browser values.
    #[serde(flatten)]
    pub(crate) registration: lash::triggers::TriggerRegistration,
    pub(crate) subscription_id: String,
    pub(crate) registrant_scope: String,
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
pub(crate) struct TriggerMutationResponse {
    pub(crate) changed: bool,
    pub(crate) registration: Option<lash::triggers::TriggerRegistration>,
}

/// The in-process turn registry and the session fence, under one lock.
///
/// Turn admission and session retirement race on exactly one fact — whether
/// this session may still start work — so the retirement marks live in the
/// same ledger the turn claim reads. A claim taken under this lock is either
/// ordered before the delete marked the session (and the delete then cancels
/// and settles it) or refused by the mark; there is no third interleaving.
///
/// Only the turns are persisted. The marks are an in-process ordering device:
/// after a restart the durable session tombstone is the authority, and every
/// admission read consults it as well (`AppState::admit_session`).
#[derive(Clone, Default)]
pub(crate) struct ActiveTurns {
    inner: Arc<Mutex<ActiveTurnLedger>>,
    pub(crate) prompts: Arc<Mutex<BTreeMap<(String, TurnId), ActiveTurnPrompt>>>,
    pub(crate) path: Option<Arc<PathBuf>>,
}

#[derive(Default)]
struct ActiveTurnLedger {
    turns: BTreeSet<(String, TurnId)>,
    retirements: BTreeMap<String, SessionRetirement>,
}

/// Where a session stands in retirement, as recorded by this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionRetirement {
    /// A delete is in flight, or its outcome is still unconfirmed. Turn
    /// admission refuses; a delete retry may proceed.
    Retiring,
    /// The durable tombstone is confirmed. Every session-bound surface refuses.
    Retired,
}

/// The outcome of claiming the idle slot for a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActiveTurnClaim {
    /// The turn now owns the session's single active slot.
    Claimed,
    /// Another turn owns the slot; the caller queues or refuses.
    Busy,
    /// The session is retiring or retired; no turn may start.
    Refused(SessionRetirement),
}

impl ActiveTurnClaim {
    pub(crate) fn is_claimed(self) -> bool {
        matches!(self, Self::Claimed)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveTurnPrompt {
    pub(crate) text: String,
    pub(crate) attachment_id: Option<String>,
}

/// Cleans up an active-turn claim unless its work-driver submission completes.
///
/// This guard lives inside the detached admission task. It therefore runs when
/// submission returns an error, the task is cancelled during runtime shutdown,
/// or the task unwinds after a panic. User-turn admission also retires its
/// optimistic row and publishes the terminal failure expected by the browser;
/// queued turns have no optimistic row, so they only release their claim.
pub(crate) struct ActiveTurnSubmissionGuard {
    pub(crate) active_turns: ActiveTurns,
    pub(crate) failure_publisher: Option<AppState>,
    pub(crate) session_id: String,
    pub(crate) turn_id: TurnId,
    pub(crate) armed: bool,
}

impl ActiveTurnSubmissionGuard {
    pub(crate) fn user_turn(state: &AppState, session_id: &str, turn_id: &TurnId) -> Self {
        Self {
            active_turns: state.active_turns.clone(),
            failure_publisher: Some(state.clone()),
            session_id: session_id.to_string(),
            turn_id: TurnId::from(turn_id.to_string()),
            armed: true,
        }
    }

    pub(crate) fn queued_turn(
        active_turns: ActiveTurns,
        session_id: &str,
        turn_id: &TurnId,
    ) -> Self {
        Self {
            active_turns,
            failure_publisher: None,
            session_id: session_id.to_string(),
            turn_id: TurnId::from(turn_id.to_string()),
            armed: true,
        }
    }

    pub(crate) fn complete(mut self) {
        self.armed = false;
    }
}

impl Drop for ActiveTurnSubmissionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let already_panicking = std::thread::panicking();
        let removal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.active_turns.remove(&self.session_id, &self.turn_id);
        }));
        let publication = self.failure_publisher.as_ref().map(|state| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.publish_turn_failed(&self.session_id, &self.turn_id);
            }))
        });
        let cleanup_panic = removal.err().or_else(|| publication.and_then(Result::err));
        if let Some(payload) = cleanup_panic {
            if already_panicking {
                eprintln!("turn admission cleanup panicked while preserving the original panic");
            } else {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct PersistedActiveTurns {
    pub(crate) turns: BTreeSet<(String, TurnId)>,
    #[serde(default)]
    pub(crate) prompts: Vec<PersistedActiveTurnPrompt>,
}

#[derive(Serialize)]
pub(crate) struct PersistedActiveTurnsRef<'a> {
    pub(crate) turns: &'a BTreeSet<(String, TurnId)>,
    pub(crate) prompts: Vec<PersistedActiveTurnPromptRef<'a>>,
}

#[derive(Deserialize)]
pub(crate) struct PersistedActiveTurnPrompt {
    pub(crate) session_id: String,
    pub(crate) turn_id: TurnId,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) attachment_id: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PersistedActiveTurnPromptRef<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) turn_id: &'a TurnId,
    pub(crate) prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attachment_id: Option<&'a str>,
}

impl ActiveTurns {
    pub(crate) fn persistent(path: PathBuf) -> AnyhowResult<Self> {
        let (turns, prompts) = match std::fs::read(&path) {
            Ok(bytes) => {
                let persisted: PersistedActiveTurns = serde_json::from_slice(&bytes)
                    .map_err(|error| {
                        let hint = serde_json::from_slice::<serde_json::Value>(&bytes)
                            .ok()
                            .filter(serde_json::Value::is_array)
                            .map(|_| {
                                "; legacy bare active turn set is no longer supported; \
                                 expected an object with `turns` and `prompts`"
                            })
                            .unwrap_or("");
                        anyhow::anyhow!("{error}{hint}")
                    })
                    .with_context(|| format!("decode active turns `{}`", path.display()))?;
                (
                    persisted.turns,
                    persisted
                        .prompts
                        .into_iter()
                        .map(|prompt| {
                            (
                                (prompt.session_id, prompt.turn_id),
                                ActiveTurnPrompt {
                                    text: prompt.prompt,
                                    attachment_id: prompt.attachment_id,
                                },
                            )
                        })
                        .collect(),
                )
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                (BTreeSet::new(), BTreeMap::new())
            }
            Err(err) => {
                return Err(err).with_context(|| format!("read active turns `{}`", path.display()));
            }
        };
        let active = Self {
            inner: Arc::new(Mutex::new(ActiveTurnLedger {
                turns,
                retirements: BTreeMap::new(),
            })),
            prompts: Arc::new(Mutex::new(prompts)),
            path: Some(Arc::new(path)),
        };
        active.persist();
        Ok(active)
    }

    #[cfg(test)]
    pub(crate) fn insert(&self, session_id: impl Into<String>, turn_id: impl Into<TurnId>) {
        self.insert_with_prompt(session_id, turn_id, None, None);
    }

    #[cfg(test)]
    pub(crate) fn insert_with_prompt(
        &self,
        session_id: impl Into<String>,
        turn_id: impl Into<TurnId>,
        prompt: Option<String>,
        attachment_id: Option<String>,
    ) {
        let key = (session_id.into(), turn_id.into());
        let mut ledger = self.inner.lock_recover();
        let mut prompts = self.prompts.lock_recover();
        ledger.turns.insert(key.clone());
        if let Some(prompt) = prompt {
            prompts.insert(
                key,
                ActiveTurnPrompt {
                    text: prompt,
                    attachment_id,
                },
            );
        }
        self.persist_snapshot(&ledger.turns, &prompts);
    }

    pub(crate) fn try_insert_for_idle_session(
        &self,
        session_id: &str,
        turn_id: &TurnId,
    ) -> ActiveTurnClaim {
        self.try_insert_with_prompt_for_idle_session(session_id, turn_id, None, None)
    }

    /// Claim the session's single active slot for `turn_id`, unless the session
    /// is busy or fenced.
    ///
    /// The retirement read and the slot claim happen under one lock: a delete
    /// that marks the session before this claim refuses it, and a claim that
    /// lands first is a turn the delete will find in the registry and cancel.
    pub(crate) fn try_insert_with_prompt_for_idle_session(
        &self,
        session_id: &str,
        turn_id: &TurnId,
        prompt: Option<String>,
        attachment_id: Option<String>,
    ) -> ActiveTurnClaim {
        let mut ledger = self.inner.lock_recover();
        if let Some(retirement) = ledger.retirements.get(session_id) {
            return ActiveTurnClaim::Refused(*retirement);
        }
        if ledger
            .turns
            .iter()
            .any(|(active_session_id, _)| active_session_id == session_id)
        {
            return ActiveTurnClaim::Busy;
        }
        let key = (session_id.to_string(), turn_id.clone());
        let mut prompts = self.prompts.lock_recover();
        ledger.turns.insert(key.clone());
        if let Some(prompt) = prompt {
            prompts.insert(
                key,
                ActiveTurnPrompt {
                    text: prompt,
                    attachment_id,
                },
            );
        }
        self.persist_snapshot(&ledger.turns, &prompts);
        ActiveTurnClaim::Claimed
    }

    pub(crate) fn remove(&self, session_id: &str, turn_id: &TurnId) {
        let key = (session_id.to_string(), turn_id.clone());
        let mut ledger = self.inner.lock_recover();
        let mut prompts = self.prompts.lock_recover();
        ledger.turns.remove(&key);
        prompts.remove(&key);
        self.persist_snapshot(&ledger.turns, &prompts);
    }

    pub(crate) fn contains(&self, session_id: &str, turn_id: &TurnId) -> bool {
        self.inner
            .lock_recover()
            .turns
            .contains(&(session_id.to_string(), turn_id.clone()))
    }

    pub(crate) fn for_session(&self, session_id: &str) -> Vec<lash::TurnAddress> {
        self.inner
            .lock_recover()
            .turns
            .iter()
            .filter(|(active_session_id, _)| active_session_id == session_id)
            .map(|(session_id, turn_id)| lash::TurnAddress::new(session_id, turn_id))
            .collect()
    }

    /// Mark `session_id` as retiring, so no new turn claims its slot while the
    /// delete runs. Idempotent: a session already retiring or retired keeps its
    /// mark, and the return value says whether this call placed one.
    pub(crate) fn begin_retirement(&self, session_id: &str) -> bool {
        let mut ledger = self.inner.lock_recover();
        if ledger.retirements.contains_key(session_id) {
            return false;
        }
        ledger
            .retirements
            .insert(session_id.to_string(), SessionRetirement::Retiring);
        true
    }

    /// Record that the durable tombstone for `session_id` is confirmed.
    pub(crate) fn confirm_retirement(&self, session_id: &str) {
        self.inner
            .lock_recover()
            .retirements
            .insert(session_id.to_string(), SessionRetirement::Retired);
    }

    /// Lift a retiring mark after the delete definitively failed and the
    /// session remains live. A confirmed retirement is never lifted: a deleted
    /// session id cannot come back.
    pub(crate) fn abandon_retirement(&self, session_id: &str) {
        let mut ledger = self.inner.lock_recover();
        if ledger.retirements.get(session_id) == Some(&SessionRetirement::Retiring) {
            ledger.retirements.remove(session_id);
        }
    }

    pub(crate) fn retirement(&self, session_id: &str) -> Option<SessionRetirement> {
        self.inner
            .lock_recover()
            .retirements
            .get(session_id)
            .copied()
    }

    pub(crate) fn prompt_for(
        &self,
        session_id: &str,
        turn_id: &TurnId,
    ) -> Option<ActiveTurnPrompt> {
        self.prompts
            .lock_recover()
            .get(&(session_id.to_string(), turn_id.clone()))
            .cloned()
    }

    pub(crate) fn persist(&self) {
        let ledger = self.inner.lock_recover();
        let prompts = self.prompts.lock_recover();
        self.persist_snapshot(&ledger.turns, &prompts);
    }

    fn persist_snapshot(
        &self,
        active: &BTreeSet<(String, TurnId)>,
        prompts: &BTreeMap<(String, TurnId), ActiveTurnPrompt>,
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
                    prompt: &prompt.text,
                    attachment_id: prompt.attachment_id.as_deref(),
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
pub(crate) struct CommandAccepted {
    pub(crate) accepted: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProcessCancelAccepted {
    pub(crate) accepted: bool,
    pub(crate) operation_id: String,
    pub(crate) process_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TurnCancelResponse {
    pub(crate) accepted: bool,
    pub(crate) cancellations: Vec<TurnCancelReceipt>,
}

/// Host-visible notice the workbench renders when the durable-process worker
/// reports a fault.
///
/// Driving pending processes is an *admission* call: it hands claimable rows to
/// execution and returns, so a claim, read, write, release, or worklist-scan
/// failure that happens after admission has no return value left to ride. The
/// worker reports it as a typed
/// [`ProcessWorkerFault`](lash::process::ProcessWorkerFault) on the same
/// unconditional sink the workbench already installs for process events, and
/// this notice is the host end of that contract: typed fault in, one rendered
/// line out on the workbench's stderr process log (the browser feed carries
/// process *events*; a worker fault is an operator signal, not a UI row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerFaultNotice {
    pub(crate) kind: &'static str,
    pub(crate) process_id: Option<String>,
    pub(crate) operation: Option<String>,
    pub(crate) error: String,
}

impl WorkerFaultNotice {
    pub(crate) fn from_fault(fault: &lash::process::ProcessWorkerFault) -> Self {
        match fault {
            lash::process::ProcessWorkerFault::RecoveryBackendError {
                process_id,
                operation,
                error,
            } => Self {
                kind: "recovery-backend-error",
                process_id: Some(process_id.clone()),
                // The typed operation is why this notice is actionable: it says
                // which registry call failed without parsing the message.
                operation: Some(format!("{operation:?}")),
                error: error.clone(),
            },
            lash::process::ProcessWorkerFault::RecoveryRunFailed { process_id, error } => Self {
                kind: "recovery-run-failed",
                process_id: Some(process_id.clone()),
                operation: None,
                error: error.clone(),
            },
            // Pass-scoped: no row owns a scan that gave up part-way, so the
            // notice carries no process id rather than blaming one.
            lash::process::ProcessWorkerFault::WorklistScanIncomplete { error } => Self {
                kind: "worklist-scan-incomplete",
                process_id: None,
                operation: None,
                error: error.clone(),
            },
            other => Self {
                kind: "unknown-worker-fault",
                process_id: None,
                operation: None,
                error: format!("{other:?}"),
            },
        }
    }

    pub(crate) fn render(&self) -> String {
        format!(
            "kind={} process={} operation={} error={}",
            self.kind,
            self.process_id.as_deref().unwrap_or("-"),
            self.operation.as_deref().unwrap_or("-"),
            self.error
        )
    }
}

/// Best-effort [`ProcessEventSink`](lash::process::ProcessEventSink) that hands
/// each appended process event to a channel (ADR 0017). `emit` runs inline on
/// the registry append path, so it must return fast: it does no I/O, only a
/// non-blocking `try_send`. Dropping on a full channel is intentional — the
/// durable event log (`events_after`) is the reconcile source, not this feed.
///
/// The same sink carries the durable-process worker's typed faults, which have
/// no durable log to reconcile from: dropping one loses the only report that a
/// pass lost a row, so the fault channel is sized for the whole feed rather
/// than sharing the event channel's drop-under-pressure budget.
#[derive(Clone)]
pub(crate) struct ChannelProcessEventSink {
    pub(crate) tx: mpsc::Sender<lash::process::ProcessEvent>,
    pub(crate) faults: mpsc::Sender<WorkerFaultNotice>,
}

impl ChannelProcessEventSink {
    pub(crate) fn new(
        tx: mpsc::Sender<lash::process::ProcessEvent>,
        faults: mpsc::Sender<WorkerFaultNotice>,
    ) -> Self {
        Self { tx, faults }
    }
}

#[async_trait]
impl lash::process::ProcessEventSink for ChannelProcessEventSink {
    async fn emit(&self, event: &lash::process::ProcessEvent) {
        // Non-blocking: drop on a full channel rather than slow every append.
        let _ = self.tx.try_send(event.clone());
    }

    async fn emit_worker_fault(&self, fault: &lash::process::ProcessWorkerFault) {
        // Runs on the worker's own path, so it stays non-blocking like `emit`.
        let _ = self.faults.try_send(WorkerFaultNotice::from_fault(fault));
    }
}

#[derive(Clone)]
pub(crate) struct WorkbenchQueuedWorkSubmitter {
    pub(crate) sessions: WorkbenchSessions,
    pub(crate) store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    pub(crate) restate_ingress_url: String,
    pub(crate) restate_http: reqwest::Client,
    pub(crate) active_turns: ActiveTurns,
}

#[async_trait]
impl lash::runtime::QueuedWorkRunHandle for WorkbenchQueuedWorkSubmitter {
    async fn run_queued_work(
        &self,
        request: lash::runtime::QueuedWorkRunRequest,
    ) -> std::result::Result<(), lash::runtime::QueuedWorkRunError> {
        let session_id = request
            .session_id
            .unwrap_or_else(|| self.sessions.current());
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
            turn_id: TurnId::from(format!("workbench-queued-{}", uuid::Uuid::new_v4())),
            session_id: session_id.clone(),
            reason: request.reason,
            batch_ids: Vec::new(),
            drain_id: None,
        };
        let cleanup = ActiveTurnSubmissionGuard::queued_turn(
            self.active_turns.clone(),
            &session_id,
            &workflow_request.turn_id,
        );
        if !self
            .active_turns
            .try_insert_for_idle_session(&session_id, &workflow_request.turn_id)
            .is_claimed()
        {
            cleanup.complete();
            return Ok(());
        }
        let submission = tokio::spawn(submit_tracked_queued_turn(
            cleanup,
            self.restate_http.clone(),
            self.restate_ingress_url.clone(),
            workflow_request,
        ));
        match submission.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(lash::runtime::QueuedWorkRunError::transient(
                PluginError::Session(error.to_string()),
            )),
            Err(error) => Err(lash::runtime::QueuedWorkRunError::transient(
                PluginError::Session(format!("queued-turn submission task failed: {error}")),
            )),
        }
    }
}

impl WorkbenchQueuedWorkSubmitter {
    pub(crate) async fn has_queued_work(
        &self,
        session_id: &str,
    ) -> std::result::Result<bool, PluginError> {
        let store = self
            .store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: lash::persistence::SessionRelation::default(),
                policy: lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded),
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
            .any(|input| matches!(input.ingress, lash::persistence::TurnInputIngress::NextTurn));
        Ok(!queued.is_empty() || next_turn_inputs)
    }
}

#[cfg(test)]
pub(crate) struct NoopQueuedWorkRunHandle;

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
pub(crate) fn inert_queued_work() -> lash::runtime::NativeQueuedWork {
    lash::runtime::NativeQueuedWork::new(Arc::new(NoopQueuedWorkRunHandle))
}

#[cfg(test)]
pub(crate) fn inert_queued_work_port() -> Arc<dyn lash::runtime::QueuedWorkSubstrate> {
    Arc::new(lash::runtime::NativeQueuedWork::new(Arc::new(
        NoopQueuedWorkRunHandle,
    )))
}

// Process work is now resolved through LashCore's substrate port.
// The AppState no longer mirrors that driver as a second source of truth.

#[derive(Debug, Serialize)]
pub(crate) struct WorkItem {
    pub(crate) process: WorkProcess,
    pub(crate) events: Vec<WorkEvent>,
    pub(crate) kind: String,
    pub(crate) label: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkProcess {
    pub(crate) process_id: String,
    pub(crate) graph_key: String,
    pub(crate) lifecycle: lash::process::ProcessStatus,
    pub(crate) status_label: String,
    pub(crate) terminal: bool,
    pub(crate) error: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) input: Value,
    pub(crate) external_ref: Option<Value>,
    pub(crate) child_session_id: Option<String>,
    pub(crate) label: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkEvent {
    pub(crate) sequence: u64,
    pub(crate) event_type: String,
    pub(crate) occurred_at_ms: u64,
    pub(crate) payload: Value,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkAwaitResult {
    pub(crate) process_id: String,
    pub(crate) outcome: lash::process::ProcessAwaitOutput,
    /// Reconciled from the durable log at terminal (ADR 0017): the authoritative,
    /// complete record, unlike the best-effort event sink.
    pub(crate) events: Vec<WorkAwaitEvent>,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkAwaitEvent {
    pub(crate) sequence: u64,
    pub(crate) event_type: String,
}
