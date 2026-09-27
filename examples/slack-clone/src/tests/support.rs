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
use lash::provider::{
    FailureCode, LlmResponse, ProviderFailureKind, ProviderHandle, TransportRetryVerdict,
};
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
    Text(String),
    /// The loop continues, so a `Text` step must follow.
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    /// Announce arrival, block until released, then finish with this text.
    ///
    /// Holds a turn open inside its model call, after the engine admitted the
    /// queued input — the state a process killed mid-turn leaves behind.
    Gated(String),
    /// Reject the request with a terminal provider failure.
    ProviderError {
        message: String,
        kind: ProviderFailureKind,
        code: String,
    },
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

    /// A non-retryable provider rejection with typed classification.
    pub fn provider_error(message: &str) -> Self {
        Self::new([Step::ProviderError {
            message: message.to_string(),
            kind: ProviderFailureKind::Validation,
            code: "unsupported_attachment_capability".to_string(),
        }])
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

    pub fn saw(&self, needle: &str) -> bool {
        self.requests()
            .iter()
            .any(|request| request.contains(needle))
    }

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
                    let response = match step {
                        Step::Gated(_) => unreachable!("gated steps are unwrapped above"),
                        Step::Text(text) => LlmResponse {
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
                        Step::ProviderError {
                            message,
                            kind,
                            code,
                        } => {
                            let error = lash::provider::LlmTransportError::new(message)
                                .with_kind(kind)
                                .with_code(FailureCode::provider(code))
                                .with_retry_verdict(TransportRetryVerdict::NotRetryable);
                            return Err(error);
                        }
                    };
                    Ok(response)
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
            .map(|row| {
                crate::platform::web_api::message_object(row, true)
                    .expect("stored message metadata must decode")
            })
            .collect()
    }

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
            .map(|row| {
                crate::platform::web_api::message_object(row, true)
                    .expect("stored message metadata must decode")
            })
            .collect()
    }
}

/// One bot deployment's durable side: the bot's SQLite store set under
/// `<bot_dir>/lash`, with the Restate test double as the engine that drives
/// its turns.
///
/// The double stands in for the local restate-server the bot runs beside, so
/// it outlives any one boot: restart tests start a second bot from the same
/// host, a new process's worth of state over the same stores and server.
pub struct BotHost {
    bot_dir: PathBuf,
    double: lash_restate_test::RestateTestBackend,
}

impl BotHost {
    /// The host over `bot_dir`'s stores, created if absent.
    pub async fn open(bot_dir: &Path) -> Self {
        let stores: Arc<dyn lash::StoreSet> = Arc::new(
            runtime::open_stores(&bot_dir.join("lash"))
                .await
                .expect("open the bot's store set"),
        );
        let double = lash_restate_test::backend_with(
            0,
            lash_restate_test::ServerConfig::default(),
            move |_| stores,
        )
        .await
        .expect("build the Restate double over the bot's store set");
        Self {
            bot_dir: bot_dir.to_path_buf(),
            double,
        }
    }

    /// Kill a boot whose mention turn is held inside the model call by a
    /// [`Step::Gated`] step of `script`.
    ///
    /// Every attempt the boot's deployment is running ends with the process:
    /// the turn's attempt is cut inside the model call, before the server
    /// stores its result, and the session drive waiting on it suspends. The
    /// server keeps both invocations and holds them back while the bot is
    /// down; the gate is released only once nothing of that boot is left to
    /// answer it. Returns once the dead boot's session driver is released,
    /// so the next [`start`](Self::start) installs its own. Dropping the
    /// returned [`DeadBoot`] brings the deployment back: the server re-drives
    /// the held invocations on the boot running by then.
    pub async fn kill_boot_mid_turn(
        &self,
        bot: Arc<ChannelBot>,
        script: &Script,
        turn: JoinHandle<()>,
    ) -> DeadBoot {
        let server = self.double.server();
        let mut holds = Vec::new();
        let mut cut = 0;
        for invocation in server.invocations() {
            if invocation.status == "completed" {
                continue;
            }
            // `Service/key/handler`; only a keyed invocation can be held.
            let mut parts = invocation.target.splitn(2, '/');
            let (Some(service), Some(rest)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some((key, _handler)) = rest.rsplit_once('/') else {
                continue;
            };
            let hold = server.hold(service, key);
            tokio::pin!(hold);
            // One poll places the hold; it then waits for the running attempt
            // to stop, which for the turn is the cut below.
            let placed = tokio::select! {
                biased;
                hold = &mut hold => Some(hold),
                () = std::future::ready(()) => None,
            };
            if service == lash_restate_test::TURN_DRIVER_SERVICE
                && invocation.status == "running"
                && server.crash(&invocation.id)
            {
                cut += 1;
            }
            holds.push(match placed {
                Some(hold) => hold,
                None => hold.await,
            });
        }
        assert_eq!(
            cut, 1,
            "the dead boot's turn attempt is cut in its model call"
        );
        turn.abort();
        let _ = turn.await;
        drop(bot);
        script.release_gate();
        let slot = self.double.restate().session_work_engine().driver_slot();
        tokio::time::timeout(Duration::from_secs(30), async {
            while slot.installed().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the dead boot's session driver is released");
        DeadBoot { _holds: holds }
    }

    /// Boot a bot over this host's stores and engine.
    pub async fn start(&self, platform: &TestPlatform, script: &Script) -> Arc<ChannelBot> {
        let data_dir = &self.bot_dir;
        let api = Arc::new(SlackApi::new(&platform.base_url, BOT_TOKEN).expect("build api client"));
        let auth = api.auth_test().await.expect("auth.test");
        let identity = BotIdentity {
            bot_user_id: auth.user_id,
            bot_id: auth.bot_id,
            handle: auth.user,
            team_id: auth.team_id,
        };
        let ledger_database = SqliteHandle::open(&data_dir.join("events.db"), ledger::SCHEMA)
            .expect("open test ledger");
        let mut runtime_config = RuntimeConfig::new(data_dir.join("lash"));
        runtime_config.trace_to_stderr = false;
        let model = ModelSpec::builder("mock/model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid mock model metadata");
        let built = runtime::build_core(
            &runtime_config,
            self.double.lash_backend(),
            script.provider(),
            model,
            Arc::clone(&api),
        )
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
}

/// A boot [`BotHost::kill_boot_mid_turn`] killed: the server holds back the
/// invocations its deployment was running until this drops.
#[must_use = "dropping a dead boot lets the server re-drive its invocations"]
pub struct DeadBoot {
    _holds: Vec<lash_restate_test::Hold>,
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
