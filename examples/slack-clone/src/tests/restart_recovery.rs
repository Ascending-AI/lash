//! Restart and delivery-retry tests.
//!
//! These pin the properties a chat bot is judged on when something goes wrong:
//! it does not forget the room, it does not answer twice, and it finishes work it
//! accepted before it died.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::response::IntoResponse as _;
use axum::routing::post;

use crate::bot::channel::{Disposition, ReplySource};
use crate::bot::ledger::{EventLedger, KIND_APP_MENTION, KIND_MESSAGE, Stage};
use crate::bot::runtime::session_id;
use crate::bot::{ledger, webhook};
use crate::store::SqliteHandle;
use crate::wire::events::{RETRY_NUM_HEADER, RETRY_REASON_HEADER};

use super::support::{
    BOT_TOKEN, Script, TestPlatform, bot_dir, only_event, scratch, serve_bot, start_bot,
};

#[tokio::test]
async fn a_restarted_bot_keeps_the_channel_transcript_and_does_not_reply_twice() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let channel = platform.channel("restart").await;
    let ada = platform.identify("ada").await;
    let mention = platform.mention();

    // --- First process: fold context, then answer a mention. ---
    let first_script = Script::prose("Before the restart.");
    let first_mention = {
        let bot = start_bot(&platform, &bot_dir, &first_script).await;
        platform.say(&channel, &ada, "the migration finished").await;
        for envelope in platform.drain_envelopes().await {
            bot.ingest(envelope, None).await.expect("fold context");
        }
        platform
            .say(&channel, &ada, &format!("{mention} status?"))
            .await;
        let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
        let disposition = bot
            .ingest(app_mention.clone(), None)
            .await
            .expect("handle mention");
        assert!(matches!(
            disposition,
            Disposition::Replied {
                source: ReplySource::Turn,
                ..
            }
        ));
        // Drop the bot's own reply event so the second process starts from a
        // quiet outbox, as a real restart would after the platform delivered it.
        let _ = platform.drain_envelopes().await;
        app_mention
    };
    assert_eq!(first_script.calls(), 1);
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);

    // --- Second process: same durable stores, brand new in-memory state. ---
    let second_script = Script::prose("After the restart.");
    let bot = start_bot(&platform, &bot_dir, &second_script).await;
    let recovered = bot.recover().await.expect("recovery pass");
    assert!(
        recovered.is_empty(),
        "nothing was left unfinished: {recovered:?}"
    );

    // A redelivery of the pre-restart mention must be inert: the ledger survived
    // the process, so the new boot knows the event is already answered.
    let record = bot
        .ledger()
        .get(first_mention.event_id.clone())
        .await
        .expect("read ledger")
        .expect("the ledger row survived the restart");
    assert_eq!(record.stage, Stage::Replied);
    let disposition = bot
        .ingest(first_mention.clone(), Some(1))
        .await
        .expect("redelivery after restart");
    assert!(
        matches!(
            disposition,
            Disposition::Duplicate {
                stage: Stage::Replied,
                ..
            }
        ),
        "{disposition:?}"
    );
    assert_eq!(second_script.calls(), 0, "no turn for a settled redelivery");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
    // The redelivery must not have queued a second copy of the mention either.
    let session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("pending inputs")
            .is_empty()
    );

    // The committed transcript survived too: a fresh mention's prompt still
    // carries the pre-restart room context.
    platform
        .say(&channel, &ada, &format!("{mention} and now?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    assert!(matches!(
        disposition,
        Disposition::Replied {
            source: ReplySource::Turn,
            ..
        }
    ));
    assert!(
        second_script.saw("the migration finished"),
        "the pre-restart transcript must survive: {:?}",
        second_script.requests()
    );
    assert_eq!(
        platform.bot_messages(&channel).await.len(),
        2,
        "one reply before the restart, one after — never a duplicate"
    );
}

