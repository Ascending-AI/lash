//! Bot event-semantics tests: dedupe, ambient folding, session isolation, and
//! the standard-mode tool loop.

use crate::bot::channel::{Disposition, ReplySource};
use crate::bot::ledger::Stage;
use crate::bot::runtime::{session_id, thread_session_id};
use crate::bot::tools::{CHANNEL_HISTORY, LIST_CHANNELS};

use super::support::{Script, Step, TestPlatform, bot_dir, only_event, scratch, start_bot};

#[tokio::test]
async fn a_mention_runs_one_turn_and_posts_one_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("I am here.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("mention-turn").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} are you there?"))
        .await;

    let envelopes = platform.drain_envelopes().await;
    let app_mention = only_event(&envelopes, "app_mention");
    let disposition = bot
        .ingest(app_mention.clone(), None)
        .await
        .expect("handle app_mention");
    let Disposition::Replied {
        reply_ts, source, ..
    } = disposition
    else {
        panic!("expected a reply, got {disposition:?}");
    };
    assert_eq!(
        source,
        ReplySource::Turn,
        "the first delivery must run the model"
    );
    assert_eq!(script.calls(), 1);

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].text, "I am here.");
    assert_eq!(replies[0].ts, reply_ts);
    // The reply carries the originating event id, which is what makes recovery a
    // read rather than a guess.
    let metadata = replies[0].metadata.as_ref().expect("reply metadata");
    assert_eq!(metadata.event_type, "slack_clone_bot_reply");
    assert_eq!(
        metadata.event_payload["event_id"],
        serde_json::json!(app_mention.event_id)
    );

    // The bot's own mention token is stripped and the author is named, so the
    // model sees a line of conversation rather than Slack markup.
    assert!(
        script.saw("ada: are you there?"),
        "prompt should carry the resolved mention text: {:?}",
        script.requests()
    );
    assert!(
        !script.saw(&mention),
        "the bot's own mention token must not reach the model"
    );
}

#[tokio::test]
async fn the_same_event_id_delivered_twice_runs_one_turn_and_posts_one_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Once only.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("dedupe").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hello"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let first = bot
        .ingest(app_mention.clone(), None)
        .await
        .expect("first delivery");
    // Slack's retries carry the same event id and an `x-slack-retry-num` header.
    let second = bot
        .ingest(app_mention.clone(), Some(1))
        .await
        .expect("redelivery");
    let third = bot
        .ingest(app_mention.clone(), Some(2))
        .await
        .expect("second redelivery");

    assert!(matches!(
        first,
        Disposition::Replied {
            source: ReplySource::Turn,
            ..
        }
    ));
    for redelivery in [&second, &third] {
        let Disposition::Duplicate { stage, .. } = redelivery else {
            panic!("a redelivery must not act: {redelivery:?}");
        };
        assert_eq!(*stage, Stage::Replied);
    }
    assert_eq!(script.calls(), 1, "exactly one turn for three deliveries");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);

    let record = bot
        .ledger()
        .get(app_mention.event_id.clone())
        .await
        .expect("read ledger")
        .expect("ledger row");
    assert_eq!(record.deliveries, 3, "every delivery is counted");
    assert_eq!(record.stage, Stage::Replied);
}

