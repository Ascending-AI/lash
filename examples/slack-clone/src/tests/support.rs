//! Test harness: a real platform on a real socket, and a bot with a scripted
//! model.
//!
//! No test in this crate ever needs a model token. The provider is
//! `lash::testing::TestProvider` scripted with standard-mode responses — plain
//! text, or a native tool call followed by text — so the tool loop is exercised
//! deterministically.

use lash::sync::MutexExt;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::ModelSpec;
use lash::direct::LlmOutputPart;
use lash::provider::{LlmResponse, ProviderHandle};
use tokio::task::JoinHandle;

use crate::bot::channel::{BotIdentity, ChannelBot};
use crate::bot::ledger::EventLedger;
use crate::bot::runtime::{self, RuntimeConfig};
use crate::bot::slack_api::SlackApi;
use crate::bot::{ledger, webhook};
use crate::ids::Ts;
use crate::platform::db::{self, Author};
use crate::platform::state::PlatformState;
use crate::platform::{self, PlatformConfig};
use crate::store::SqliteHandle;
use crate::wire::events::{EventCallback, EventRequest};
use crate::wire::methods::MessageObject;

/// The bot token every test uses.
///
/// Deliberately not `xoxb-…`: a checked-in literal shaped like a real Slack bot
/// token trips secret scanners and teaches the wrong reflex.
pub const BOT_TOKEN: &str = "slack-clone-test-token";
/// The verification token every test envelope carries.
pub const VERIFICATION_TOKEN: &str = "test-verification";

/// One scripted model response.
#[derive(Clone, Debug)]
pub enum Step {
    /// Finish the turn with this text.
    Text(String),
    /// Call a native tool. The loop continues, so a `Text` step must follow.
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    /// Announce arrival, block until released, then finish with this text.
    ///
    /// Holds a turn open at a point where the runtime has already claimed the
    /// queued input and taken the session-execution lease — the state a process
    /// killed mid-turn leaves behind.
    Gated(String),
}

/// A scripted standard-mode model.
#[derive(Clone)]
pub struct Script {
    steps: Arc<tokio::sync::Mutex<VecDeque<Step>>>,
    /// Serialized `LlmRequest` per call, so a test can prove what the model saw.
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    /// Notified when a [`Step::Gated`] call is entered.
    entered: Arc<tokio::sync::Notify>,
    /// Awaited by a [`Step::Gated`] call before it answers.
    release: Arc<tokio::sync::Notify>,
}

impl Script {
    /// A script that plays `steps` in order and then repeats its last text.
    pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: Arc::new(tokio::sync::Mutex::new(steps.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Wait until a gated step has been entered — i.e. the turn is live and its
    /// queued input is claimed.
    pub async fn wait_gated(&self) {
        self.entered.notified().await;
    }

    /// Let a gated step finish.
    pub fn release_gate(&self) {
        self.release.notify_waiters();
        self.release.notify_one();
    }

    /// A script that answers every turn with one line of prose.
    pub fn prose(text: &str) -> Self {
        Self::new([Step::Text(text.to_string())])
    }

    /// How many provider calls happened. One plain turn is one call; a turn with
    /// a tool call is two.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The serialized requests the model received.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock_recover().clone()
    }

    /// Whether any request carried `needle` — used to prove that ambient channel
    /// traffic really reached the prompt.
    pub fn saw(&self, needle: &str) -> bool {
        self.requests()
            .iter()
            .any(|request| request.contains(needle))
    }

    /// Build the provider handle.
    pub fn provider(&self) -> ProviderHandle {
        let steps = Arc::clone(&self.steps);
        let requests = Arc::clone(&self.requests);
        let calls = Arc::clone(&self.calls);
        let entered = Arc::clone(&self.entered);
        let release = Arc::clone(&self.release);
        lash::testing::TestProvider::builder()
            .kind("slack-clone-test")
            .complete(move |request| {
                let steps = Arc::clone(&steps);
                let requests = Arc::clone(&requests);
                let calls = Arc::clone(&calls);
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if let Ok(encoded) = serde_json::to_string(&request) {
                        requests.lock_recover().push(encoded);
                    }
                    let mut queue = steps.lock().await;
                    // The last step repeats so a test that runs an extra turn
                    // gets a sensible answer instead of a panic.
                    let step = if queue.len() > 1 {
                        queue.pop_front().expect("non-empty queue")
                    } else {
                        queue
                            .front()
                            .cloned()
                            .unwrap_or(Step::Text("ok".to_string()))
                    };
                    // A gated call models one in-flight turn; it must not hold
                    // the script queue lock, because a different routed session
                    // is allowed to enter the provider concurrently.
                    drop(queue);
                    let step = match step {
                        Step::Gated(text) => {
                            // The turn is now live: the input is claimed and the
                            // session-execution lease is held.
                            entered.notify_waiters();
                            entered.notify_one();
                            release.notified().await;
                            Step::Text(text)
                        }
                        other => other,
                    };
                    Ok(match step {
                        Step::Gated(_) => unreachable!("gated steps are unwrapped above"),
                        Step::Text(text) => LlmResponse {
                            full_text: text.clone(),
                            parts: vec![LlmOutputPart::Text {
                                text,
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                        Step::ToolCall { name, args } => LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: format!("call-{}", calls.load(Ordering::SeqCst)),
                                tool_name: name,
                                input_json: args.to_string(),
                                replay: None,
                            }],
                            response_metadata: Default::default(),
                            ..LlmResponse::default()
                        },
                    })
                }
            })
            .build()
            .into_handle()
    }
}