#[tokio::test]
async fn a_reply_owed_at_crash_time_is_posted_by_the_next_boots_recovery_pass() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let channel = platform.channel("owed").await;
    let ada = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &ada, &format!("{mention} anything?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    // Simulate the crash window precisely: a ledger row that says a reply is
    // owed, with the text on record and nothing posted. This is the state the bot
    // writes immediately before calling `chat.postMessage`.
    let ledger_database =
        SqliteHandle::open(&bot_dir.join("events.db"), ledger::SCHEMA).expect("open ledger");
    let ledger = EventLedger::new(ledger_database);
    ledger
        .claim(
            app_mention.event_id.clone(),
            channel.clone(),
            app_mention.event.ts().to_string(),
            KIND_APP_MENTION.to_string(),
            Some("ada: anything?".to_string()),
        )
        .await
        .expect("claim event");
    ledger
        .advance(
            app_mention.event_id.clone(),
            Stage::Accepted,
            Stage::ReplyPending,
            None,
            Some("Recovered answer.".to_string()),
        )
        .await
        .expect("record the owed reply");

    let script = Script::prose("should never run");
    let bot = start_bot(&platform, &bot_dir, &script).await;
    let recovered = bot.recover().await.expect("recovery pass");
    assert_eq!(recovered.len(), 1);
    assert!(
        matches!(
            recovered[0],
            Disposition::Replied {
                source: ReplySource::Ledger,
                ..
            }
        ),
        "recovery posts the recorded text without re-running the model: {recovered:?}"
    );
    assert_eq!(script.calls(), 0, "no model call during recovery");

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].text, "Recovered answer.");

    // And the recovered event is now terminal, so a redelivery does nothing.
    let disposition = bot
        .ingest(app_mention, Some(1))
        .await
        .expect("redelivery after recovery");
    assert!(
        matches!(
            disposition,
            Disposition::Duplicate {
                stage: Stage::Replied,
                ..
            }
        ),
        "{disposition:?}"
    );
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
}

#[tokio::test]
async fn a_crash_between_posting_and_recording_does_not_produce_a_second_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let channel = platform.channel("double-post").await;
    let ada = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &ada, &format!("{mention} hello"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let script = Script::prose("Posted once.");
    let bot = start_bot(&platform, &bot_dir, &script).await;
    bot.ingest(app_mention.clone(), None)
        .await
        .expect("handle mention");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);

    // Rewind the ledger to the instant before the post was recorded — the state a
    // crash there would leave. The reply is already in the channel, stamped with
    // this event id.
    // A deliberate rewind, naming the stage the row is actually at — the
    // compare-and-set permits this and rejects a *stale* caller's version of it.
    bot.ledger()
        .advance(
            app_mention.event_id.clone(),
            Stage::Replied,
            Stage::ReplyPending,
            None,
            Some("Posted once.".to_string()),
        )
        .await
        .expect("rewind the ledger");

    let recovered = bot.recover().await.expect("recovery pass");
    assert_eq!(recovered.len(), 1);
    assert!(
        matches!(
            recovered[0],
            Disposition::Duplicate {
                stage: Stage::Replied,
                ..
            }
        ),
        "the reply's own metadata proves it already landed: {recovered:?}"
    );
    assert_eq!(
        platform.bot_messages(&channel).await.len(),
        1,
        "recovery must not post a second copy"
    );
}