#[tokio::test]
async fn ambient_traffic_folds_into_the_session_without_a_turn_or_a_reply() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Caught up.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("ambient").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;
    platform.say(&channel, &ada, "the deploy is stuck").await;
    platform.say(&channel, &grace, "rolling it back now").await;

    let mut folded = Vec::new();
    for envelope in platform.drain_envelopes().await {
        folded.push(bot.ingest(envelope, None).await.expect("handle message"));
    }
    assert_eq!(folded.len(), 2);
    for disposition in &folded {
        assert!(
            matches!(disposition, Disposition::Folded { .. }),
            "ambient traffic folds: {disposition:?}"
        );
    }
    assert_eq!(script.calls(), 0, "no turn runs for ambient traffic");
    assert!(
        platform.bot_messages(&channel).await.is_empty(),
        "and no reply is posted"
    );

    // Lash's own evidence: two durable admissions waiting for the next turn.
    let session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    let pending = session
        .pending_turn_inputs()
        .await
        .expect("read pending turn inputs");
    assert_eq!(
        pending.len(),
        2,
        "both ambient lines are queued: {pending:?}"
    );

    // Now a mention: one drain folds the room context and the mention into one turn.
    let mention = platform.mention();
    platform
        .say(&channel, &ada, &format!("{mention} what happened?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    assert!(matches!(disposition, Disposition::Replied { .. }));
    assert_eq!(script.calls(), 1, "one turn, not one per queued line");
    assert!(
        session
            .pending_turn_inputs()
            .await
            .expect("read pending turn inputs")
            .is_empty(),
        "the drain consumed every queued input"
    );
    assert!(
        script.saw("the deploy is stuck") && script.saw("rolling it back now"),
        "the ambient context must reach the prompt: {:?}",
        script.requests()
    );
}

#[tokio::test]
async fn the_bot_ignores_app_authored_messages_and_its_own_replies() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Hello back.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("self-guard").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hi"))
        .await;

    // The mention arrives as *two* events. Handling the `message` twin as well
    // must not produce a second turn.
    let envelopes = platform.drain_envelopes().await;
    let mut dispositions = Vec::new();
    for envelope in envelopes {
        dispositions.push(bot.ingest(envelope, None).await.expect("handle event"));
    }
    let ignored = dispositions
        .iter()
        .filter(|disposition| {
            matches!(
                disposition,
                Disposition::Ignored {
                    reason: "superseded_by_app_mention",
                    ..
                }
            )
        })
        .count();
    assert_eq!(ignored, 1, "the message twin of a mention is dropped");
    assert_eq!(script.calls(), 1, "one turn for one mention");

    // The bot's own reply generated an event too. Feeding it back must be inert —
    // otherwise the bot answers itself forever.
    let after_reply = platform.drain_envelopes().await;
    assert!(!after_reply.is_empty(), "the reply produced an event");
    for envelope in after_reply {
        let disposition = bot.ingest(envelope, None).await.expect("handle own reply");
        assert!(
            matches!(
                disposition,
                Disposition::Ignored {
                    reason: "app_authored_message",
                    ..
                }
            ),
            "app-authored messages are inert: {disposition:?}"
        );
    }
    assert_eq!(script.calls(), 1, "still one turn");
    assert_eq!(platform.bot_messages(&channel).await.len(), 1);
}

#[tokio::test]
async fn each_channel_gets_its_own_session_and_neither_sees_the_others_context() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Noted.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let secrets = platform.channel("secrets").await;
    let public = platform.channel("public").await;
    let ada = platform.identify("ada").await;
    platform
        .say(&secrets, &ada, "the passphrase is hunter2")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold secret");
    }

    let mention = platform.mention();
    platform
        .say(&public, &ada, &format!("{mention} anything to share?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(app_mention, None).await.expect("handle mention");

    assert!(
        !script.saw("hunter2"),
        "#secrets context must not leak into #public's session: {:?}",
        script.requests()
    );

    let secret_session = bot
        .core()
        .session(session_id(&secrets))
        .open()
        .await
        .expect("open secrets session");
    assert_eq!(
        secret_session
            .pending_turn_inputs()
            .await
            .expect("pending inputs")
            .len(),
        1,
        "the other channel's queued context is untouched"
    );
    let public_session = bot
        .core()
        .session(session_id(&public))
        .open()
        .await
        .expect("open public session");
    assert!(
        public_session
            .pending_turn_inputs()
            .await
            .expect("pending inputs")
            .is_empty()
    );
    assert_ne!(secret_session.session_id(), public_session.session_id());
}

#[tokio::test]
async fn a_thread_forks_on_its_first_reply_and_inherits_uncommitted_root_context() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("The deploy target was EU west.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("thread-fork").await;
    let ada = platform.identify("ada").await;
    let grace = platform.identify("grace").await;
    let root = platform
        .say(&channel, &ada, "the deploy target is EU west")
        .await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold thread root");
    }

    let channel_session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    let channel_head_before = channel_session
        .read_view()
        .session_graph()
        .leaf_node_id
        .clone();

    let mention = platform.mention();
    platform
        .say_thread(
            &channel,
            &grace,
            root,
            &format!("{mention} where are we deploying?"),
        )
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot
        .ingest(app_mention, None)
        .await
        .expect("handle first thread engagement");
    assert!(matches!(disposition, Disposition::Replied { .. }));

    let thread_id = thread_session_id(&channel, &root.to_string());
    assert!(
        bot.core()
            .fork_points()
            .await
            .expect("list fork points")
            .iter()
            .any(|point| point.source_session_id == thread_id),
        "the deterministic child session must exist"
    );
    assert_eq!(
        channel_session.read_view().session_graph().leaf_node_id,
        channel_head_before,
        "forking and running the thread must not advance the channel head"
    );
    assert!(
        script.saw("the deploy target is EU west"),
        "the queued root is copied because it was not yet in the forked graph: {:?}",
        script.requests()
    );
    assert_eq!(platform.thread_messages(&channel, root).await.len(), 2);
    assert!(
        platform.bot_messages(&channel).await.is_empty(),
        "the bot reply is in the thread, not channel history"
    );
}