/// A platform served on an ephemeral port.
pub struct TestPlatform {
    pub state: PlatformState,
    pub base_url: String,
    pub addr: SocketAddr,
    _server: JoinHandle<()>,
}

impl TestPlatform {
    /// Boot a platform rooted at `dir`.
    pub async fn start(dir: &Path) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test platform");
        let addr = listener.local_addr().expect("platform addr");
        let config = PlatformConfig {
            addr,
            data_dir: dir.to_path_buf(),
            bot_token: BOT_TOKEN.to_string(),
            verification_token: VERIFICATION_TOKEN.to_string(),
            bot_handle: "lashbot".to_string(),
            team_name: "Test Workspace".to_string(),
            retry_backoff: Duration::from_millis(10),
            delivery_timeout: Duration::from_millis(500),
        };
        let database = SqliteHandle::open(&dir.join("workspace.db"), db::SCHEMA)
            .expect("open test workspace store");
        let state = PlatformState::seed(config, database)
            .await
            .expect("seed test workspace");
        let router = platform::router(state.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            state,
            base_url: format!("http://{addr}"),
            addr,
            _server: server,
        }
    }

    /// Claim a human identity, returning its `U…`.
    pub async fn identify(&self, name: &str) -> String {
        let id = self.state.ids().mint("U");
        let handle = name.to_lowercase();
        let display = name.to_string();
        self.state
            .database()
            .call(move |connection| db::upsert_user(connection, &id, &handle, &display, false))
            .await
            .expect("claim identity")
            .id
    }

    /// Create a channel, returning its `C…`.
    pub async fn channel(&self, name: &str) -> String {
        let id = self.state.ids().mint("C");
        let name = name.to_string();
        self.state
            .database()
            .call(move |connection| db::upsert_channel(connection, &id, &name, "", false))
            .await
            .expect("create channel")
            .id
    }

    /// The bot's mention token.
    pub fn mention(&self) -> String {
        crate::wire::events::mention_token(&self.state.identity().bot_user_id)
    }

    /// Post as a human and return the resulting `ts`.
    pub async fn say(&self, channel: &str, user_id: &str, text: &str) -> Ts {
        self.state
            .post_message(
                channel.to_string(),
                Author::User {
                    user_id: user_id.to_string(),
                },
                text.to_string(),
                None,
                false,
                None,
            )
            .await
            .expect("post as user")
            .ts
    }

    /// Post a human reply in a thread and return its `ts`.
    pub async fn say_thread(&self, channel: &str, user_id: &str, thread_ts: Ts, text: &str) -> Ts {
        self.state
            .post_message(
                channel.to_string(),
                Author::User {
                    user_id: user_id.to_string(),
                },
                text.to_string(),
                Some(thread_ts),
                false,
                None,
            )
            .await
            .expect("post thread reply as user")
            .ts
    }

    /// Drain the outbox, returning the envelopes the platform queued and marking
    /// them delivered so a later call returns only what is new.
    ///
    /// This is how the bot tests get *real* envelopes: the platform's own event
    /// generation is under test alongside the bot's handling of it.
    pub async fn drain_envelopes(&self) -> Vec<EventCallback> {
        let rows: Vec<(i64, String)> = self
            .state
            .database()
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, payload_json FROM event_outbox
                     WHERE delivered_at IS NULL AND abandoned_at IS NULL
                     ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (id, _) in &rows {
                    connection.execute(
                        "UPDATE event_outbox SET delivered_at = 1 WHERE id = ?1",
                        rusqlite::params![id],
                    )?;
                }
                Ok(rows)
            })
            .await
            .expect("drain outbox");
        rows.into_iter()
            .filter_map(|(_, payload)| {
                match serde_json::from_str::<EventRequest>(&payload).expect("decode envelope") {
                    EventRequest::EventCallback(envelope) => Some(*envelope),
                    EventRequest::UrlVerification(_) => None,
                }
            })
            .collect()
    }

    /// Every message in a channel, oldest first.
    pub async fn messages(&self, channel: &str) -> Vec<MessageObject> {
        let channel = channel.to_string();
        let rows = self
            .state
            .database()
            .call(move |connection| {
                db::channel_history(connection, &channel, db::TsWindow::default(), 500)
            })
            .await
            .expect("read channel history");
        rows.iter()
            .rev()
            .map(|row| crate::platform::web_api::message_object(row, true))
            .collect()
    }

    /// Remove a message from the workspace.
    ///
    /// Not a platform feature — the Slack subset here has no deletions. It exists
    /// so a test can reconstruct the state left by a crash between committing a
    /// turn and posting its reply: the transcript has the answer and the channel
    /// does not.
    pub async fn delete_message(&self, channel: &str, ts: &str) {
        let channel = channel.to_string();
        let micros = Ts::parse(ts).expect("parse ts").micros() as i64;
        self.state
            .database()
            .call(move |connection| {
                connection.execute(
                    "DELETE FROM messages WHERE channel_id = ?1 AND ts = ?2",
                    rusqlite::params![channel, micros],
                )?;
                Ok(())
            })
            .await
            .expect("delete message");
    }

    /// Only the app-authored messages in a channel.
    pub async fn bot_messages(&self, channel: &str) -> Vec<MessageObject> {
        self.messages(channel)
            .await
            .into_iter()
            .filter(|message| message.bot_id.is_some())
            .collect()
    }

    /// Every reply in one thread, oldest first (parent excluded).
    pub async fn thread_messages(&self, channel: &str, thread_ts: Ts) -> Vec<MessageObject> {
        let channel = channel.to_string();
        let rows = self
            .state
            .database()
            .call(move |connection| {
                db::thread_replies(
                    connection,
                    &channel,
                    thread_ts,
                    db::TsWindow::default(),
                    500,
                )
            })
            .await
            .expect("read thread replies");
        rows.iter()
            .map(|row| crate::platform::web_api::message_object(row, true))
            .collect()
    }
}

