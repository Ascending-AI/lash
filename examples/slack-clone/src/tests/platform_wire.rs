//! Platform wire-contract tests.
//!
//! Every assertion here is against **raw JSON keys**, not against the Rust
//! structs that produced them. Asserting on a deserialized struct would pass
//! happily after a rename that breaks every client, which is the exact failure
//! this example exists to prevent.

use serde_json::Value;

use super::support::{BOT_TOKEN, TestPlatform, scratch};

/// POST a Web API method as the bot and return the parsed body.
async fn call(platform: &TestPlatform, method: &str, args: Value) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{}/api/{method}", platform.base_url))
        .bearer_auth(BOT_TOKEN)
        .json(&args)
        .send()
        .await
        .expect("call web api");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "Slack answers every Web API call with HTTP 200"
    );
    response.json().await.expect("decode web api body")
}

/// The keys of a JSON object, sorted.
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("json object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn auth_test_reports_the_apps_own_identity_shape() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let body = call(&platform, "auth.test", serde_json::json!({})).await;
    assert_eq!(
        keys(&body),
        ["bot_id", "ok", "team", "team_id", "url", "user", "user_id"]
    );
    assert_eq!(body["ok"], Value::Bool(true));
    assert!(body["user_id"].as_str().expect("user_id").starts_with('U'));
    assert!(body["bot_id"].as_str().expect("bot_id").starts_with('B'));
    assert!(body["team_id"].as_str().expect("team_id").starts_with('T'));
}

#[tokio::test]
async fn chat_post_message_returns_slacks_documented_envelope() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("general-wire").await;
    let body = call(
        &platform,
        "chat.postMessage",
        serde_json::json!({ "channel": channel, "text": "wire check" }),
    )
    .await;
    assert_eq!(keys(&body), ["channel", "message", "ok", "ts"]);
    assert_eq!(body["channel"], Value::String(channel));
    let ts = body["ts"].as_str().expect("ts");
    // `<seconds>.<6 digits>` — Slack's message identity format.
    let (seconds, micros) = ts.split_once('.').expect("ts has a fractional part");
    assert!(seconds.chars().all(|character| character.is_ascii_digit()));
    assert_eq!(micros.len(), 6, "ts fraction is exactly six digits: {ts}");

    let message = &body["message"];
    assert_eq!(
        keys(message),
        ["bot_id", "subtype", "text", "ts", "type", "username"]
    );
    assert_eq!(message["type"], Value::String("message".to_string()));
    assert_eq!(
        message["subtype"],
        Value::String("bot_message".to_string()),
        "an app's own post carries subtype bot_message"
    );
    assert_eq!(message["ts"], body["ts"]);
    assert!(
        message.get("user").is_none(),
        "app messages carry bot_id, not user"
    );
}

#[tokio::test]
async fn web_api_failures_are_http_200_with_an_error_code() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let missing = call(
        &platform,
        "chat.postMessage",
        serde_json::json!({ "channel": "C000NOPE00", "text": "hi" }),
    )
    .await;
    assert_eq!(missing["ok"], Value::Bool(false));
    assert_eq!(missing["error"], Value::String("channel_not_found".into()));

    let channel = platform.channel("no-text").await;
    let empty = call(
        &platform,
        "chat.postMessage",
        serde_json::json!({ "channel": channel, "text": "   " }),
    )
    .await;
    assert_eq!(empty["error"], Value::String("no_text".into()));
}

#[tokio::test]
async fn an_unauthorized_call_reports_not_authed_rather_than_401() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let response = reqwest::Client::new()
        .post(format!("{}/api/auth.test", platform.base_url))
        .send()
        .await
        .expect("call without a token");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("decode body");
    assert_eq!(body["error"], Value::String("not_authed".into()));

    let wrong = reqwest::Client::new()
        .post(format!("{}/api/auth.test", platform.base_url))
        .bearer_auth("xoxb-not-the-token")
        .send()
        .await
        .expect("call with a bad token");
    let body: Value = wrong.json().await.expect("decode body");
    assert_eq!(body["error"], Value::String("invalid_auth".into()));
}

#[tokio::test]
async fn conversations_list_returns_slacks_channel_object_shape() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let body = call(&platform, "conversations.list", serde_json::json!({})).await;
    assert_eq!(keys(&body), ["channels", "ok"]);
    let channel = &body["channels"][0];
    assert_eq!(
        keys(channel),
        [
            "created",
            "creator",
            "id",
            "is_archived",
            "is_channel",
            "is_general",
            "is_group",
            "is_im",
            "is_member",
            "is_mpim",
            "is_private",
            "name",
            "name_normalized",
            "num_members",
            "purpose",
            "topic",
        ]
    );
    assert_eq!(keys(&channel["topic"]), ["creator", "last_set", "value"]);
    assert!(channel["id"].as_str().expect("id").starts_with('C'));
    assert!(
        channel["created"].as_u64().expect("created") > 1_600_000_000,
        "`created` is epoch seconds, unlike `ts`"
    );
}