#[tokio::test]
async fn the_platform_retries_a_failed_delivery_with_slacks_retry_headers() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;

    // A receiver that fails its first attempt and records what it was sent.
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(
        Vec::<(Option<String>, Option<String>)>::new(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind flaky receiver");
    let addr = listener.local_addr().expect("receiver addr");
    let router = {
        let attempts = Arc::clone(&attempts);
        let seen = Arc::clone(&seen);
        Router::new().route(
            "/events",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let attempts = Arc::clone(&attempts);
                let seen = Arc::clone(&seen);
                async move {
                    let header = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string)
                    };
                    // The handshake must always succeed, or the URL is never
                    // registered and no event is ever sent.
                    if body.contains("url_verification") {
                        let request: serde_json::Value =
                            serde_json::from_str(&body).expect("handshake json");
                        let challenge = request["challenge"].as_str().unwrap_or_default();
                        return axum::Json(serde_json::json!({ "challenge": challenge }))
                            .into_response();
                    }
                    seen.lock()
                        .expect("seen mutex")
                        .push((header(RETRY_NUM_HEADER), header(RETRY_REASON_HEADER)));
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        axum::http::StatusCode::OK.into_response()
                    }
                }
            }),
        )
    };
    let _receiver = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    reqwest::Client::new()
        .post(format!("{}/platform/apps", platform.base_url))
        .bearer_auth(BOT_TOKEN)
        .json(&serde_json::json!({ "request_url": format!("http://{addr}/events") }))
        .send()
        .await
        .expect("register the request url")
        .error_for_status()
        .expect("registration succeeded, so the handshake did too");

    let channel = platform.channel("retries").await;
    let ada = platform.identify("ada").await;
    platform.say(&channel, &ada, "delivered eventually").await;

    // The dispatcher loop is not running in tests; pump it explicitly so the
    // retry schedule is observed rather than raced.
    crate::platform::dispatch::deliver_once(&platform.state)
        .await
        .expect("first delivery pass");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    crate::platform::dispatch::deliver_once(&platform.state)
        .await
        .expect("retry pass");

    let seen = seen.lock().expect("seen mutex").clone();
    assert_eq!(seen.len(), 2, "one failed attempt and one retry: {seen:?}");
    assert_eq!(
        seen[0],
        (None, None),
        "the first delivery carries no retry headers"
    );
    assert_eq!(
        seen[1],
        (Some("1".to_string()), Some("http_error".to_string())),
        "the retry carries Slack's retry headers"
    );
}

#[tokio::test]
async fn the_webhook_endpoint_answers_the_url_verification_handshake() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("hi");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let (request_url, _server) = serve_bot(Arc::clone(&bot)).await;

    // Registration drives the platform's real handshake against the bot's real
    // endpoint, which is the only way to know both halves agree.
    let response = reqwest::Client::new()
        .post(format!("{}/platform/apps", platform.base_url))
        .bearer_auth(BOT_TOKEN)
        .json(&serde_json::json!({ "request_url": request_url }))
        .send()
        .await
        .expect("register");
    let body: serde_json::Value = response.json().await.expect("decode body");
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["verified"], serde_json::json!(true));
    assert_eq!(
        body["bot_user_id"],
        serde_json::json!(bot.identity().bot_user_id)
    );

    // A URL that does not echo the challenge must be refused, not stored.
    let refused = reqwest::Client::new()
        .post(format!("{}/platform/apps", platform.base_url))
        .bearer_auth(BOT_TOKEN)
        .json(&serde_json::json!({
            "request_url": format!("http://{}{}", platform.addr, webhook::EVENTS_PATH),
        }))
        .send()
        .await
        .expect("register a wrong url");
    let body: serde_json::Value = refused.json().await.expect("decode body");
    assert_eq!(
        body["error"],
        serde_json::json!("request_url_verification_failed")
    );
}

#[tokio::test]
async fn a_channel_session_id_is_derived_from_the_channel_id() {
    // Pinned because the mapping is the example's doctrine: change this and every
    // existing room loses its memory on deploy.
    assert_eq!(session_id("C012AB3CD"), "channel:C012AB3CD");
}