#[tokio::test]
async fn a_thread_fork_shares_a_committed_channel_turn_by_provenance() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Text("Channel answer.".to_string()),
        Step::Text("Thread answer.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-committed-root").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "retained channel fact").await;
    let root_event = only_event(&platform.drain_envelopes().await, "message");
    bot.ingest(root_event.clone(), None)
        .await
        .expect("fold root");

    platform
        .say(
            &channel,
            &ada,
            &format!("{} commit the channel turn", platform.mention()),
        )
        .await;
    let channel_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(channel_mention, None)
        .await
        .expect("commit channel turn");
    let root_record = bot
        .ledger()
        .get(root_event.event_id)
        .await
        .expect("read ledger")
        .expect("root row");
    assert!(
        root_record.input_id.is_some() && root_record.fork_node_id.is_some(),
        "typed application correlation records the retained boundary: {root_record:?}"
    );

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} what was retained?", platform.mention()),
        )
        .await;
    let thread_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(thread_mention, None)
        .await
        .expect("run forked thread turn");
    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].contains("retained channel fact"),
        "the child shares committed ancestry: {}",
        requests[1]
    );
}

#[tokio::test]
async fn thread_and_channel_traffic_are_isolated_after_the_fork() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::new([
        Step::Text("Thread answer one.".to_string()),
        Step::Text("Thread answer two.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-isolation").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "shared before fork").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }

    let mention = platform.mention();
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{mention} first thread turn"),
        )
        .await;
    let first = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(first, None).await.expect("first thread turn");

    platform
        .say(&channel, &ada, "channel only after fork")
        .await;
    for envelope in platform.drain_envelopes().await {
        if envelope.event.thread_ts().is_none() {
            bot.ingest(envelope, None)
                .await
                .expect("fold channel-only traffic");
        }
    }
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{mention} second thread turn"),
        )
        .await;
    let second = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(second, None).await.expect("second thread turn");

    let requests = script.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("first thread turn"));
    assert!(
        !requests[1].contains("channel only after fork"),
        "post-fork channel traffic cannot appear in the child prompt: {}",
        requests[1]
    );

    let channel_text = channel_session_text(&bot, &channel).await;
    assert!(channel_text.contains("shared before fork") || channel_text.is_empty());
    assert!(!channel_text.contains("first thread turn"));
    assert!(!channel_text.contains("second thread turn"));
    let channel_session = bot
        .core()
        .session(session_id(&channel))
        .open()
        .await
        .expect("open channel session");
    assert_eq!(
        channel_session
            .pending_turn_inputs()
            .await
            .expect("channel pending inputs")
            .len(),
        2,
        "the root and post-fork channel traffic remain queued only on the channel session"
    );
}

#[tokio::test]
async fn a_thread_event_is_deduplicated_in_the_shared_ledger() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("Once in thread.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-dedupe").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "root").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }
    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} answer once", platform.mention()),
        )
        .await;
    let event = only_event(&platform.drain_envelopes().await, "app_mention");
    assert!(matches!(
        bot.ingest(event.clone(), None)
            .await
            .expect("first delivery"),
        Disposition::Replied { .. }
    ));
    assert!(matches!(
        bot.ingest(event, Some(1)).await.expect("redelivery"),
        Disposition::Duplicate {
            stage: Stage::Replied,
            ..
        }
    ));
    assert_eq!(script.calls(), 1);
    assert_eq!(platform.thread_messages(&channel, root).await.len(), 2);
}