#[tokio::test]
async fn users_list_returns_slacks_member_object_shape() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let body = call(&platform, "users.list", serde_json::json!({})).await;
    assert_eq!(keys(&body), ["cache_ts", "members", "ok"]);
    let member = &body["members"][0];
    assert_eq!(
        keys(member),
        [
            "color",
            "deleted",
            "id",
            "is_admin",
            "is_app_user",
            "is_bot",
            "is_owner",
            "is_primary_owner",
            "is_restricted",
            "is_ultra_restricted",
            "name",
            "profile",
            "real_name",
            "team_id",
            "tz",
            "tz_label",
            "tz_offset",
            "updated",
        ]
    );
    assert_eq!(
        keys(&member["profile"]),
        [
            "display_name",
            "display_name_normalized",
            "real_name",
            "real_name_normalized",
            "team",
        ]
    );
}

#[tokio::test]
async fn conversations_history_is_newest_first_and_paginates_by_cursor() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("paging").await;
    let user = platform.identify("ada").await;
    for index in 0..5 {
        platform
            .say(&channel, &user, &format!("line {index}"))
            .await;
    }

    let first = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel, "limit": 2 }),
    )
    .await;
    assert_eq!(
        keys(&first),
        [
            "has_more",
            "messages",
            "ok",
            "pin_count",
            "response_metadata"
        ]
    );
    assert_eq!(first["has_more"], Value::Bool(true));
    assert_eq!(first["pin_count"], serde_json::json!(0));
    assert_eq!(keys(&first["response_metadata"]), ["next_cursor"]);
    let texts: Vec<&str> = first["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["text"].as_str().expect("text"))
        .collect();
    assert_eq!(
        texts,
        ["line 4", "line 3"],
        "conversations.history returns newest first"
    );
    assert_eq!(keys(&first["messages"][0]), ["text", "ts", "type", "user"]);

    let cursor = first["response_metadata"]["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_string();
    let second = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel, "limit": 2, "cursor": cursor }),
    )
    .await;
    let texts: Vec<&str> = second["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["text"].as_str().expect("text"))
        .collect();
    assert_eq!(
        texts,
        ["line 2", "line 1"],
        "a cursor continues strictly older than the previous page"
    );

    let bad_cursor = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel, "cursor": "not-a-cursor" }),
    )
    .await;
    assert_eq!(bad_cursor["error"], Value::String("invalid_cursor".into()));
}

#[tokio::test]
async fn history_hides_message_metadata_unless_it_is_asked_for() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("metadata").await;
    call(
        &platform,
        "chat.postMessage",
        serde_json::json!({
            "channel": channel,
            "text": "stamped",
            "metadata": {
                "event_type": "slack_clone_bot_reply",
                "event_payload": { "event_id": "Ev123ABC456" },
            },
        }),
    )
    .await;

    let plain = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel }),
    )
    .await;
    assert!(
        plain["messages"][0].get("metadata").is_none(),
        "metadata is opt-in, as it is on Slack"
    );

    let with_metadata = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel, "include_all_metadata": true }),
    )
    .await;
    let metadata = &with_metadata["messages"][0]["metadata"];
    assert_eq!(keys(metadata), ["event_payload", "event_type"]);
    assert_eq!(
        metadata["event_payload"]["event_id"],
        Value::String("Ev123ABC456".into())
    );
}

#[tokio::test]
async fn form_encoded_arguments_are_accepted_exactly_as_slack_accepts_them() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("form-args").await;
    // Slack's form dialect: scalars as text, object arguments JSON-encoded.
    let response = reqwest::Client::new()
        .post(format!("{}/api/chat.postMessage", platform.base_url))
        .bearer_auth(BOT_TOKEN)
        .form(&[
            ("channel", channel.as_str()),
            ("text", "form encoded"),
            (
                "metadata",
                r#"{"event_type":"slack_clone_bot_reply","event_payload":{"event_id":"Ev1"}}"#,
            ),
        ])
        .send()
        .await
        .expect("form-encoded post");
    let body: Value = response.json().await.expect("decode body");
    assert_eq!(body["ok"], Value::Bool(true));

    // …and a GET with query-string arguments, which the read methods also allow.
    let response = reqwest::Client::new()
        .get(format!(
            "{}/api/conversations.history?channel={channel}&limit=1&include_all_metadata=true",
            platform.base_url
        ))
        .bearer_auth(BOT_TOKEN)
        .send()
        .await
        .expect("query-string get");
    let body: Value = response.json().await.expect("decode body");
    assert_eq!(body["ok"], Value::Bool(true));
    assert_eq!(
        body["messages"][0]["metadata"]["event_payload"]["event_id"],
        Value::String("Ev1".into())
    );
}