#[tokio::test]
async fn an_event_accepted_before_a_crash_is_answered_by_the_next_boots_recovery_pass() {
    // Red-proof for the abandoned-`accepted` bug: before the fix, recovery took
    // the no-recorded-text branch, marked the row `ignored` with
    // `reply_lost_after_commit`, and every later redelivery reported `Duplicate` —
    // so this mention was never answered by anyone. The assertions below are
    // exactly the ones that failed: `Replied { source: Turn }`, one model call,
    // one reply in the channel.
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let channel = platform.channel("accepted-crash").await;
    let ada = platform.identify("ada").await;
    let mention = platform.mention();

    // Ambient context first, so recovery has to fold it into the answer too.
    platform
        .say(&channel, &ada, "the cache is cold again")
        .await;
    platform
        .say(&channel, &ada, &format!("{mention} why is it slow?"))
        .await;
    let envelopes = platform.drain_envelopes().await;
    let ambient = envelopes
        .iter()
        .find(|envelope| {
            super::support::event_kind(envelope) == "message"
                && !envelope.event.text().contains(&mention)
        })
        .cloned()
        .expect("an ambient message envelope");
    let app_mention = only_event(&envelopes, "app_mention");

    // The crash: both events are claimed and recorded at `accepted`, and the
    // process dies before doing any of the work. This is what `ingest` writes
    // before it opens a session.
    let ledger_database =
        SqliteHandle::open(&bot_dir.join("events.db"), ledger::SCHEMA).expect("open ledger");
    let ledger = EventLedger::new(ledger_database);
    for (envelope, kind, text) in [
        (&ambient, KIND_MESSAGE, "ada: the cache is cold again"),
        (&app_mention, KIND_APP_MENTION, "ada: why is it slow?"),
    ] {
        let claim = ledger
            .claim(
                envelope.event_id.clone(),
                channel.clone(),
                envelope.event.ts().to_string(),
                kind.to_string(),
                Some(text.to_string()),
            )
            .await
            .expect("claim event");
        assert_eq!(claim.record().stage, Stage::Accepted);
    }

    // The next boot.
    let script = Script::prose("Because the cache is cold.");
    let bot = start_bot(&platform, &bot_dir, &script).await;
    let recovered = bot.recover().await.expect("recovery pass");
    assert_eq!(recovered.len(), 2, "both accepted rows are picked up");

    let folded = recovered
        .iter()
        .filter(|disposition| matches!(disposition, Disposition::Folded { .. }))
        .count();
    assert_eq!(
        folded, 1,
        "the ambient row re-admits its context: {recovered:?}"
    );
    let replied = recovered
        .iter()
        .find(|disposition| matches!(disposition, Disposition::Replied { .. }))
        .expect("the mention row is answered");
    assert!(
        matches!(
            replied,
            Disposition::Replied {
                source: ReplySource::Turn,
                ..
            }
        ),
        "recovery runs the turn it never got to run: {replied:?}"
    );
    assert_eq!(
        script.calls(),
        1,
        "exactly one turn, not one per queued line"
    );

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1, "one reply, and it exists at all");
    assert_eq!(replies[0].text, "Because the cache is cold.");
    assert!(
        script.saw("the cache is cold again"),
        "the recovered turn still folds the ambient context: {:?}",
        script.requests()
    );

    // Both rows are terminal now, so the platform's redeliveries are inert.
    for envelope in [ambient, app_mention] {
        let disposition = bot
            .ingest(envelope, Some(1))
            .await
            .expect("redelivery after recovery");
        assert!(
            matches!(disposition, Disposition::Duplicate { .. }),
            "{disposition:?}"
        );
    }
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
    assert_eq!(script.calls(), 1);
}

#[tokio::test]
async fn a_reply_lost_with_its_process_is_recovered_from_the_committed_transcript() {
    // The narrowest crash window: the turn drained its queued input and
    // committed, then the process died before the reply text reached the ledger.
    // The answer is in the session transcript, so recovery reads it back instead
    // of reporting a loss.
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let bot_dir = bot_dir(scratch.path());
    let channel = platform.channel("lost-reply").await;
    let ada = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &ada, &format!("{mention} still there?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let script = Script::prose("Still here.");
    let bot = start_bot(&platform, &bot_dir, &script).await;
    bot.ingest(app_mention.clone(), None)
        .await
        .expect("handle mention");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
    let posted_ts = platform.bot_messages(&channel).await[0].ts.clone();

    // Reconstruct the window: the turn is committed (it really ran above), the
    // reply is not in the channel, and the ledger is back at `accepted` with no
    // recorded text.
    platform.delete_message(&channel, &posted_ts).await;
    bot.ledger()
        .advance(
            app_mention.event_id.clone(),
            Stage::Replied,
            Stage::Accepted,
            None,
            None,
        )
        .await
        .expect("rewind the ledger");

    let recovered = bot.recover().await.expect("recovery pass");
    assert_eq!(recovered.len(), 1);
    assert!(
        matches!(
            recovered[0],
            Disposition::Replied {
                source: ReplySource::Transcript,
                ..
            }
        ),
        "the committed transcript holds the answer: {recovered:?}"
    );
    assert_eq!(
        script.calls(),
        1,
        "the model is not asked again — the turn already happened"
    );
    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].text, "Still here.");
}