/// Build a bot against a platform, on `data_dir`.
///
/// Restart tests call this twice with the same `data_dir`: the second call is a
/// new process's worth of state, rebuilt from the same durable stores.
pub async fn start_bot(
    platform: &TestPlatform,
    data_dir: &Path,
    script: &Script,
) -> Arc<ChannelBot> {
    let api = Arc::new(SlackApi::new(&platform.base_url, BOT_TOKEN).expect("build api client"));
    let auth = api.auth_test().await.expect("auth.test");
    let identity = BotIdentity {
        bot_user_id: auth.user_id,
        bot_id: auth.bot_id,
        handle: auth.user,
        team_id: auth.team_id,
    };
    let ledger_database =
        SqliteHandle::open(&data_dir.join("events.db"), ledger::SCHEMA).expect("open test ledger");
    let mut runtime_config = RuntimeConfig::new(data_dir.join("lash"));
    runtime_config.trace_to_stderr = false;
    let model = ModelSpec::builder("mock/model")
        .context_window_tokens(200_000)
        .build()
        .expect("valid mock model metadata");
    let built = runtime::build_core(&runtime_config, script.provider(), model, Arc::clone(&api))
        .await
        .expect("build test core");
    let bot = Arc::new(ChannelBot::new(
        built.core,
        api,
        EventLedger::new(ledger_database),
        identity,
        VERIFICATION_TOKEN.to_string(),
    ));
    bot.refresh_directory().await.expect("preload directory");
    bot
}

/// Serve a bot's webhook router on an ephemeral port, returning its request URL.
pub async fn serve_bot(bot: Arc<ChannelBot>) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test bot");
    let addr = listener.local_addr().expect("bot addr");
    let router = webhook::router(bot);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}{}", webhook::EVENTS_PATH), handle)
}

/// Simulate the session-execution lease TTL elapsing.
///
/// The defect this guards against only appears inside a previous boot's lease TTL
/// (30s by default), and the fix's liveness only appears once that TTL passes.
/// Waiting 30 seconds in a test is not an option, and sleeping is not the property
/// under test — the property is what the *store state* makes possible. So this
/// backdates every lease row, which is precisely what wall-clock time does.
///
/// Test-only surgery, and deliberately blunt: the bot has no business expiring its
/// own leases, so this lives here rather than behind a product API.
pub fn expire_session_leases(bot_dir: &Path) -> usize {
    let root = bot_dir.join("lash").join("lash-sessions");
    let mut expired = 0;
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("db") {
                continue;
            }
            let Ok(connection) = rusqlite::Connection::open(&path) else {
                continue;
            };
            expired += connection
                .execute(
                    "UPDATE session_execution_leases SET lease_expires_at_ms = 1",
                    [],
                )
                .unwrap_or(0);
        }
    }
    expired
}

/// A scratch directory that cleans itself up.
pub fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Sub-directory paths a restart test reuses across two bot instances.
pub fn bot_dir(root: &Path) -> PathBuf {
    root.join("bot")
}

/// Find the single envelope of a given event type, failing loudly otherwise.
pub fn only_event(envelopes: &[EventCallback], kind: &str) -> EventCallback {
    let matching: Vec<&EventCallback> = envelopes
        .iter()
        .filter(|envelope| event_kind(envelope) == kind)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one {kind} envelope, got {:?}",
        envelopes.iter().map(event_kind).collect::<Vec<_>>()
    );
    matching[0].clone()
}

/// The event type name inside an envelope.
pub fn event_kind(envelope: &EventCallback) -> &'static str {
    match envelope.event {
        crate::wire::events::Event::Message(_) => "message",
        crate::wire::events::Event::AppMention(_) => "app_mention",
    }
}