#[tokio::test]
async fn conversations_replies_returns_the_parent_then_its_replies() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("threads").await;
    let user = platform.identify("grace").await;
    let parent = platform.say(&channel, &user, "thread root").await;
    let posted = call(
        &platform,
        "chat.postMessage",
        serde_json::json!({
            "channel": channel,
            "text": "threaded reply",
            "thread_ts": parent.to_string(),
        }),
    )
    .await;
    assert_eq!(posted["ok"], Value::Bool(true));

    let body = call(
        &platform,
        "conversations.replies",
        serde_json::json!({ "channel": channel, "ts": parent.to_string() }),
    )
    .await;
    assert_eq!(keys(&body), ["has_more", "messages", "ok"]);
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "parent plus one reply");
    assert_eq!(messages[0]["ts"], Value::String(parent.to_string()));
    assert_eq!(
        messages[0]["thread_ts"],
        Value::String(parent.to_string()),
        "the parent names itself as the thread"
    );
    assert_eq!(messages[0]["reply_count"], serde_json::json!(1));
    assert_eq!(messages[0]["reply_users_count"], serde_json::json!(1));
    assert_eq!(messages[0]["latest_reply"], messages[1]["ts"]);
    assert_eq!(messages[1]["parent_user_id"], Value::String(user));

    // A thread reply must not appear in the channel's top-level history.
    let history = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel }),
    )
    .await;
    assert_eq!(history["messages"].as_array().expect("messages").len(), 1);

    let missing = call(
        &platform,
        "conversations.replies",
        serde_json::json!({ "channel": channel, "ts": "1500000000.000001" }),
    )
    .await;
    assert_eq!(missing["error"], Value::String("thread_not_found".into()));
}

#[tokio::test]
async fn reply_broadcast_projects_one_thread_reply_onto_both_surfaces() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("thread-broadcast").await;
    let user = platform.identify("grace").await;
    let parent = platform.say(&channel, &user, "broadcast root").await;
    let posted = call(
        &platform,
        "chat.postMessage",
        serde_json::json!({
            "channel": channel,
            "text": "important thread reply",
            "thread_ts": parent.to_string(),
            "reply_broadcast": true,
        }),
    )
    .await;
    assert_eq!(posted["ok"], Value::Bool(true));

    let history = call(
        &platform,
        "conversations.history",
        serde_json::json!({ "channel": channel }),
    )
    .await;
    assert_eq!(history["messages"].as_array().expect("history").len(), 2);
    assert_eq!(history["messages"][0]["ts"], posted["ts"]);
    assert_eq!(
        history["messages"][0]["thread_ts"],
        Value::String(parent.to_string())
    );

    let replies = call(
        &platform,
        "conversations.replies",
        serde_json::json!({ "channel": channel, "ts": parent.to_string() }),
    )
    .await;
    assert_eq!(replies["messages"].as_array().expect("replies").len(), 2);
    assert_eq!(replies["messages"][1]["ts"], posted["ts"]);
}

#[tokio::test]
async fn a_channel_message_queues_a_message_event_in_slacks_envelope() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("events").await;
    let user = platform.identify("ada").await;
    platform.say(&channel, &user, "plain traffic").await;

    let rows: Vec<String> = platform
        .state
        .database()
        .call(|connection| {
            let mut statement =
                connection.prepare("SELECT payload_json FROM event_outbox ORDER BY id")?;
            let rows = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .expect("read outbox");
    assert_eq!(rows.len(), 1, "a plain message queues exactly one event");
    let envelope: Value = serde_json::from_str(&rows[0]).expect("decode envelope");
    assert_eq!(
        keys(&envelope),
        [
            "api_app_id",
            "authorizations",
            "event",
            "event_id",
            "event_time",
            "team_id",
            "token",
            "type",
        ]
    );
    assert_eq!(envelope["type"], Value::String("event_callback".into()));
    assert!(
        envelope["event_id"]
            .as_str()
            .expect("event_id")
            .starts_with("Ev")
    );
    let event = &envelope["event"];
    assert_eq!(event["type"], Value::String("message".into()));
    assert_eq!(event["channel"], Value::String(channel));
    assert_eq!(event["user"], Value::String(user));
    assert_eq!(event["channel_type"], Value::String("channel".into()));
    assert_eq!(
        keys(&envelope["authorizations"][0]),
        ["is_bot", "is_enterprise_install", "team_id", "user_id"]
    );
}

#[tokio::test]
async fn a_mention_queues_both_a_message_and_an_app_mention_event() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let channel = platform.channel("mentions").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} are you there"))
        .await;

    let envelopes = platform.drain_envelopes().await;
    let mut kinds: Vec<&str> = envelopes.iter().map(super::support::event_kind).collect();
    kinds.sort();
    assert_eq!(
        kinds,
        ["app_mention", "message"],
        "Slack delivers both subscriptions, under two event ids"
    );
    let ids: std::collections::HashSet<&str> = envelopes
        .iter()
        .map(|envelope| envelope.event_id.as_str())
        .collect();
    assert_eq!(ids.len(), 2, "the two events have distinct event ids");
}