#[tokio::test]
async fn the_api_client_form_encodes_read_methods_and_uses_json_only_for_posting() {
    // Slack accepts form-encoded bodies for every Web API method and JSON for
    // only some. A client that JSON-posts `conversations.history` works against a
    // permissive server and fails against real Slack, which would make this
    // example's migration claim false. So the encoding is asserted, not assumed.
    let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, String, String)>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind recording server");
    let addr = listener.local_addr().expect("recorder addr");
    let router = {
        let seen = Arc::clone(&seen);
        Router::new().route(
            "/api/{method}",
            post(
                move |axum::extract::Path(method): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      body: String| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let content_type = headers
                            .get(axum::http::header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        seen.lock()
                            .expect("seen mutex")
                            .push((method.clone(), content_type, body));
                        // Enough of each response for the client's `ok` gate and
                        // its typed decode to succeed.
                        let payload = match method.as_str() {
                            "conversations.history" => serde_json::json!({
                                "ok": true, "messages": [], "has_more": false, "pin_count": 0
                            }),
                            "users.list" => serde_json::json!({
                                "ok": true, "members": [], "cache_ts": 0
                            }),
                            "chat.postMessage" => serde_json::json!({
                                "ok": true,
                                "channel": "C1",
                                "ts": "1.000001",
                                "message": {"type": "message", "text": "hi", "ts": "1.000001"}
                            }),
                            _ => serde_json::json!({ "ok": true }),
                        };
                        axum::Json(payload).into_response()
                    }
                },
            ),
        )
    };
    let _recorder = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let api = crate::bot::slack_api::SlackApi::new(format!("http://{addr}"), BOT_TOKEN)
        .expect("build client");
    api.conversations_history(&crate::bot::slack_api::HistoryQuery::since(
        "C1", "1.000001",
    ))
    .await
    .expect("history call");
    api.users_list(None).await.expect("users call");
    api.chat_post_message(&crate::bot::slack_api::ChatPostMessageRequest::reply(
        "C1", "hi", "Ev1",
    ))
    .await
    .expect("post call");

    let seen = seen.lock().expect("seen mutex").clone();
    let by_method = |name: &str| {
        seen.iter()
            .find(|(method, _, _)| method == name)
            .cloned()
            .unwrap_or_else(|| panic!("no request recorded for {name}"))
    };

    let (_, content_type, body) = by_method("conversations.history");
    assert!(
        content_type.starts_with("application/x-www-form-urlencoded"),
        "read methods must be form-encoded, got {content_type}"
    );
    // Slack's argument-string model, and the `ts` bounds the recovery scan needs.
    assert!(body.contains("channel=C1"), "{body}");
    assert!(body.contains("oldest=1.000001"), "{body}");
    assert!(body.contains("inclusive=true"), "{body}");
    assert!(body.contains("include_all_metadata=true"), "{body}");

    let (_, content_type, body) = by_method("users.list");
    assert!(
        content_type.starts_with("application/x-www-form-urlencoded"),
        "got {content_type}"
    );
    assert!(body.contains("limit=200"), "{body}");
    assert!(
        !body.contains("cursor"),
        "an absent cursor must not be sent as an empty argument: {body}"
    );

    let (_, content_type, body) = by_method("chat.postMessage");
    assert!(
        content_type.starts_with("application/json"),
        "chat.postMessage carries a metadata object, so it posts JSON: got {content_type}"
    );
    let posted: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(posted["metadata"]["event_payload"]["event_id"], "Ev1");
}