#[tokio::test]
async fn an_ambient_thread_reply_creates_the_fork_and_waits_for_a_mention() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("I saw the thread context.");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;
    let channel = platform.channel("thread-ambient").await;
    let ada = platform.identify("ada").await;
    let root = platform.say(&channel, &ada, "channel root fact").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold root");
    }

    platform
        .say_thread(&channel, &ada, root, "thread detail before engagement")
        .await;
    let ambient = only_event(&platform.drain_envelopes().await, "message");
    assert!(matches!(
        bot.ingest(ambient, None)
            .await
            .expect("fold thread ambient"),
        Disposition::Folded { .. }
    ));
    assert_eq!(script.calls(), 0, "ambient thread traffic spends no token");
    let thread = bot
        .core()
        .session(thread_session_id(&channel, &root.to_string()))
        .open()
        .await
        .expect("open fork created by first reply");
    assert_eq!(
        thread
            .pending_turn_inputs()
            .await
            .expect("thread pending inputs")
            .len(),
        2,
        "the inherited root and ambient thread reply wait in the child"
    );

    platform
        .say_thread(
            &channel,
            &ada,
            root,
            &format!("{} now answer", platform.mention()),
        )
        .await;
    let mention = only_event(&platform.drain_envelopes().await, "app_mention");
    bot.ingest(mention, None)
        .await
        .expect("drain thread context");
    assert!(script.saw("channel root fact"));
    assert!(script.saw("thread detail before engagement"));
}

async fn channel_session_text(bot: &crate::bot::channel::ChannelBot, channel: &str) -> String {
    let session = bot
        .core()
        .session(session_id(channel))
        .open()
        .await
        .expect("open channel session");
    session
        .read_view()
        .chronological_projection()
        .into_entries()
        .into_iter()
        .filter_map(|entry| match entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                Some(lash::message_text(&message))
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_mention_can_drive_the_standard_tool_loop() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    // Two native tool calls, then prose: the standard-mode loop, twice around.
    let script = Script::new([
        Step::ToolCall {
            name: LIST_CHANNELS.to_string(),
            args: serde_json::json!({}),
        },
        Step::ToolCall {
            name: CHANNEL_HISTORY.to_string(),
            args: serde_json::json!({ "channel": "tooling", "limit": 5 }),
        },
        Step::Text("There are channels, and #tooling has traffic.".to_string()),
    ]);
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("tooling").await;
    let user = platform.identify("ada").await;
    platform.say(&channel, &user, "background chatter").await;
    for envelope in platform.drain_envelopes().await {
        bot.ingest(envelope, None).await.expect("fold chatter");
    }

    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} what channels exist?"))
        .await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    let Disposition::Replied { .. } = disposition else {
        panic!("expected a reply, got {disposition:?}");
    };
    assert_eq!(
        script.calls(),
        3,
        "two tool calls plus the final answer is three provider calls"
    );

    let replies = platform.bot_messages(&channel).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].text,
        "There are channels, and #tooling has traffic."
    );
    // The tools really called the platform: the channel the test created and the
    // message it posted both came back through `conversations.*`.
    let requests = script.requests().join("\n");
    assert!(
        requests.contains("tooling"),
        "list_channels output should name the channel"
    );
    assert!(
        requests.contains("background chatter"),
        "channel_history output should carry the posted message"
    );
}

#[tokio::test]
async fn an_envelope_with_the_wrong_verification_token_is_rejected() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("never");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("spoofed").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform
        .say(&channel, &user, &format!("{mention} hi"))
        .await;
    let mut app_mention = only_event(&platform.drain_envelopes().await, "app_mention");
    app_mention.token = "not-the-verification-token".to_string();

    let disposition = bot.ingest(app_mention, None).await.expect("handle spoof");
    assert_eq!(
        disposition,
        Disposition::Rejected {
            reason: "bad_verification_token"
        }
    );
    assert_eq!(script.calls(), 0);
    assert!(platform.bot_messages(&channel).await.is_empty());
}

#[tokio::test]
async fn an_empty_model_answer_is_absorbed_rather_than_posted() {
    let scratch = scratch();
    let platform = TestPlatform::start(scratch.path()).await;
    let script = Script::prose("   ");
    let bot = start_bot(&platform, &bot_dir(scratch.path()), &script).await;

    let channel = platform.channel("quiet").await;
    let user = platform.identify("ada").await;
    let mention = platform.mention();
    platform.say(&channel, &user, &format!("{mention} ?")).await;
    let app_mention = only_event(&platform.drain_envelopes().await, "app_mention");

    let disposition = bot.ingest(app_mention, None).await.expect("handle mention");
    assert!(
        matches!(
            disposition,
            Disposition::Silent {
                reason: "empty_model_reply",
                ..
            }
        ),
        "an empty answer must not become an empty message: {disposition:?}"
    );
    assert!(platform.bot_messages(&channel).await.is_empty());
}
